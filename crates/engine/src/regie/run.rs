//! `regie.run`: from a sentence to a film, as a background job (see [the module overview](super)).
//!
//! The job thread does the slow part: it searches the folders, reads capture dates, finds each
//! video's best moments and the music's beats. When it is done, [`poll`] (UI thread) imports the
//! chosen media into a new bin and builds the film, all in one undo step. `jobs.list` shows the
//! progress, `jobs.cancel` stops it without changing the project; `regie.status` reports it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_project::{ItemId, ItemKind, MediaRef};
use serde_json::{Value, json};

use crate::commands::{bad, bool_p, str_p};

use super::types::Highlights;
use crate::{EngineError, Result, Session};

use super::assemble::{Medium, Music};
use super::types::{Beats, Candidate, Plan};

/// The most videos analysed for one film (the rest are left out, spread over the time span).
const MAX_VIDEOS: usize = 240;
/// The most photos considered for one film.
const MAX_PHOTOS: usize = 600;

/// What the job thread found.
pub struct Analysis {
    pub plan: Plan,
    pub media: Vec<Medium>,
    /// Music file and its beats.
    pub music: Option<(String, Option<Beats>)>,
    pub scanned: u64,
    /// Photos and videos in formats FilmCraft cannot import (camera raw, …) and unreadable files.
    pub skipped: u64,
    /// Screenshots and screen recordings left out.
    pub screenshots: u64,
}

/// A running `regie.run` job.
pub struct Pending {
    pub job: u64,
    pub result: Arc<Mutex<Option<std::result::Result<Analysis, String>>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `regie.run`: start the job.
pub(crate) fn start(s: &mut Session, p: &Value) -> Result<Value> {
    if s.regie.pending.is_some() {
        return Err(EngineError::Other("the AI Director is already making a film".into()));
    }
    let mut plan: Plan = match (p.get("plan").filter(|v| !v.is_null()), str_p(p, "request")) {
        (Some(v), _) => serde_json::from_value(v.clone()).map_err(|e| bad("regie.run", format!("`plan`: {e}")))?,
        (None, Some(r)) if !r.trim().is_empty() => super::plan::understand(r, super::plan::system_today()),
        _ => return Err(bad("regie.run", "give a `request` (text) or a `plan`")),
    };
    if let Some(f) = p.get("folders").and_then(Value::as_array) {
        let folders: Vec<String> = f.iter().filter_map(Value::as_str).map(str::to_string).filter(|x| !x.trim().is_empty()).collect();
        if !folders.is_empty() {
            plan.folders = folders;
        }
    }
    let de = plan.language == "de";
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let label = if de { "KI-Regie: Film wird vorbereitet".to_string() } else { "AI Director: preparing the film".to_string() };
    let job = crate::Job { id, label, progress: Default::default(), result: Default::default() };
    job.progress.total.store(100, Ordering::Relaxed);
    let slot: Arc<Mutex<Option<std::result::Result<Analysis, String>>>> = Arc::default();
    let (prog, res, out) = (job.progress.clone(), job.result.clone(), slot.clone());
    let media = s.media.clone();
    let services = s.services.clone();
    let backgrounds = crate::autosave::default_data_dir().map(|d| d.join("regie-backgrounds"));
    let run = move || {
        let t0 = web_time::Instant::now();
        let status = |text: String| *lock(&prog.status) = text;
        let r = analyse(&plan, &media, &*services, backgrounds.as_deref(), &prog.cancel, &|done, total, text| {
            prog.total.store(total.max(1), Ordering::Relaxed);
            prog.done.store(done.min(total), Ordering::Relaxed);
            status(text);
        });
        let secs = t0.elapsed().as_secs_f64();
        let report = match &r {
            Ok(a) => {
                status(if de {
                    format!("{} Medien gefunden, analysiert in {secs:.0} s – der Film wird gebaut …", a.media.len())
                } else {
                    format!("Found {} media, analysed in {secs:.0} s – building the film…", a.media.len())
                });
                Ok(filmcraft_export::Report {
                    path: String::new(),
                    frames: a.media.len() as u64,
                    seconds: secs,
                    bytes: 0,
                    render_fps: 0.0,
                    extra_files: Vec::new(),
                    gpu: None,
                })
            }
            Err(e) => {
                status(e.clone());
                Err(e.clone())
            }
        };
        *lock(&out) = Some(r);
        *lock(&res) = Some(report);
        prog.finished.store(true, Ordering::Relaxed);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    s.jobs.push(job);
    s.regie.pending = Some(Pending { job: id, result: slot });
    s.regie.last = None;
    let wait = bool_p(p, "wait").unwrap_or(false);
    if wait || cfg!(target_arch = "wasm32") {
        run();
        poll(s);
        return match &s.regie.last {
            Some(Ok(v)) => Ok(json!({"job": id, "result": v})),
            Some(Err(e)) => Err(EngineError::Other(e.clone())),
            None => Ok(json!({"job": id})),
        };
    }
    std::thread::Builder::new().name("filmcraft-regie".into()).spawn(run).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({"job": id}))
}

/// The slow part, off the UI thread: search, read capture data, analyse.
fn analyse(
    plan: &Plan,
    pool: &crate::media_pool::MediaPool,
    services: &dyn crate::Services,
    backgrounds: Option<&std::path::Path>,
    cancel: &AtomicBool,
    progress: &dyn Fn(u64, u64, String),
) -> std::result::Result<Analysis, String> {
    let de = plan.language == "de";
    progress(0, 100, if de { "Suche Medien in deinen Ordnern …".into() } else { "Looking for media in your folders…".into() });
    let found = search(plan, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(stopped(de));
    }
    let (scanned, skipped, screenshots) = (found.scanned, found.skipped, found.screenshots);
    let mut videos: Vec<Candidate> = found.candidates.iter().filter(|c| c.kind == "video" && plan.videos).cloned().collect();
    let mut photos: Vec<Candidate> = found.candidates.iter().filter(|c| c.kind == "photo" && plan.photos).cloned().collect();
    let audio: Vec<Candidate> = found.candidates.iter().filter(|c| c.kind == "audio").cloned().collect();
    spread(&mut videos, MAX_VIDEOS);
    spread(&mut photos, MAX_PHOTOS);
    if videos.is_empty() && photos.is_empty() {
        let shots = screenshots_left_out(screenshots, de).map(|t| format!(" {t}")).unwrap_or_default();
        return Err(if de {
            format!("In den Ordnern ({}) habe ich keine passenden Fotos oder Videos gefunden.{shots}", found.folders.join(", "))
        } else {
            format!("No matching photos or videos in the folders ({}).{shots}", found.folders.join(", "))
        });
    }
    // the music: the file the plan names, else the longest audio file of at least a minute
    let music_path = match plan.music.as_deref() {
        Some("") => None,
        Some(path) => Some(path.to_string()),
        None => audio
            .iter()
            .filter(|a| a.seconds.is_none_or(|s| s >= 45.0))
            .max_by(|a, b| a.seconds.unwrap_or(0.0).total_cmp(&b.seconds.unwrap_or(0.0)))
            .map(|a| a.path.clone()),
    };
    let total = videos.len() as u64 + 3;
    // Every photo gets a small blurred background: the film puts it behind a photo of another
    // shape (a portrait photo in a landscape film), and another version of the film
    // (`regie.variant`: a 9:16 reel of landscape photos) needs it for others. Blurring the
    // full-size photo on every frame instead is slow and holds gigabytes of GPU memory.
    progress(0, total, if de { "Bereite die Fotos vor …".into() } else { "Preparing the photos…".into() });
    let backgrounds: Vec<Option<String>> = match backgrounds {
        Some(dir) => {
            let one = |c: &Candidate| if cancel.load(Ordering::Relaxed) { None } else { blurred_background(pool, services, &c.path, dir) };
            // a few at a time: each decodes a whole photo (up to ~200 MB for 48 megapixels)
            match rayon::ThreadPoolBuilder::new().num_threads(4).build() {
                Ok(workers) => workers.install(|| {
                    use rayon::prelude::*;
                    photos.par_iter().map(one).collect()
                }),
                Err(_) => photos.iter().map(one).collect(),
            }
        }
        None => vec![None; photos.len()],
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(stopped(de));
    }
    let mut media: Vec<Medium> =
        photos.into_iter().zip(backgrounds).map(|(c, background)| Medium { candidate: c, highlights: None, background, background_item: None }).collect();
    for (k, c) in videos.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(stopped(de));
        }
        let name = std::path::Path::new(&c.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        progress(
            k as u64 + 2,
            total,
            if de {
                format!("Suche die besten Momente: {name} ({} von {})", k + 1, total - 3)
            } else {
                format!("Finding the best moments: {name} ({} of {})", k + 1, total - 3)
            },
        );
        let highlights: Option<Highlights> = pool.open_file(&c.path, services).ok().and_then(|src| best_moments(&src, cancel));
        media.push(Medium { candidate: c, highlights, background: None, background_item: None });
    }
    let music = match music_path {
        Some(path) => {
            progress(
                total - 1,
                total,
                if de { "Höre mir die Musik an (Takt und Stimmung) …".into() } else { "Listening to the music (beat and energy)…".into() },
            );
            let beats = pool.open_file(&path, services).ok().and_then(|src| beats_of_source(&src));
            Some((path, beats))
        }
        None => None,
    };
    progress(total, total, String::new());
    Ok(Analysis { plan: plan.clone(), media, music, scanned, skipped, screenshots })
}

/// Longest side of a photo's blurred background, pixels: it is blurred and scaled up to fill
/// the frame, so more would only cost memory.
const BACKGROUND_SIDE: usize = 480;

/// A small ([`BACKGROUND_SIDE`]), blurred and darkened copy of the photo at `path`, written as PNG
/// into `dir` (named after the photo, so a second film reuses it). `None` when the photo cannot
/// be read.
fn blurred_background(pool: &crate::media_pool::MediaPool, services: &dyn crate::Services, path: &str, dir: &std::path::Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    std::fs::metadata(path).ok()?.len().hash(&mut hasher);
    // (v2: small; the first ones kept the photo's full size)
    ("v2", BACKGROUND_SIDE).hash(&mut hasher);
    let out = dir.join(format!("{:016x}.png", hasher.finish()));
    if out.is_file() {
        return Some(out.to_string_lossy().into_owned());
    }
    let src = pool.open_file(path, services).ok()?;
    let v = src.info().video.clone()?;
    let scale = (BACKGROUND_SIDE as f32 / v.width.max(v.height).max(1) as f32).clamp(0.01, 1.0);
    let frame = src.video_frame(filmcraft_media::FrameRequest { time: filmcraft_time::Tick::ZERO, scale }).ok()?;
    let (w, h) = (frame.width as usize, frame.height as usize);
    let px = frame.to_rgba8();
    if w < 2 || h < 2 || px.len() != w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    // stills come at full size whatever the request asks
    let (mut px, w, h) = shrink(&px, w, h, BACKGROUND_SIDE)?;
    let radius = (w.max(h) / 30).max(2);
    for _ in 0..3 {
        box_blur(&mut px, w, h, radius);
    }
    for p in px.as_chunks_mut::<4>().0 {
        for c in p.iter_mut().take(3) {
            *c = (f32::from(*c) * 0.55) as u8;
        }
    }
    let png = filmcraft_export::encode_png(px, w as u32, h as u32).ok()?;
    std::fs::create_dir_all(dir).ok()?;
    std::fs::write(&out, png).ok()?;
    Some(out.to_string_lossy().into_owned())
}

/// `px` (straight RGBA8, `w` × `h`) scaled down by a whole factor so its longer side is at most
/// `side`, each pixel the average of the block it covers (blocks at the right and bottom edges
/// are smaller). Unchanged when it already fits; `None` for a buffer that does not match.
pub(crate) fn shrink(px: &[u8], w: usize, h: usize, side: usize) -> Option<(Vec<u8>, usize, usize)> {
    if px.len() != w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    let f = w.max(h).div_ceil(side.max(1)).max(1);
    if f == 1 {
        return Some((px.to_vec(), w, h));
    }
    let (ow, oh) = (w.div_ceil(f), h.div_ceil(f));
    let mut out = vec![0u8; ow.checked_mul(oh)?.checked_mul(4)?];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut sum = [0u32; 4];
            let mut n = 0u32;
            for y in oy * f..((oy + 1) * f).min(h) {
                let row = px.get((y * w + ox * f) * 4..(y * w + ((ox + 1) * f).min(w)) * 4)?;
                for p in row.as_chunks::<4>().0 {
                    for (s, c) in sum.iter_mut().zip(p) {
                        *s += u32::from(*c);
                    }
                    n += 1;
                }
            }
            let o = out.get_mut((oy * ow + ox) * 4..(oy * ow + ox) * 4 + 4)?;
            for (o, s) in o.iter_mut().zip(sum) {
                *o = (s / n.max(1)) as u8;
            }
        }
    }
    Some((out, ow, oh))
}

/// One horizontal and one vertical box blur of radius `r` over straight RGBA8 pixels (edges
/// repeat). Three passes approximate a Gaussian.
fn box_blur(px: &mut [u8], w: usize, h: usize, r: usize) {
    let mut tmp = vec![0u8; px.len()];
    box_pass(px, &mut tmp, w, h, r, true);
    box_pass(&tmp, px, w, h, r, false);
}

fn box_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horizontal: bool) {
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    if inner == 0 {
        return;
    }
    let idx = |o: usize, i: usize| if horizontal { (o * w + i) * 4 } else { (i * w + o) * 4 };
    let span = (2 * r + 1) as u32;
    let last = inner as isize - 1;
    let r = r as isize;
    for o in 0..outer {
        for ch in 0..4 {
            let at = |i: isize| -> u32 { src.get(idx(o, i.clamp(0, last) as usize) + ch).copied().map(u32::from).unwrap_or(0) };
            let mut acc: u32 = (-r..=r).map(at).sum();
            for i in 0..inner {
                if let Some(d) = dst.get_mut(idx(o, i) + ch) {
                    *d = (acc / span) as u8;
                }
                let i = i as isize;
                acc = (acc + at(i + r + 1)).saturating_sub(at(i - r));
            }
        }
    }
}

/// The best moments of a video (as `media.highlights`, with its default settings).
fn best_moments(src: &filmcraft_media::SharedSource, cancel: &AtomicBool) -> Option<Highlights> {
    let opts = super::analysis::HighlightOptions::default();
    match super::analysis::highlights(src.as_ref(), &opts, cancel, &|_, _| {}) {
        Ok(h) => Some(h),
        Err(e) => {
            log::info!("AI Director: no highlights: {e}");
            None
        }
    }
}

/// The beat grid of a music file (as `audio.beats`).
fn beats_of_source(src: &filmcraft_media::SharedSource) -> Option<Beats> {
    let info = src.info();
    if !info.has_audio() {
        return None;
    }
    let range = filmcraft_time::TimeRange::new(filmcraft_time::Tick::ZERO, info.duration);
    let rate = super::analysis::READ_RATE;
    let mono = super::analysis::read_mono(src.as_ref(), range, rate).ok()?;
    Some(super::analysis::beats_from_mono(&mono, rate))
}

fn stopped(de: bool) -> String {
    if de { "Abgebrochen – nichts wurde verändert.".into() } else { "Stopped – nothing was changed.".into() }
}

/// Keep at most `max` candidates, spread evenly over the list (which is in time order).
fn spread(list: &mut Vec<Candidate>, max: usize) {
    if list.len() <= max || max == 0 {
        return;
    }
    let n = list.len();
    let keep: Vec<Candidate> = (0..max).filter_map(|k| list.get(k * n / max).cloned()).collect();
    *list = keep;
}

/// The project item that shows the file at `path`, if any.
fn item_for_path(s: &Session, path: &str) -> Option<ItemId> {
    s.project.items.iter().find_map(|(id, it)| match &it.kind {
        ItemKind::Media(m) if matches!(&m.media, MediaRef::File { path: p } if p == path) => Some(*id),
        _ => None,
    })
}

/// Apply a finished job: import the media into a new bin and build the film (one undo step).
/// Called once per UI frame from [`Session::poll_persistence`] and after synchronous runs.
pub fn poll(s: &mut Session) {
    let Some(pending) = s.regie.pending.as_ref() else { return };
    let job = s.jobs.iter().find(|j| j.id == pending.job);
    let finished = job.is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
    if !finished {
        return;
    }
    let cancelled = job.is_some_and(|j| j.progress.cancel.load(Ordering::Relaxed));
    let Some(pending) = s.regie.pending.take() else { return };
    let taken = lock(&pending.result).take();
    let analysis = match taken {
        _ if cancelled => {
            s.regie.last = Some(Err("stopped".into()));
            return;
        }
        Some(Ok(a)) => a,
        Some(Err(e)) => {
            s.regie.last = Some(Err(e));
            return;
        }
        None => {
            s.regie.last = Some(Err("the AI Director job ended without a result".into()));
            return;
        }
    };
    let r = finish(s, analysis);
    if let Err(e) = &r {
        s.error_toast("regie.run", e.to_string());
    }
    s.regie.last = Some(r.map_err(|e| e.to_string()));
}

fn finish(s: &mut Session, a: Analysis) -> Result<Value> {
    let de = a.plan.language == "de";
    let mut paths: Vec<String> = a.media.iter().map(|m| m.candidate.path.clone()).collect();
    paths.extend(a.media.iter().filter_map(|m| m.background.clone()));
    if let Some((m, _)) = &a.music {
        paths.push(m.clone());
    }
    let label = if de { "KI-Regie: Film" } else { "AI Director: film" };
    let plan = a.plan.clone();
    s.edit_group(label, move |s| {
        let bin_name = plan.title.clone().unwrap_or_else(|| if de { "KI-Regie".into() } else { "AI Director".into() });
        let bin = s.execute("file.newBin", json!({"name": bin_name})).ok().and_then(|v| v.get("bin").and_then(Value::as_u64));
        // import in chunks so one unreadable file does not stop the rest
        for chunk in paths.chunks(50) {
            let mut p = json!({"paths": chunk});
            if let Some(b) = bin {
                p["bin"] = json!(b);
            }
            if let Err(e) = s.execute("file.import", p) {
                log::warn!("AI Director: import: {e}");
            }
        }
        let mut media = a.media.clone();
        for m in &mut media {
            m.candidate.item = item_for_path(s, &m.candidate.path).map(|i| i.0);
            m.background_item = m.background.as_deref().and_then(|b| item_for_path(s, b)).map(|i| i.0);
        }
        media.retain(|m| m.candidate.item.is_some());
        let music = a.music.as_ref().and_then(|(path, beats)| item_for_path(s, path).map(|i| Music { item: i.0, beats: beats.clone() }));
        let mut v = super::assemble::assemble_in(s, &plan, &media, music.as_ref())?;
        s.regie.material = Some(super::assemble::Material::of(&plan, &media, music.as_ref(), &v));
        v["scanned"] = json!(a.scanned);
        v["skipped"] = json!(a.skipped);
        v["screenshots"] = json!(a.screenshots);
        if let Some(sum) = v.get_mut("summary").and_then(Value::as_array_mut) {
            if a.skipped > 0 {
                sum.push(json!(if de {
                    format!("{} Dateien konnte ich nicht verwenden (z. B. RAW-Fotos der Kamera außer DNG, oder WMV-Videos).", a.skipped)
                } else {
                    format!("{} files could not be used (for example camera raw photos other than DNG, or WMV videos).", a.skipped)
                }));
            }
            if let Some(t) = screenshots_left_out(a.screenshots, de) {
                sum.push(json!(t));
            }
        }
        Ok(v)
    })
}

/// The summary line for screenshots and screen recordings the search left out (none for 0).
fn screenshots_left_out(n: u64, de: bool) -> Option<String> {
    (n > 0).then(|| match (de, n) {
        (true, 1) => "Ein Bildschirmfoto (oder eine Bildschirmaufnahme) habe ich weggelassen.".to_string(),
        (true, _) => format!("{n} Bildschirmfotos und Bildschirmaufnahmen habe ich weggelassen."),
        (false, 1) => "I left out one screenshot or screen recording.".to_string(),
        (false, _) => format!("I left out {n} screenshots and screen recordings."),
    })
}

/// What a search found.
pub struct Found {
    pub candidates: Vec<Candidate>,
    pub scanned: u64,
    pub skipped: u64,
    /// Screenshots and screen recordings left out.
    pub screenshots: u64,
    pub folders: Vec<String>,
}

/// Search the plan's folders (the default picture, video and music folders when it names none):
/// photos and videos through the plan's dates and places, the music by itself.
fn search(plan: &Plan, cancel: &AtomicBool) -> std::result::Result<Found, String> {
    let mut folders: Vec<std::path::PathBuf> = plan.folders.iter().map(|f| expand_home(f)).collect();
    if folders.is_empty() {
        folders = super::library::default_folders();
    }
    if folders.is_empty() {
        return Err(if plan.language == "de" {
            "Ich weiß nicht, wo ich suchen soll: bitte einen Ordner angeben.".into()
        } else {
            "I don't know where to look: please add a folder.".into()
        });
    }
    let pictures = super::library::find_media(&folders, &super::library::MediaFilter::from_plan(plan), cancel);
    let music_filter = super::library::MediaFilter { kinds: vec!["audio".to_string()], ..Default::default() };
    let music = super::library::find_media(&folders, &music_filter, cancel);
    let mut candidates = pictures.candidates;
    candidates.extend(music.candidates);
    Ok(Found {
        candidates,
        scanned: pictures.scanned,
        skipped: pictures.skipped,
        screenshots: pictures.screenshots,
        folders: folders.iter().map(|f| f.display().to_string()).collect(),
    })
}

fn expand_home(f: &str) -> std::path::PathBuf {
    match (f.strip_prefix("~/"), crate::media_browser::std_home_dir()) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(f),
    }
}
