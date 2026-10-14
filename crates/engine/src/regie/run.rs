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
    let run = move || {
        let t0 = web_time::Instant::now();
        let status = |text: String| *lock(&prog.status) = text;
        let r = analyse(&plan, &media, &*services, &prog.cancel, &|done, total, text| {
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
    cancel: &AtomicBool,
    progress: &dyn Fn(u64, u64, String),
) -> std::result::Result<Analysis, String> {
    let de = plan.language == "de";
    progress(0, 100, if de { "Suche Medien in deinen Ordnern …".into() } else { "Looking for media in your folders…".into() });
    let found = search(plan, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(stopped(de));
    }
    let scanned = found.scanned;
    let mut videos: Vec<Candidate> = found.candidates.iter().filter(|c| c.kind == "video" && plan.videos).cloned().collect();
    let mut photos: Vec<Candidate> = found.candidates.iter().filter(|c| c.kind == "photo" && plan.photos).cloned().collect();
    let audio: Vec<Candidate> = found.candidates.iter().filter(|c| c.kind == "audio").cloned().collect();
    spread(&mut videos, MAX_VIDEOS);
    spread(&mut photos, MAX_PHOTOS);
    if videos.is_empty() && photos.is_empty() {
        return Err(if de {
            format!("In den Ordnern ({}) habe ich keine passenden Fotos oder Videos gefunden.", found.folders.join(", "))
        } else {
            format!("No matching photos or videos in the folders ({}).", found.folders.join(", "))
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
    let total = videos.len() as u64 + 2;
    let mut media: Vec<Medium> = photos.into_iter().map(|c| Medium { candidate: c, highlights: None }).collect();
    for (k, c) in videos.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(stopped(de));
        }
        let name = std::path::Path::new(&c.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        progress(
            k as u64 + 1,
            total,
            if de {
                format!("Suche die besten Momente: {name} ({} von {})", k + 1, total - 2)
            } else {
                format!("Finding the best moments: {name} ({} of {})", k + 1, total - 2)
            },
        );
        let highlights: Option<Highlights> = pool.open_file(&c.path, services).ok().and_then(|src| best_moments(&src, cancel));
        media.push(Medium { candidate: c, highlights });
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
    Ok(Analysis { plan: plan.clone(), media, music, scanned })
}

/// What is known about a file without reading its metadata (the library step replaces it).
struct Captured {
    created: Option<String>,
    modified: Option<String>,
    location: Option<[f64; 2]>,
    width: u32,
    height: u32,
    seconds: Option<f64>,
}

fn capture_fallback(path: &std::path::Path) -> Captured {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| crate::media_browser::format_date(d.as_secs()).replacen(' ', "T", 1) + ":00Z");
    Captured { created: None, modified, location: None, width: 0, height: 0, seconds: None }
}

/// The best moments of a video (the analysis step's `media.highlights`).
fn best_moments(src: &filmcraft_media::SharedSource, cancel: &AtomicBool) -> Option<Highlights> {
    let _ = (src, cancel);
    None
}

/// The beat grid of a music file (the analysis step's `audio.beats`).
fn beats_of_source(src: &filmcraft_media::SharedSource) -> Option<Beats> {
    let _ = src;
    None
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
        }
        media.retain(|m| m.candidate.item.is_some());
        let music = a.music.as_ref().and_then(|(path, beats)| item_for_path(s, path).map(|i| Music { item: i.0, beats: beats.clone() }));
        let mut v = super::assemble::assemble_in(s, &plan, &media, music.as_ref())?;
        v["scanned"] = json!(a.scanned);
        Ok(v)
    })
}

/// What a search found.
pub struct Found {
    pub candidates: Vec<Candidate>,
    pub scanned: u64,
    pub folders: Vec<String>,
}

/// Search the plan's folders for its media (capture date, place and kind filters).
fn search(plan: &Plan, cancel: &AtomicBool) -> std::result::Result<Found, String> {
    let folders: Vec<std::path::PathBuf> = plan.folders.iter().map(|f| expand_home(f)).collect();
    let mut candidates = Vec::new();
    let mut scanned = 0u64;
    for root in &folders {
        walk(root, root, 0, &mut candidates, &mut scanned, cancel);
    }
    candidates.retain(|c| in_range(c, plan));
    candidates.sort_by(|a, b| a.created.cmp(&b.created).then(a.path.cmp(&b.path)));
    Ok(Found { candidates, scanned, folders: folders.iter().map(|f| f.display().to_string()).collect() })
}

fn expand_home(f: &str) -> std::path::PathBuf {
    match (f.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(f),
    }
}

fn in_range(c: &Candidate, plan: &Plan) -> bool {
    let day = c.created.as_deref().or(c.modified.as_deref()).and_then(|d| d.get(..10)).unwrap_or("");
    let after = plan.from.as_deref().is_none_or(|f| day.is_empty() || day >= f);
    let before = plan.to.as_deref().is_none_or(|t| day.is_empty() || day <= t);
    let place = plan.places.is_empty() || {
        let hay = format!("{} {} {}", c.place.clone().unwrap_or_default(), c.folder, c.path).to_lowercase();
        plan.places.iter().any(|p| hay.contains(&p.to_lowercase()))
    };
    after && before && place
}

/// Fallback walk (the library step's search replaces it): files by extension.
fn walk(root: &std::path::Path, dir: &std::path::Path, depth: usize, out: &mut Vec<Candidate>, scanned: &mut u64, cancel: &AtomicBool) {
    if depth > 8 || *scanned > 50_000 || cancel.load(Ordering::Relaxed) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(root, &path, depth + 1, out, scanned, cancel);
            continue;
        }
        *scanned += 1;
        let ext = path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        let kind = match ext.as_str() {
            "mp4" | "mov" | "m4v" | "mkv" | "webm" | "avi" | "mts" | "m2ts" => "video",
            "jpg" | "jpeg" | "png" | "tif" | "tiff" | "webp" | "bmp" => "photo",
            "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "aif" | "aiff" => "audio",
            _ => continue,
        };
        let folder = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let info = capture_fallback(&path);
        out.push(Candidate {
            path: path.to_string_lossy().into_owned(),
            kind: kind.into(),
            folder,
            created: info.created,
            modified: info.modified,
            location: info.location,
            width: info.width,
            height: info.height,
            seconds: info.seconds,
            ..Default::default()
        });
        let _ = root;
    }
}
