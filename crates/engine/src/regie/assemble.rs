//! `regie.assemble`, `regie.run`, `regie.status`: build the film (see [the module overview](super)).
//!
//! [`assemble`] turns a [`Plan`], the media (with their best segments) and the music's beat grid
//! into a new sequence, using the same commands a person would (`timeline.place`,
//! `clip.fillFrame`, `effects.setParam`, `graphics.newText`, transitions…) inside one
//! [`Session::edit_group`], so the whole film is one undo step and every cut stays editable.

use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_project::ItemId;
use filmcraft_time::{FrameRate, Tick};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{EngineError, Result, Session};

use super::types::{Beats, Candidate, Highlights, Plan};

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "regie.assemble",
            label: "AI Director: Build Film",
            menu: &[],
            shortcut: None,
            params: r#"{"plan":Plan,"media":[{…Candidate,"item":id,"highlights":Highlights?}],"music":{"item":id,"beats":Beats?}?}"#,
            enabled: always,
            run: run_assemble,
            journal: true,
        },
        CommandSpec {
            id: "regie.run",
            label: "AI Director: Make Film",
            menu: &[],
            shortcut: None,
            params: r#"{"request":str?,"plan":Plan?,"folders":[str]?,"wait":bool=false}"#,
            enabled: always,
            run: super::run::start,
            journal: true,
        },
        CommandSpec {
            id: "regie.status",
            label: "AI Director: Status",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: always,
            run: run_status,
            journal: false,
        },
    ]
}

/// What the AI Director is doing and what it did last (kept in the session).
#[derive(Default)]
pub struct RegieState {
    /// The running job, if any.
    pub pending: Option<super::run::Pending>,
    /// The last finished film (the `regie.assemble` result) or error.
    pub last: Option<std::result::Result<Value, String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a film is being made, its progress and the last result.
fn run_status(s: &mut Session, _p: &Value) -> Result<Value> {
    use std::sync::atomic::Ordering;
    let pending = s.regie.pending.as_ref().map(|p| p.job);
    let job = pending.and_then(|id| s.jobs.iter().find(|j| j.id == id));
    let (progress, status) = match job {
        Some(j) => (j.progress.fraction(), lock(&j.progress.status).clone()),
        None => (if s.regie.last.is_some() { 1.0 } else { 0.0 }, String::new()),
    };
    let running = job.is_some_and(|j| !j.progress.finished.load(Ordering::Relaxed)) || pending.is_some();
    let (result, error) = match &s.regie.last {
        Some(Ok(v)) => (v.clone(), Value::Null),
        Some(Err(e)) => (Value::Null, json!(e)),
        None => (Value::Null, Value::Null),
    };
    Ok(json!({"running": running, "job": pending, "progress": progress, "status": status, "result": result, "error": error}))
}

/// One medium with its project item, as `regie.assemble` takes it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Medium {
    #[serde(flatten)]
    pub candidate: Candidate,
    pub highlights: Option<Highlights>,
}

/// The music to cut to.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Music {
    pub item: u64,
    pub beats: Option<Beats>,
}

fn run_assemble(s: &mut Session, p: &Value) -> Result<Value> {
    let plan: Plan = serde_json::from_value(p.get("plan").cloned().unwrap_or(Value::Null)).map_err(|e| bad("regie.assemble", format!("`plan`: {e}")))?;
    let media: Vec<Medium> =
        serde_json::from_value(p.get("media").cloned().unwrap_or(json!([]))).map_err(|e| bad("regie.assemble", format!("`media`: {e}")))?;
    let music: Option<Music> = match p.get("music") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| bad("regie.assemble", format!("`music`: {e}")))?),
    };
    let r = assemble(s, &plan, &media, music.as_ref());
    s.regie.last = Some(r.as_ref().map(Clone::clone).map_err(ToString::to_string));
    r
}

/// Longest film the recipe builds, seconds.
const MAX_FILM: f64 = 900.0;

/// A shot of the film, before it is placed.
#[derive(Clone, Debug)]
struct Shot {
    item: ItemId,
    photo: bool,
    /// Media time where the shot starts (videos), seconds.
    source_in: f64,
    /// The medium's shape differs from the sequence's: fit it over a blurred copy.
    letterboxed: bool,
    chapter: usize,
    start: Tick,
    duration: Tick,
}

/// A chapter of the film: media close together in time.
#[derive(Clone, Debug)]
struct Chapter {
    title: String,
    subtitle: String,
    members: Vec<usize>,
}

/// Seconds since the epoch of an RFC 3339 / ISO date-time (offset applied), for ordering.
pub fn timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    let date = s.get(..10)?;
    let (y, m, d) = super::plan::parse_date(date)?;
    let rest = s.get(10..).unwrap_or("");
    let rest = rest.trim_start_matches(['T', ' ']);
    let (hh, mm, ss) = {
        let t = rest.get(..8).unwrap_or("00:00:00");
        let mut it = t.split(':');
        let h: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let mi: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let se: f64 = it.next().and_then(|v| v.get(..2).unwrap_or(v).parse().ok()).unwrap_or(0.0);
        (h, mi, se)
    };
    let offset = rest
        .rfind(['+', '-'])
        .filter(|i| *i >= 8)
        .and_then(|i| {
            let sign = if rest.get(i..i + 1) == Some("-") { -1.0 } else { 1.0 };
            let o = rest.get(i + 1..)?.replace(':', "");
            let h: f64 = o.get(..2)?.parse().ok()?;
            let mi: f64 = o.get(2..4).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            Some(sign * (h * 3600.0 + mi * 60.0))
        })
        .unwrap_or(0.0);
    let days = days_from_civil(y, m, d) as f64;
    Some(days * 86_400.0 + hh * 3600.0 + mm * 60.0 + ss - offset)
}

/// Days since 1970-01-01 (Howard Hinnant's days-from-civil).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// When a medium was made: capture time, else modification time.
fn when(c: &Candidate) -> Option<f64> {
    c.created.as_deref().and_then(timestamp).or_else(|| c.modified.as_deref().and_then(timestamp))
}

/// The month (1..12) and year of a medium's local capture date.
fn month_of(c: &Candidate) -> Option<(i32, u32)> {
    let s = c.created.as_deref().or(c.modified.as_deref())?;
    let (y, m, _) = super::plan::parse_date(s.get(..10)?)?;
    Some((y, m))
}

/// A folder name as a chapter title: dates, numbers and separators at the start removed, words
/// capitalised ("05_sommer" → "Sommer", "2026-09 Mykonos" → "Mykonos").
pub fn folder_title(folder: &str) -> String {
    let trimmed = folder.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ' '));
    let words: Vec<String> = trimmed
        .split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs).collect(),
                None => String::new(),
            }
        })
        .collect();
    words.join(" ")
}

/// The name a medium gives its chapter: its place, else its folder.
fn place_key(c: &Candidate) -> String {
    c.place.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| folder_title(&c.folder))
}

/// Group media (sorted by time) into chapters. A gap of more than 36 hours starts a new chapter
/// when the place (or folder) changes too; a gap of more than a week always does. So a trip
/// with a quiet day in the middle stays one chapter, and two trips to the same town are two.
fn chapters(media: &[Medium], order: &[usize], language: &str) -> Vec<Chapter> {
    let mut out: Vec<Chapter> = Vec::new();
    let mut last: Option<f64> = None;
    let mut key = String::new();
    for &i in order {
        let Some(m) = media.get(i) else { continue };
        let t = when(&m.candidate);
        let k = place_key(&m.candidate);
        let gap = match (last, t) {
            (Some(a), Some(b)) => (b - a > 36.0 * 3600.0 && k != key) || b - a > 7.0 * 86_400.0,
            (None, _) => true,
            _ => false,
        };
        if gap || out.is_empty() {
            out.push(Chapter { title: String::new(), subtitle: String::new(), members: Vec::new() });
            key = k;
        }
        if let Some(c) = out.last_mut() {
            c.members.push(i);
        }
        if t.is_some() {
            last = t;
        }
    }
    for c in &mut out {
        // the most common place, else the most common folder
        let mut names: Vec<(String, usize)> = Vec::new();
        for &i in &c.members {
            let Some(m) = media.get(i) else { continue };
            let name = place_key(&m.candidate);
            if name.is_empty() {
                continue;
            }
            match names.iter_mut().find(|(n, _)| *n == name) {
                Some(e) => e.1 += 1,
                None => names.push((name, 1)),
            }
        }
        names.sort_by_key(|a| std::cmp::Reverse(a.1));
        c.title = names.first().map(|n| n.0.clone()).unwrap_or_default();
        let months: Vec<(i32, u32)> = c.members.iter().filter_map(|i| media.get(*i)).filter_map(|m| month_of(&m.candidate)).collect();
        if let (Some(a), Some(b)) = (months.iter().min(), months.iter().max()) {
            c.subtitle = if a == b {
                format!("{} {}", super::plan::month_name(a.1, language), a.0)
            } else if a.0 == b.0 {
                format!("{} – {} {}", super::plan::month_name(a.1, language), super::plan::month_name(b.1, language), b.0)
            } else {
                format!("{} {} – {} {}", super::plan::month_name(a.1, language), a.0, super::plan::month_name(b.1, language), b.0)
            };
        }
        if c.title.is_empty() {
            c.title = std::mem::take(&mut c.subtitle);
        }
    }
    out
}

/// Seconds → ticks on the sequence's frame grid.
fn frame_tick(secs: f64, rate: FrameRate) -> Tick {
    let fd = rate.frame_duration();
    let fps = if fd.0 > 0 { filmcraft_time::TICKS_PER_SECOND as f64 / fd.0 as f64 } else { 25.0 };
    let frames = (secs.max(0.0) * fps).round().min(1e12) as i64;
    Tick(fd.0.saturating_mul(frames))
}

/// The beat grid to cut on: the music's beats, else a steady 120 BPM grid.
fn grid(beats: Option<&Beats>, length: f64) -> (Vec<f64>, Vec<bool>) {
    let mut times: Vec<f64> = beats.map(|b| b.beats.iter().copied().filter(|t| t.is_finite() && *t > 0.05 && *t < length).collect()).unwrap_or_default();
    times.sort_by(f64::total_cmp);
    times.dedup_by(|a, b| (*a - *b).abs() < 0.2);
    if times.len() < 8 {
        times = (1..).map(|k| k as f64 * 0.5).take_while(|t| *t < length).collect();
    }
    let lively: Vec<bool> = times
        .iter()
        .map(|t| beats.and_then(|b| b.sections.iter().find(|s| *t >= s.start && *t < s.end.max(s.start + 0.001))).is_some_and(|s| s.kind == "lively"))
        .collect();
    (times, lively)
}

/// Build the film as one undo step; returns the new sequence and a summary.
pub fn assemble(s: &mut Session, plan: &Plan, media: &[Medium], music: Option<&Music>) -> Result<Value> {
    let label = if plan.language == "de" { "KI-Regie: Film" } else { "AI Director: film" };
    s.edit_group(label, |s| assemble_in(s, plan, media, music))
}

/// [`assemble`] without its own undo group (the caller groups it with the import).
pub fn assemble_in(s: &mut Session, plan: &Plan, media: &[Medium], music: Option<&Music>) -> Result<Value> {
    let de = plan.language == "de";
    if media.is_empty() {
        return Err(EngineError::Other(if de { "Keine passenden Medien gefunden.".into() } else { "No matching media found.".into() }));
    }
    if !(16..=8192).contains(&plan.width) || !(16..=8192).contains(&plan.height) || !(plan.fps.is_finite() && (1.0..=240.0).contains(&plan.fps)) {
        return Err(bad("regie.assemble", "the plan's frame size or rate is out of range"));
    }
    let rate = FrameRate::from_f64(plan.fps);
    let music_seconds = music.and_then(|m| m.beats.as_ref()).map(|b| b.seconds).filter(|v| v.is_finite() && *v > 1.0);

    // order and chapters
    let mut order: Vec<usize> = (0..media.len()).filter(|i| media.get(*i).is_some_and(|m| m.candidate.item.is_some())).collect();
    order.sort_by(|a, b| {
        let (ta, tb) = (media.get(*a).and_then(|m| when(&m.candidate)), media.get(*b).and_then(|m| when(&m.candidate)));
        ta.unwrap_or(f64::MAX).total_cmp(&tb.unwrap_or(f64::MAX)).then(a.cmp(b))
    });
    if order.is_empty() {
        return Err(bad("regie.assemble", "no medium has a project item"));
    }
    let chs = chapters(media, &order, &plan.language);

    // length and the grid
    let natural: f64 = order.len() as f64 * 2.2;
    let mut length = plan.seconds.filter(|v| v.is_finite() && *v >= 3.0).or(music_seconds).unwrap_or(natural).min(MAX_FILM);
    if let Some(ms) = music_seconds {
        length = length.min(ms);
    }
    length = length.min(natural.max(8.0) * 1.6);
    let (beats, lively) = grid(music.and_then(|m| m.beats.as_ref()), length);
    let beat = beats
        .windows(2)
        .map(|w| w.get(1).copied().unwrap_or(0.0) - w.first().copied().unwrap_or(0.0))
        .filter(|d| *d > 0.0)
        .fold((0.0, 0), |(s, n), d| (s + d, n + 1));
    let beat = if beat.1 > 0 { beat.0 / beat.1 as f64 } else { 0.5 };
    let bar = (beat * 4.0).clamp(1.0, 4.0);

    // how many shots each chapter gets: proportional to the square root of its size, at least 2
    let avg_shot = bar * if plan.mood == "calm" { 1.0 } else { 0.8 };
    let total_shots = ((length / avg_shot).floor() as usize).max(1);
    let weights: Vec<f64> = chs.iter().map(|c| (c.members.len() as f64).sqrt()).collect();
    let wsum: f64 = weights.iter().sum::<f64>().max(1e-9);
    let mut picks: Vec<Vec<usize>> = Vec::new();
    for (c, w) in chs.iter().zip(&weights) {
        let want = ((total_shots as f64 * w / wsum).round() as usize).clamp(2.min(c.members.len()), c.members.len());
        picks.push(pick(media, &c.members, want));
    }

    // lay the shots on the grid
    let mut shots: Vec<Shot> = Vec::new();
    let mut beat_index = 0usize;
    let mut t = 0.0f64;
    'chapters: for (ci, members) in picks.iter().enumerate() {
        for &i in members {
            let Some(m) = media.get(i) else { continue };
            let Some(item) = m.candidate.item else { continue };
            let photo = m.candidate.kind == "photo";
            let is_lively = lively.get(beat_index).copied().unwrap_or(false);
            let n_beats = if photo || plan.mood == "calm" || !is_lively { 4 } else { 2 };
            let mut next = beat_index + n_beats;
            // never shorter than 0.8 s
            while beats.get(next).is_some_and(|b| *b - t < 0.8) {
                next += 1;
            }
            let end = beats.get(next).copied().unwrap_or(length).min(length);
            if end - t < 0.5 {
                break 'chapters;
            }
            let dur = end - t;
            let (source_in, media_seconds) = if photo {
                (0.0, None)
            } else {
                let secs = m.candidate.seconds.or(m.highlights.as_ref().map(|h| h.seconds)).filter(|v| v.is_finite() && *v > 0.0);
                let best = m.highlights.as_ref().and_then(|h| h.segments.iter().max_by(|a, b| a.score.total_cmp(&b.score)));
                let centre = match (best, secs) {
                    (Some(b), _) => (b.start + b.end) / 2.0,
                    (None, Some(sec)) => sec / 2.0,
                    (None, None) => dur / 2.0,
                };
                let max_in = secs.map(|v| (v - dur).max(0.0)).unwrap_or(f64::MAX);
                ((centre - dur / 2.0).clamp(0.0, max_in), secs)
            };
            if media_seconds.is_some_and(|v| v < 0.3) {
                continue;
            }
            let (mw, mh) = (m.candidate.width.max(1) as f64, m.candidate.height.max(1) as f64);
            let ratio = (mw / mh) / (plan.width as f64 / plan.height as f64);
            let letterboxed = m.candidate.width > 0 && m.candidate.height > 0 && !(0.85..=1.18).contains(&ratio);
            let start = frame_tick(t, rate);
            let stop = frame_tick(end, rate);
            // a video shorter than its slot ends early: the slot shrinks to the video
            let stop = match media_seconds {
                Some(v) if v - source_in < dur => frame_tick(t + (v - source_in).max(0.3), rate),
                _ => stop,
            };
            if stop <= start {
                continue;
            }
            shots.push(Shot { item: ItemId(item), photo, source_in, letterboxed, chapter: ci, start, duration: stop - start });
            t = (stop.0 as f64) / filmcraft_time::TICKS_PER_SECOND as f64;
            beat_index = beats.iter().position(|b| *b >= t - 0.02).unwrap_or(beats.len());
            if t >= length - 0.5 {
                break 'chapters;
            }
        }
    }
    if shots.is_empty() {
        return Err(EngineError::Other(if de { "Aus den Medien ließ sich kein Film bauen.".into() } else { "Could not build a film from the media.".into() }));
    }
    let film_end = shots.iter().map(|x| x.start + x.duration).max().unwrap_or(Tick::ZERO);

    let title = plan.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(plan, &chs, media));
    let music_item = music.map(|m| m.item);
    let built = build(s, plan, &title, &shots, &chs, rate, film_end, music_item)?;

    let photos = shots.iter().filter(|x| x.photo).count();
    let videos = shots.len() - photos;
    let secs = film_end.seconds();
    let chapter_list: Vec<Value> = chs
        .iter()
        .enumerate()
        .filter_map(|(ci, c)| {
            let first = shots.iter().find(|x| x.chapter == ci)?;
            Some(json!({"title": c.title, "subtitle": c.subtitle, "start": first.start.seconds(), "shots": shots.iter().filter(|x| x.chapter == ci).count()}))
        })
        .collect();
    let mut summary = Vec::new();
    let mut explain = Vec::new();
    if de {
        summary.push(format!(
            "„{title}“: {} Kapitel, {} Fotos und {} Videoausschnitte, {}:{:02} Minuten.",
            chapter_list.len(),
            photos,
            videos,
            secs as u64 / 60,
            secs as u64 % 60
        ));
        if music_item.is_some() {
            summary.push("Geschnitten auf den Takt der Musik; die Musik blendet am Ende aus.".into());
        }
        explain.push(format!("Neue Sequenz in {} × {}, {} Bilder pro Sekunde (Datei ▸ Neu ▸ Sequenz).", plan.width, plan.height, plan.fps));
        explain.push("Die Clips liegen auf Spur V2, jeder Schnitt sitzt auf einem Taktschlag (Clip auf die Zeitleiste ziehen, mit In- und Out-Punkt).".into());
        explain.push("Hochkant-Material steht vor einer unscharfen, abgedunkelten Kopie auf V1 (Effekte ▸ Weichzeichnen ▸ Gaußscher Weichzeichner, Helligkeit & Kontrast).".into());
        explain.push("Fotos zoomen langsam (Effekteinstellungen ▸ Bewegung ▸ Skalierung mit zwei Keyframes).".into());
        explain.push(
            "Titel und Kapitelnamen liegen auf V3 (Grafiken ▸ Neuer Text) und werden weich ein- und ausgeblendet (Videoübergänge ▸ Weiche Blende).".into(),
        );
        if music_item.is_some() {
            explain.push("Die Musik liegt auf A1 und blendet aus (Audioübergänge ▸ Konstante Leistung).".into());
        }
        explain.push("Alles ist ein einziger Schritt: Bearbeiten ▸ Rückgängig nimmt den ganzen Film zurück.".into());
    } else {
        summary.push(format!(
            "“{title}”: {} chapters, {} photos and {} video shots, {}:{:02} minutes.",
            chapter_list.len(),
            photos,
            videos,
            secs as u64 / 60,
            secs as u64 % 60
        ));
        if music_item.is_some() {
            summary.push("Cut to the music's beat; the music fades out at the end.".into());
        }
        explain.push(format!("A new {} × {} sequence at {} frames per second (File ▸ New ▸ Sequence).", plan.width, plan.height, plan.fps));
        explain.push("The shots are on track V2, every cut on a beat (drag a clip to the timeline with In and Out points).".into());
        explain.push("Portrait media sit on a blurred, darkened copy on V1 (Effects ▸ Blur ▸ Gaussian Blur, Brightness & Contrast).".into());
        explain.push("Photos zoom slowly (Effect Controls ▸ Motion ▸ Scale with two keyframes).".into());
        explain.push("Titles and chapter names are on V3 (Graphics ▸ New Text) and fade in and out (Video Transitions ▸ Cross Dissolve).".into());
        if music_item.is_some() {
            explain.push("The music is on A1 and fades out (Audio Transitions ▸ Constant Power).".into());
        }
        explain.push("It is all one step: Edit ▸ Undo removes the whole film.".into());
    }
    Ok(json!({
        "sequence": built.0,
        "title": title,
        "clips": shots.len(),
        "photos": photos,
        "videos": videos,
        "seconds": secs,
        "chapters": chapter_list,
        "summary": summary,
        "explain": explain,
    }))
}

/// `want` media of a chapter: its time span cut into `want` stretches, the best medium of each
/// (a video by its best segment's score, a photo counts as 0.55), in time order.
fn pick(media: &[Medium], members: &[usize], want: usize) -> Vec<usize> {
    if want >= members.len() {
        return members.to_vec();
    }
    let score = |i: usize| -> f64 {
        media
            .get(i)
            .map(|m| {
                let base = if m.candidate.kind == "photo" {
                    0.55
                } else {
                    m.highlights.as_ref().and_then(|h| h.segments.iter().map(|s| s.score).reduce(f64::max)).unwrap_or(0.5)
                };
                // landscape media fill a landscape frame: a small bonus
                base + if m.candidate.width >= m.candidate.height { 0.03 } else { 0.0 }
            })
            .unwrap_or(0.0)
    };
    let mut out = Vec::with_capacity(want);
    for k in 0..want {
        let a = k * members.len() / want.max(1);
        let b = ((k + 1) * members.len() / want.max(1)).max(a + 1).min(members.len());
        let best = members.get(a..b).and_then(|slice| slice.iter().copied().max_by(|x, y| score(*x).total_cmp(&score(*y))));
        if let Some(i) = best {
            out.push(i);
        }
    }
    out
}

fn default_title(plan: &Plan, chs: &[Chapter], media: &[Medium]) -> String {
    let de = plan.language == "de";
    let years: Vec<i32> = media.iter().filter_map(|m| month_of(&m.candidate)).map(|(y, _)| y).collect();
    let year = match (years.iter().min(), years.iter().max()) {
        (Some(a), Some(b)) if a == b => format!(" {a}"),
        (Some(a), Some(b)) => format!(" {a}–{b}"),
        _ => String::new(),
    };
    match (plan.kind.as_str(), chs.len()) {
        ("recap", n) if n > 1 => {
            if de {
                format!("Mein Reisejahr{year}")
            } else {
                format!("My year of travel{year}")
            }
        }
        (_, _) => match chs.first() {
            Some(c) if !c.title.is_empty() => format!("{}{year}", c.title),
            _ => {
                if de {
                    format!("Mein Film{year}")
                } else {
                    format!("My film{year}")
                }
            }
        },
    }
}

/// Run a command inside the build; `Ok(None)` when it failed but the film can do without it.
fn soft(s: &mut Session, id: &str, p: Value) -> Option<Value> {
    match s.execute(id, p) {
        Ok(v) => Some(v),
        Err(e) => {
            log::info!("AI Director: {id} skipped: {e}");
            None
        }
    }
}

fn clips_of(v: &Value) -> Vec<u64> {
    v.get("clips").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default()
}

/// Place everything (inside the edit group). Returns the sequence id.
#[allow(clippy::too_many_arguments)]
fn build(s: &mut Session, plan: &Plan, title: &str, shots: &[Shot], chs: &[Chapter], rate: FrameRate, film_end: Tick, music: Option<u64>) -> Result<(u64,)> {
    let de = plan.language == "de";
    let seq = s.execute(
        "file.newSequence",
        json!({"name": title, "width": plan.width, "height": plan.height, "fps": plan.fps, "sampleRate": 48000, "video": 4, "audio": 2}),
    )?;
    let seq_id = seq.get("sequence").and_then(Value::as_u64).ok_or_else(|| EngineError::Other("file.newSequence returned no sequence".into()))?;
    let fd = rate.frame_duration();
    let (w, h) = (plan.width as f64, plan.height as f64);
    let fade = (rate.as_f64() * 0.4).round().max(1.0) as i64;
    let mut first_main: Option<u64> = None;
    let mut last_main: Option<u64> = None;
    let mut first_bg: Option<u64> = None;
    let mut last_bg: Option<u64> = None;
    let mut chapter_first: Vec<(usize, u64)> = Vec::new();
    for (k, shot) in shots.iter().enumerate() {
        let mut place =
            json!({"item": shot.item.0, "track": "V2", "time": shot.start.0, "duration": shot.duration.0, "audio": music.is_none(), "insert": false});
        if !shot.photo {
            place["sourceIn"] = json!(frame_tick(shot.source_in, rate).0);
        }
        let Some(placed) = soft(s, "timeline.place", place.clone()) else { continue };
        let Some(&clip) = clips_of(&placed).first() else { continue };
        if first_main.is_none() {
            first_main = Some(clip);
        }
        last_main = Some(clip);
        if chapter_first.last().is_none_or(|(c, _)| *c != shot.chapter) {
            chapter_first.push((shot.chapter, clip));
        }
        // fill the frame, or fit it over a blurred, darker copy of itself
        let fitted = soft(s, if shot.letterboxed { "clip.fitToFrame" } else { "clip.fillFrame" }, json!({"clips": [clip]}));
        let base = fitted.as_ref().and_then(|v| v.get("clips")?.get(0)?.get("scale")?.as_f64()).unwrap_or(100.0);
        if shot.letterboxed {
            let mut bg = place.clone();
            bg["track"] = json!("V1");
            bg["audio"] = json!(false);
            if let Some(bg_clip) = soft(s, "timeline.place", bg).and_then(|v| clips_of(&v).first().copied()) {
                soft(s, "clip.fillFrame", json!({"clips": [bg_clip]}));
                soft(s, "effects.apply", json!({"clips": [bg_clip], "effect": "gaussian_blur"}));
                soft(s, "effects.setParam", json!({"clip": bg_clip, "effect": "gaussian_blur", "param": "blurriness", "value": 60.0 * h / 1080.0}));
                soft(s, "effects.setParam", json!({"clip": bg_clip, "effect": "gaussian_blur", "param": "repeat_edge", "value": true}));
                soft(s, "effects.apply", json!({"clips": [bg_clip], "effect": "brightness_contrast"}));
                soft(s, "effects.setParam", json!({"clip": bg_clip, "effect": "brightness_contrast", "param": "brightness", "value": -35.0}));
                if first_bg.is_none() && k == 0 {
                    first_bg = Some(bg_clip);
                }
                last_bg = Some(bg_clip);
            }
        } else if k + 1 == shots.len() {
            last_bg = None;
        }
        // photos move slowly: zoom in on even shots, out on odd ones
        if shot.photo {
            let (a, b) = if k % 2 == 0 { (base, base * 1.08) } else { (base * 1.08, base) };
            let end = Tick(shot.start.0 + (shot.duration.0 - fd.0).max(0));
            soft(s, "effects.setParam", json!({"clip": clip, "effect": "motion", "param": "scale", "value": a, "time": shot.start.0, "keyframe": true}));
            soft(s, "effects.setParam", json!({"clip": clip, "effect": "motion", "param": "scale", "value": b, "time": end.0, "keyframe": true}));
        }
    }
    // soft cross dissolves into each new chapter, a fade from and to black
    for (_, clip) in chapter_first.iter().skip(1) {
        soft(s, "sequence.applyVideoTransition", json!({"clip": clip, "effect": "cross_dissolve", "frames": fade * 2, "edge": "in"}));
    }
    for clip in [first_main, first_bg].into_iter().flatten() {
        soft(s, "sequence.applyVideoTransition", json!({"clip": clip, "effect": "cross_dissolve", "frames": fade * 2, "edge": "in"}));
    }
    for clip in [last_main, last_bg].into_iter().flatten() {
        soft(s, "sequence.applyVideoTransition", json!({"clip": clip, "effect": "cross_dissolve", "frames": fade * 3, "edge": "out"}));
    }

    // titles on V3 (track index 2)
    let bar_ticks = |secs: f64| frame_tick(secs, rate);
    let title_len = bar_ticks(4.0).min(film_end);
    let big = (h * 0.085).round();
    let small = (h * 0.042).round();
    let mut title_clips = Vec::new();
    if let Some(v) = soft(
        s,
        "graphics.newText",
        json!({"text": title, "size": big, "font": "Inter", "fontStyle": "Bold", "position": [w / 2.0, h * 0.50], "seconds": title_len.seconds(), "track": 2, "time": 0}),
    ) {
        title_clips.push(v);
    }
    let year_line = chs.iter().map(|c| c.subtitle.clone()).find(|s| !s.is_empty()).map(|s| s.rsplit(' ').next().unwrap_or("").to_string()).unwrap_or_default();
    if !year_line.is_empty()
        && !title.contains(&year_line)
        && let Some(v) = soft(
            s,
            "graphics.newText",
            json!({"text": year_line, "size": small, "font": "Inter", "position": [w / 2.0, h * 0.60], "seconds": title_len.seconds(), "track": 3, "time": 0}),
        )
    {
        title_clips.push(v);
    }
    for (ci, _) in &chapter_first {
        let Some(c) = chs.get(*ci) else { continue };
        let Some(shot) = shots.iter().find(|x| x.chapter == *ci) else { continue };
        // the first chapter's name waits for the film title to go; a name stays up for at most
        // three seconds and never into the next chapter
        let at = if shot.start < title_len { title_len } else { shot.start };
        let chapter_end = shots.iter().filter(|x| x.chapter > *ci).map(|x| x.start).min().unwrap_or(film_end).min(film_end);
        let len = bar_ticks(3.0).min(chapter_end - at.min(chapter_end));
        if len < bar_ticks(1.0) || c.title.is_empty() {
            continue;
        }
        if let Some(v) = soft(
            s,
            "graphics.newText",
            json!({"text": c.title, "size": (h * 0.065).round(), "font": "Inter", "fontStyle": "Bold", "position": [w / 2.0, h * 0.80], "seconds": len.seconds(), "track": 2, "time": at.0}),
        ) {
            title_clips.push(v);
        }
        if !c.subtitle.is_empty()
            && c.subtitle != c.title
            && let Some(v) = soft(
                s,
                "graphics.newText",
                json!({"text": c.subtitle, "size": small, "font": "Inter", "position": [w / 2.0, h * 0.88], "seconds": len.seconds(), "track": 3, "time": at.0}),
            )
        {
            title_clips.push(v);
        }
    }
    let outro = if de { "Bis zum nächsten Abenteuer" } else { "Until the next adventure" };
    let outro_len = bar_ticks(4.0);
    if film_end > outro_len + title_len {
        let at = film_end - outro_len;
        if let Some(v) = soft(
            s,
            "graphics.newText",
            json!({"text": outro, "size": (h * 0.06).round(), "font": "Inter", "fontStyle": "Bold", "position": [w / 2.0, h * 0.5], "seconds": outro_len.seconds(), "track": 2, "time": at.0}),
        ) {
            title_clips.push(v);
        }
    }
    for v in &title_clips {
        let Some(clip) = v.get("clip").and_then(Value::as_u64) else { continue };
        soft(s, "graphics.set", json!({"clip": clip, "props": {"fill_color": "#ffffff", "shadow": true, "align": "center"}}));
        soft(s, "sequence.applyVideoTransition", json!({"clip": clip, "effect": "cross_dissolve", "frames": fade, "edge": "in"}));
        soft(s, "sequence.applyVideoTransition", json!({"clip": clip, "effect": "cross_dissolve", "frames": fade, "edge": "out"}));
    }

    // the music on A1, fading out with the film
    if let Some(m) = music
        && let Some(placed) =
            soft(s, "timeline.place", json!({"item": m, "track": "A1", "time": 0, "sourceIn": 0, "duration": film_end.0, "video": false, "insert": false}))
    {
        for clip in clips_of(&placed) {
            let frames = (rate.as_f64() * 3.0).round() as i64;
            soft(s, "sequence.applyAudioTransition", json!({"clip": clip, "effect": "constant_power", "frames": frames, "edge": "out"}));
            soft(s, "sequence.applyAudioTransition", json!({"clip": clip, "effect": "constant_power", "frames": fade, "edge": "in"}));
        }
    }
    Ok((seq_id,))
}

/// Medium for a project item (for agents that already imported their media).
pub fn medium(item: ItemId, kind: &str, width: u32, height: u32, created: Option<&str>) -> Medium {
    Medium {
        candidate: Candidate { kind: kind.into(), width, height, created: created.map(str::to_string), item: Some(item.0), ..Default::default() },
        highlights: None,
    }
}

/// Shared slot a background job fills.
pub type Slot<T> = Arc<Mutex<Option<T>>>;
