//! AI Director panel (Window ▸ AI Director; German "KI-Regie"): a beginner says what film they
//! want ("a 16:9 4K recap of this year's holidays, upbeat, about three minutes") and FilmCraft
//! builds a normal, fully editable sequence from their own media.
//!
//! The panel only asks the engine (`docs/regie.md`): Understand runs `regie.plan` and shows what
//! was understood, with the plan's key fields (format, size, frame rate, length, dates, mood,
//! title) to change here. Make film runs `regie.run` with `wait: false`, which returns at once
//! with the job; [`poll`] then asks `regie.status` at most every 250 ms (also while the panel is
//! hidden) for progress and, when done, the result. Cancel is `jobs.cancel`. Play opens the new
//! sequence (`sequence.open`) and plays it from the start, Undo takes the film back
//! (`edit.undo`, only while it is still the last change), and Explain how lists what was done with
//! the menu paths to do it by hand. Every command error becomes one friendly line in the panel
//! (and the log), never a panic; the UI thread never waits for the film.
//!
//! State: [`RegieUi`] in `UiState::regie` (`ui.inspect` → `ui.regie`, `ui.set {"regie": {…}}`).
//!
//! Automation ids: `regie.request`, `regie.example.<0-3>`, `regie.section.folders`,
//! `regie.folders.default`, `regie.folder.<i>`, `regie.folder.remove.<i>`, `regie.folder.add`,
//! `regie.understand`, `regie.make`, `regie.error`, `regie.progress`, `regie.cancel`,
//! `regie.result`, `regie.play`, `regie.undo`, `regie.explain`, `regie.section.plan`,
//! `regie.aspect` (open: `regie.aspect.<16x9|9x16|1x1|4x5>`), `regie.resolution`
//! (`regie.resolution.<hd|uhd>`), `regie.fps` (`regie.fps.<24|25|30|50|60>`), `regie.seconds`,
//! `regie.from`, `regie.to`, `regie.mood` (`regie.mood.<upbeat|calm|epic>`), `regie.title`.

use std::time::Duration;

use egui::{Color32, Rect, RichText, Sense, vec2};
use filmcraft_engine::EngineError;
use filmcraft_engine::regie::Plan;
use filmcraft_project::ItemId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::FilmcraftApp;
use crate::automation::Registry;
use crate::icons::{self, Icon};
use crate::native_dialogs::FileDialog;
use crate::theme::Tokens;

/// Section keys of [`RegieUi::open_sections`].
pub const FOLDERS: &str = "folders";
pub const PLAN: &str = "plan";
pub const EXPLAIN: &str = "explain";

/// How often `regie.status` is asked while a film is being made.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Polls after the job ended without a reported film before the panel stops waiting for it.
const GRACE_POLLS: u32 = 8;

/// Longest wish (characters) and most folders the panel keeps.
pub const MAX_REQUEST_CHARS: usize = 2000;
pub const MAX_FOLDERS: usize = 64;
/// Longest film length the length field takes, in seconds (ten hours).
const MAX_SECONDS: f64 = 36_000.0;
/// Most lines of any list shown (understood, questions, summary, explanation, chapters) and the
/// longest line kept: what the engine answers is data, not a trusted size.
const MAX_LINES: usize = 200;
const MAX_LINE_CHARS: usize = 1000;

/// The picture formats offered, with the automation key of each.
pub const ASPECTS: [(&str, &str); 4] = [("16:9", "16x9"), ("9:16", "9x16"), ("1:1", "1x1"), ("4:5", "4x5")];
/// The frame rates offered.
pub const RATES: [u32; 5] = [24, 25, 30, 50, 60];
/// The moods offered (the plan's values).
pub const MOODS: [&str; 3] = ["upbeat", "calm", "epic"];

/// The panel's state (serde, so agents can read and set it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RegieUi {
    /// The wish as typed (later also spoken).
    pub request: String,
    /// Folders to search; empty: the default folders (Pictures, Videos, Music).
    pub folders: Vec<String>,
    /// What `regie.plan` understood, with the changes made in the panel.
    pub plan: Option<Plan>,
    /// The film being made: its job (`jobs.list`); None when nothing runs.
    pub job: Option<u64>,
    /// Progress of the running film, 0..1, and what it is doing (from `regie.status`).
    pub progress: f64,
    pub status: String,
    /// Cancel was pressed for the running film.
    pub cancelling: bool,
    /// The last film made.
    pub result: Option<RegieResult>,
    /// The last problem, as one friendly line (empty: none).
    pub error: String,
    /// Expanded sections: [`FOLDERS`], [`PLAN`], [`EXPLAIN`].
    pub open_sections: Vec<String>,
    /// Scroll the progress or the new film into view on the next draw.
    #[serde(skip)]
    pub reveal: bool,
}

impl Default for RegieUi {
    fn default() -> Self {
        RegieUi {
            request: String::new(),
            folders: Vec::new(),
            plan: None,
            job: None,
            progress: 0.0,
            status: String::new(),
            cancelling: false,
            result: None,
            error: String::new(),
            open_sections: vec![FOLDERS.into(), PLAN.into()],
            reveal: false,
        }
    }
}

impl RegieUi {
    pub fn is_open(&self, section: &str) -> bool {
        self.open_sections.iter().any(|s| s == section)
    }

    pub fn set_open(&mut self, section: &str, open: bool) {
        if open != self.is_open(section) {
            self.open_sections.retain(|s| s != section);
            if open {
                self.open_sections.push(section.to_string());
            }
        }
    }

    /// A film is being made.
    pub fn running(&self) -> bool {
        self.job.is_some()
    }

    /// The plan was made from the wish as it reads now (otherwise Make film sends the wish).
    pub fn plan_is_current(&self) -> bool {
        self.plan.as_ref().is_some_and(|p| p.request.trim() == self.request.trim())
    }

    /// Add a folder to search (not twice, at most [`MAX_FOLDERS`]). False when it was not added.
    pub fn add_folder(&mut self, folder: &str) -> bool {
        let folder = folder.trim();
        if folder.is_empty() || self.folders.iter().any(|f| f == folder) {
            return false;
        }
        if self.folders.len() >= MAX_FOLDERS {
            self.error = tl!("That is enough folders. Remove one before adding another.").into();
            return false;
        }
        self.folders.push(folder.to_string());
        true
    }

    /// The state with everything an outside source set kept within bounds (`ui.set`).
    pub fn sanitized(mut self) -> Self {
        if self.request.chars().count() > MAX_REQUEST_CHARS {
            self.request = self.request.chars().take(MAX_REQUEST_CHARS).collect();
        }
        self.folders.truncate(MAX_FOLDERS);
        let mut kept: Vec<String> = Vec::with_capacity(self.folders.len());
        for f in std::mem::take(&mut self.folders) {
            let f = f.trim().to_string();
            if !f.is_empty() && !kept.contains(&f) {
                kept.push(f);
            }
        }
        self.folders = kept;
        self.progress = if self.progress.is_finite() { self.progress.clamp(0.0, 1.0) } else { 0.0 };
        self.open_sections.truncate(16);
        self
    }
}

/// A film the AI Director made (what `regie.status` reports when it is done).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RegieResult {
    /// The new sequence (project item id).
    pub sequence: Option<u64>,
    /// How many clips it has.
    pub clips: u64,
    /// Its length in seconds.
    pub seconds: f64,
    /// Its chapter names ("Mykonos · September").
    pub chapters: Vec<String>,
    /// What was made, one line each.
    pub summary: Vec<String>,
    /// What was done, with the menu paths to do it by hand.
    pub explain: Vec<String>,
    /// The folders that were searched, when the director names them (the default ones included).
    pub searched: Vec<String>,
    /// The undo history right after the film was built (its depth and the top step's name): the
    /// panel's Undo takes back only that step, and only while it is still the last change.
    pub undo_depth: Option<usize>,
    pub undo_label: String,
    /// Taken back with the panel's Undo.
    pub undone: bool,
}

impl RegieResult {
    /// Read a `regie.assemble` result (`{"sequence", "clips", "seconds", "chapters", "summary",
    /// "explain"}`, and `"folders"` searched when it names them) leniently: missing or mistyped
    /// fields stay empty, lists are capped.
    pub fn from_json(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let chapters = o
            .get("chapters")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .take(MAX_LINES)
                    .map(|c| match c {
                        Value::String(s) => clip(s),
                        other => other.get("name").or_else(|| other.get("title")).and_then(Value::as_str).map(clip).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let clips = o.get("clips").and_then(|c| c.as_u64().or_else(|| c.as_array().map(|a| a.len() as u64))).unwrap_or(0);
        Some(RegieResult {
            sequence: o.get("sequence").and_then(Value::as_u64),
            clips,
            seconds: o.get("seconds").and_then(Value::as_f64).filter(|s| s.is_finite() && *s >= 0.0).unwrap_or(0.0),
            chapters,
            summary: lines(o.get("summary")),
            explain: lines(o.get("explain")),
            searched: lines(o.get("folders")),
            ..Default::default()
        })
    }
}

/// The strings of a JSON list, capped in number and length.
fn lines(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(MAX_LINES).map(clip).collect()).unwrap_or_default()
}

/// `s`, cut to [`MAX_LINE_CHARS`] characters.
fn clip(s: &str) -> String {
    s.chars().take(MAX_LINE_CHARS).collect()
}

/// `s`, cut to a length that fits a field (64 characters).
fn short(s: &str) -> String {
    s.chars().take(64).collect()
}

/// A finite number from `v[key]`.
fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// The frame size of `aspect` in HD (1080 pixels on the short side) or 4K (2160). An unknown
/// aspect gets the 16:9 sizes.
pub fn frame_size(aspect: &str, uhd: bool) -> (u32, u32) {
    let (w, h) = match aspect {
        "9:16" => (1080, 1920),
        "1:1" => (1080, 1080),
        "4:5" => (1080, 1350),
        _ => (1920, 1080),
    };
    if uhd { (w * 2, h * 2) } else { (w, h) }
}

/// A frame size of the 4K class (at least 2000 pixels on the short side).
pub fn is_uhd(width: u32, height: u32) -> bool {
    width.min(height) >= 2000
}

/// A calendar date written `YYYY-MM-DD`.
pub fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return false;
    }
    let num = |from: usize, to: usize| s.get(from..to).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<u32>().ok());
    let (Some(year), Some(month), Some(day)) = (num(0, 4), num(5, 7), num(8, 10)) else { return false };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

/// A length as `3:05` or `1:02:03`; a dash for one that is not a length.
pub fn fmt_length(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "–".into();
    }
    // float to integer casts saturate, so a huge value can't wrap
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// A frame rate as `25 fps` or `29.97 fps`; a dash for one that is not a rate.
fn fps_text(fps: f64) -> String {
    if !fps.is_finite() || fps <= 0.0 {
        return "–".into();
    }
    let n = format!("{fps:.3}");
    format!("{} fps", n.trim_end_matches('0').trim_end_matches('.'))
}

fn mood_label(mood: &str) -> String {
    match mood {
        "upbeat" => tl!("Upbeat").into(),
        "calm" => tl!("Calm").into(),
        "epic" => tl!("Epic").into(),
        other => short(other),
    }
}

fn aspect_label(aspect: &str) -> String {
    let word = match aspect {
        "16:9" => tl!("Wide"),
        "9:16" => tl!("Upright (Reels, Shorts)"),
        "1:1" => tl!("Square"),
        "4:5" => tl!("Portrait"),
        _ => return short(aspect),
    };
    format!("{aspect} · {word}")
}

/// The wishes the example chips fill in (in the interface language).
pub fn examples() -> [&'static str; 4] {
    [
        tl!("Holiday recap 16:9 4K, about 3 minutes"),
        tl!("Instagram reel 9:16, 30 seconds, fast"),
        tl!("Birthday slideshow with music"),
        tl!("Music video cut to my song"),
    ]
}

/// The language `regie.plan` answers in: the interface's, where the director speaks it.
fn plan_language() -> Option<&'static str> {
    match crate::i18n::current() {
        crate::i18n::Language::De => Some("de"),
        crate::i18n::Language::En => Some("en"),
        _ => None,
    }
}

/// The `regie.run` parameters: the plan (with the panel's changes and folders) when it was made
/// from the wish as it reads now, else the wish itself; the folders when any are chosen.
pub fn run_params(d: &RegieUi) -> Result<Value, String> {
    let mut p = Map::new();
    match d.plan.as_ref().filter(|_| d.plan_is_current()) {
        Some(plan) => {
            let mut plan = plan.clone();
            for date in [&mut plan.from, &mut plan.to] {
                let text = date.as_deref().map(str::trim).unwrap_or_default().to_string();
                if text.is_empty() {
                    *date = None;
                } else if valid_date(&text) {
                    *date = Some(text);
                } else {
                    return Err(tlf!("“{date}” is not a date like 2026-07-31.", date = short(&text)));
                }
            }
            plan.title = plan.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            plan.folders = d.folders.clone();
            p.insert("plan".into(), serde_json::to_value(&plan).map_err(|e| e.to_string())?);
        }
        None => {
            let request = d.request.trim();
            if request.is_empty() {
                return Err(tl!("Type what film you want first.").into());
            }
            p.insert("request".into(), json!(request));
        }
    }
    if !d.folders.is_empty() {
        p.insert("folders".into(), json!(d.folders));
    }
    p.insert("wait".into(), json!(false));
    Ok(Value::Object(p))
}

/// One friendly line for a command that failed.
pub fn friendly(e: &EngineError) -> String {
    match e {
        EngineError::UnknownCommand(_) => tl!("The AI Director can't do this yet in this version of FilmCraft.").into(),
        EngineError::Disabled(_, why) => tlf!("Not possible right now: {why}", why),
        other => tlf!("Something went wrong: {e}", e = other),
    }
}

/// Show a failed command as a friendly line in the panel and the status bar, and log it.
fn fail(app: &mut FilmcraftApp, command: &str, e: &EngineError) {
    log::warn!("AI Director: `{command}` failed: {e}");
    let line = friendly(e);
    app.ui.status = line.clone();
    app.ui.regie.error = line;
}

/// Understand: `regie.plan`, and show what it understood.
fn understand(app: &mut FilmcraftApp) {
    let request = app.ui.regie.request.trim().to_string();
    app.ui.regie.error.clear();
    if request.is_empty() {
        app.ui.regie.error = tl!("Type what film you want first.").into();
        return;
    }
    let mut p = Map::new();
    p.insert("request".into(), json!(request));
    if let Some(language) = plan_language() {
        p.insert("language".into(), json!(language));
    }
    match app.session.execute("regie.plan", Value::Object(p)) {
        Ok(v) => match serde_json::from_value::<Plan>(v) {
            Ok(plan) => {
                let d = &mut app.ui.regie;
                // folders the wish named join the list, where they can be seen and removed
                for f in plan.folders.iter().take(MAX_FOLDERS) {
                    d.add_folder(f);
                }
                d.plan = Some(plan);
                d.set_open(PLAN, true);
            }
            Err(e) => {
                log::warn!("AI Director: unreadable plan: {e}");
                app.ui.regie.error = tl!("The AI Director's answer could not be read.").into();
            }
        },
        Err(e) => fail(app, "regie.plan", &e),
    }
}

/// The sequence the last film had before a new one started (to tell a stale report from the new
/// film), kept per job.
fn previous_id(job: u64) -> egui::Id {
    egui::Id::new(("regie-previous-film", job))
}

/// Make film: `regie.run` without waiting; [`poll`] follows the job.
fn make(app: &mut FilmcraftApp, ctx: &egui::Context) {
    app.ui.regie.error.clear();
    let params = match run_params(&app.ui.regie) {
        Ok(p) => p,
        Err(e) => {
            app.ui.regie.error = e;
            return;
        }
    };
    match app.session.execute("regie.run", params) {
        Ok(v) => {
            if let Some(job) = v.get("job").and_then(Value::as_u64) {
                let previous = app.ui.regie.result.as_ref().and_then(|r| r.sequence);
                ctx.data_mut(|m| m.insert_temp(previous_id(job), previous));
                let d = &mut app.ui.regie;
                d.job = Some(job);
                d.progress = 0.0;
                d.status = tl!("Starting…").into();
                d.cancelling = false;
                d.result = None;
                d.reveal = true;
                ctx.request_repaint();
            } else if let Some(r) = RegieResult::from_json(&v).filter(|r| r.sequence.is_some()) {
                // made at once
                finish(app, r);
            } else {
                stopped(app, None);
            }
        }
        Err(e) => fail(app, "regie.run", &e),
    }
}

/// What the session's job list says about the film's job.
enum JobState {
    Running { progress: f64, status: String },
    Done,
    Failed(String),
    Missing,
}

fn job_state(app: &FilmcraftApp, job: u64) -> JobState {
    let Some(j) = app.session.jobs.iter().find(|j| j.id == job) else { return JobState::Missing };
    let v = j.to_json();
    if v.get("finished").and_then(Value::as_bool) != Some(true) {
        let status = v.get("status").and_then(Value::as_str).map(clip).unwrap_or_default();
        return JobState::Running { progress: number(&v, "progress").unwrap_or(0.0).clamp(0.0, 1.0), status };
    }
    match v.get("result").and_then(|r| r.get("error")).and_then(Value::as_str) {
        Some(e) => JobState::Failed(clip(e)),
        None => JobState::Done,
    }
}

/// Follow the film being made: ask `regie.status` at most every [`POLL_INTERVAL`] (called every
/// frame, also while the panel is hidden) and show its progress, then the film or why there is
/// none. Never waits: the job runs in the background.
pub fn poll(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(job) = app.ui.regie.job else { return };
    let id = egui::Id::new("regie-status-poll");
    let now = web_time::Instant::now();
    if let Some(last) = ctx.data(|m| m.get_temp::<web_time::Instant>(id)) {
        let since = now.saturating_duration_since(last);
        if since < POLL_INTERVAL {
            ctx.request_repaint_after(POLL_INTERVAL.saturating_sub(since));
            return;
        }
    }
    ctx.data_mut(|m| m.insert_temp(id, now));
    ctx.request_repaint_after(POLL_INTERVAL);
    check(app, ctx, job);
}

fn check(app: &mut FilmcraftApp, ctx: &egui::Context, job: u64) {
    let state = job_state(app, job);
    let status = match app.session.execute("regie.status", json!({})) {
        Ok(v) => v,
        Err(e) => {
            // the job list still tells when it ends
            log::debug!("AI Director: regie.status: {e}");
            Value::Null
        }
    };
    let running = status.get("running").and_then(Value::as_bool).unwrap_or(false);
    let ours = status.get("job").and_then(Value::as_u64).is_none_or(|j| j == job);
    if running && ours {
        let d = &mut app.ui.regie;
        if let Some(p) = number(&status, "progress") {
            d.progress = p.clamp(0.0, 1.0);
        }
        if let Some(s) = status.get("status").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            d.status = clip(s);
        }
        return;
    }
    if let JobState::Running { progress, status: text } = state {
        let d = &mut app.ui.regie;
        d.progress = progress;
        if !text.is_empty() {
            d.status = text;
        }
        return;
    }
    let previous = ctx.data(|m| m.get_temp::<Option<u64>>(previous_id(job))).flatten();
    let result = status.get("result").filter(|_| ours).and_then(RegieResult::from_json).filter(|r| previous.is_none() || r.sequence != previous);
    let grace = egui::Id::new(("regie-grace", job));
    match (result, state) {
        (Some(r), _) => {
            ctx.data_mut(|m| {
                m.remove::<u32>(grace);
                m.remove::<Option<u64>>(previous_id(job));
            });
            finish(app, r);
        }
        (None, JobState::Failed(e)) => {
            ctx.data_mut(|m| m.remove::<Option<u64>>(previous_id(job)));
            stopped(app, Some(e));
        }
        (None, _) => {
            // the job is over but its film isn't reported yet: give the director a moment
            let n = ctx.data(|m| m.get_temp::<u32>(grace)).unwrap_or(0);
            if n < GRACE_POLLS {
                ctx.data_mut(|m| m.insert_temp(grace, n + 1));
                return;
            }
            ctx.data_mut(|m| {
                m.remove::<u32>(grace);
                m.remove::<Option<u64>>(previous_id(job));
            });
            stopped(app, None);
        }
    }
}

/// The film is made: show it, and remember its undo step for the panel's Undo.
fn finish(app: &mut FilmcraftApp, mut r: RegieResult) {
    if r.sequence.is_some_and(|s| app.session.project.sequence(ItemId(s)).is_some()) {
        r.undo_depth = Some(app.session.history.undo.len());
        r.undo_label = app.session.history.undo.last().map(|(label, _)| label.clone()).unwrap_or_default();
    }
    let d = &mut app.ui.regie;
    d.job = None;
    d.cancelling = false;
    d.status.clear();
    d.progress = 1.0;
    d.error.clear();
    d.reveal = true;
    d.result = Some(r);
    app.ui.status = tl!("Your film is ready.").into();
}

/// The film's job ended without a film: say why.
fn stopped(app: &mut FilmcraftApp, error: Option<String>) {
    let d = &mut app.ui.regie;
    let cancelled = d.cancelling || error.as_deref().is_some_and(|e| e.to_ascii_lowercase().contains("cancel"));
    let line = match error {
        _ if cancelled => tl!("Stopped. No film was made.").to_string(),
        Some(e) => tlf!("The film could not be made: {e}", e),
        None => tl!("The AI Director stopped without making a film.").to_string(),
    };
    log::warn!("AI Director: {line}");
    d.job = None;
    d.cancelling = false;
    d.status.clear();
    d.progress = 0.0;
    d.error = line.clone();
    app.ui.status = line;
}

fn cancel(app: &mut FilmcraftApp) {
    let Some(job) = app.ui.regie.job else { return };
    match app.session.execute("jobs.cancel", json!({"job": job})) {
        Ok(_) => {
            app.ui.regie.cancelling = true;
            app.ui.regie.status = tl!("Stopping…").into();
        }
        // the job is gone already: nothing is left to stop
        Err(_) if matches!(job_state(app, job), JobState::Missing) => {
            app.ui.regie.cancelling = true;
            stopped(app, None);
        }
        Err(e) => fail(app, "jobs.cancel", &e),
    }
}

/// Play: open the new sequence and play it from the start.
fn play(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(seq) = app.ui.regie.result.as_ref().and_then(|r| r.sequence) else { return };
    if app.session.project.sequence(ItemId(seq)).is_none() {
        app.ui.regie.error = tl!("The film's sequence is no longer in the project.").into();
        return;
    }
    app.ui.regie.error.clear();
    let steps = [("sequence.open", json!({"item": seq})), ("playhead.start", json!({}))];
    for (command, params) in steps {
        if let Err(e) = crate::menus::invoke(app, ctx, command, params) {
            log::warn!("AI Director: `{command}` failed: {e}");
            app.ui.regie.error = tlf!("Something went wrong: {e}", e);
            return;
        }
    }
    if !app.playback.playing
        && let Err(e) = crate::menus::invoke(app, ctx, "playback.toggle", json!({"monitor": "program"}))
    {
        log::warn!("AI Director: `playback.toggle` failed: {e}");
        app.ui.regie.error = tlf!("Something went wrong: {e}", e);
    }
}

/// The film is still the last change (so Undo takes back the film and nothing else).
fn film_is_last_change(app: &FilmcraftApp, r: &RegieResult) -> bool {
    let undo = &app.session.history.undo;
    !r.undone && r.undo_depth.is_some_and(|n| n > 0 && undo.len() == n) && undo.last().is_some_and(|(label, _)| *label == r.undo_label)
}

fn undo(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if !app.ui.regie.result.as_ref().is_some_and(|r| film_is_last_change(app, r)) {
        app.ui.regie.error = tl!("Other changes came after the film. Use Edit ▸ Undo or the History panel.").into();
        return;
    }
    match crate::menus::invoke(app, ctx, "edit.undo", json!({})) {
        Ok(_) => {
            if let Some(r) = app.ui.regie.result.as_mut() {
                r.undone = true;
            }
            app.ui.regie.error.clear();
            app.ui.status = tl!("The film was taken back.").into();
        }
        Err(e) => {
            log::warn!("AI Director: `edit.undo` failed: {e}");
            app.ui.regie.error = tlf!("Something went wrong: {e}", e);
        }
    }
}

enum Action {
    Understand,
    Make,
    Cancel,
    Play,
    Undo,
    Reel,
    AddFolder,
}

/// What the result section may offer for the last film.
struct Film {
    /// Its sequence is still in the project (Play).
    present: bool,
    /// It is the last change (the panel's Undo).
    last: bool,
    /// What it was cut from is known (Reel version).
    can_vary: bool,
}

/// Reel version: `regie.variant` (9:16, 30 seconds) from what the last film was cut from, shown
/// like a new film. It needs no search or analysis, so it runs at once.
fn reel(app: &mut FilmcraftApp) {
    match app.session.execute("regie.variant", json!({"aspect": "9:16", "seconds": 30})) {
        Ok(v) => match RegieResult::from_json(&v) {
            Some(r) => finish(app, r),
            None => app.ui.regie.error = tl!("The AI Director stopped without making a film.").into(),
        },
        Err(e) => fail(app, "regie.variant", &e),
    }
}

/// Register `r` for automation, with the part of it the scroll area shows (what a click can
/// reach).
fn reg(auto: &mut Registry, ui: &egui::Ui, id: &str, r: &egui::Response, label: &str) {
    auto.add_response(id, r, label);
    let shown = r.rect.intersect(ui.clip_rect());
    if let Some(e) = auto.elements.last_mut() {
        e.rect = if shown.is_positive() { [shown.min.x, shown.min.y, shown.width(), shown.height()] } else { [r.rect.min.x, r.rect.min.y, 0.0, 0.0] };
    }
}

/// An icon followed by text that wraps beside it.
fn icon_line(ui: &mut egui::Ui, icon: Icon, icon_color: Color32, text: RichText) -> egui::Response {
    let width = ui.available_width();
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        icons::paint(ui.painter(), r.shrink(1.0), icon, icon_color);
        ui.vertical(|ui| {
            ui.set_width((width - 26.0).max(40.0));
            ui.add(egui::Label::new(text).wrap());
        });
    })
    .response
}

fn field_label(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.label(RichText::new(text).color(t.text_dim));
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let can_pick = FileDialog::Folder { at: None }.available(&app.hooks);
    let (film_present, film_last) = app
        .ui
        .regie
        .result
        .as_ref()
        .map_or((false, false), |r| (r.sequence.is_some_and(|s| app.session.project.sequence(ItemId(s)).is_some()), film_is_last_change(app, r)));
    // another version needs what the last film was cut from (kept by the session)
    let can_vary = app.session.regie.material.is_some();
    let mut actions: Vec<Action> = Vec::new();
    let auto = &mut app.auto;
    let d = &mut app.ui.regie;
    let running = d.running();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(10.0, 8.0))).id_salt("regie-body"));
    egui::ScrollArea::vertical().id_salt("regie-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        ui.set_max_width((ui.available_width() - 6.0).max(0.0));
        icon_line(
            ui,
            Icon::Director,
            t.accent,
            RichText::new(tl!("Say what film you want. I'll build it from your own media — every cut stays editable.")).color(t.text),
        );
        ui.add_space(6.0);
        request_box(ui, d, auto, running);
        ui.add_space(8.0);
        folders(ui, d, auto, &t, running, can_pick, &mut actions);
        ui.add_space(8.0);
        buttons(ui, d, auto, &t, running, &mut actions);
        if !d.error.is_empty() {
            ui.add_space(6.0);
            let r = icon_line(ui, Icon::Info, t.danger, RichText::new(d.error.as_str()).color(t.danger));
            reg(auto, ui, "regie.error", &r, &d.error);
        }
        if running {
            ui.add_space(8.0);
            progress(ui, d, auto, &t, &mut actions);
        } else if d.result.is_some() {
            ui.add_space(8.0);
            result(ui, d, auto, &t, Film { present: film_present, last: film_last, can_vary }, &mut actions);
        }
        ui.add_space(8.0);
        plan_section(ui, d, auto, &t, running);
    });
    let ctx = ui.ctx().clone();
    for a in actions {
        match a {
            Action::Understand => understand(app),
            Action::Make => make(app, &ctx),
            Action::Cancel => cancel(app),
            Action::Play => play(app, &ctx),
            Action::Undo => undo(app, &ctx),
            Action::Reel => reel(app),
            Action::AddFolder => app.pick_ui(FileDialog::Folder { at: None }, |app, paths| {
                for p in paths {
                    app.ui.regie.add_folder(&p);
                }
            }),
        }
    }
}

/// The wish: a text box and the example chips that fill it.
fn request_box(ui: &mut egui::Ui, d: &mut RegieUi, auto: &mut Registry, running: bool) {
    let edit = egui::TextEdit::multiline(&mut d.request)
        .id_salt("regie-request")
        .hint_text(tl!("Describe your film: what, how long, which format, which mood"))
        .desired_width(f32::INFINITY)
        .desired_rows(3)
        .char_limit(MAX_REQUEST_CHARS);
    let r = ui.add_enabled(!running, edit);
    let label: String = d.request.chars().take(200).collect();
    reg(auto, ui, "regie.request", &r, &label);
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (i, text) in examples().into_iter().enumerate() {
            let r = ui.add_enabled(!running, egui::Button::new(RichText::new(text).size(11.5)).small().corner_radius(9.0));
            reg(auto, ui, &format!("regie.example.{i}"), &r, text);
            if r.clicked() {
                d.request = text.to_string();
                d.error.clear();
            }
        }
    });
}

/// Folders to search: the list with remove buttons, Add folder…, or the defaults (greyed).
fn folders(ui: &mut egui::Ui, d: &mut RegieUi, auto: &mut Registry, t: &Tokens, running: bool, can_pick: bool, actions: &mut Vec<Action>) {
    let title = tl!("Folders to search");
    let (hr, open) = crate::widgets::section_header(ui, egui::Id::new("regie-folders"), title, d.is_open(FOLDERS), t, true);
    reg(auto, ui, "regie.section.folders", &hr, title);
    d.set_open(FOLDERS, open);
    if !open {
        return;
    }
    ui.add_space(2.0);
    if d.folders.is_empty() {
        let text = tl!("Pictures, Videos, Music (the default folders)");
        let r = ui.label(RichText::new(text).color(t.text_faint));
        reg(auto, ui, "regie.folders.default", &r, text);
    }
    let mut remove = None;
    for (i, folder) in d.folders.iter().enumerate() {
        ui.horizontal(|ui| {
            let b = ui.add_enabled(!running, egui::Button::new("×").small()).on_hover_text(tl!("Remove folder"));
            reg(auto, ui, &format!("regie.folder.remove.{i}"), &b, tl!("Remove folder"));
            if b.clicked() {
                remove = Some(i);
            }
            let l = ui.add(egui::Label::new(RichText::new(folder.as_str()).color(t.text)).truncate()).on_hover_text(folder.as_str());
            reg(auto, ui, &format!("regie.folder.{i}"), &l, folder);
        });
    }
    if let Some(i) = remove.filter(|i| *i < d.folders.len()) {
        d.folders.remove(i);
    }
    let add = ui.add_enabled(can_pick && !running, egui::Button::new(tl!("Add folder…")));
    reg(auto, ui, "regie.folder.add", &add, tl!("Add folder…"));
    if add.clicked() {
        actions.push(Action::AddFolder);
    }
}

/// Understand and Make film.
fn buttons(ui: &mut egui::Ui, d: &RegieUi, auto: &mut Registry, t: &Tokens, running: bool, actions: &mut Vec<Action>) {
    let wish = !d.request.trim().is_empty();
    ui.horizontal_wrapped(|ui| {
        let u = ui
            .add_enabled(!running && wish, egui::Button::new(tl!("Understand")).min_size(vec2(96.0, 26.0)))
            .on_disabled_hover_text(tl!("Type what film you want first."));
        reg(auto, ui, "regie.understand", &u, tl!("Understand"));
        if u.clicked() {
            actions.push(Action::Understand);
        }
        let make = egui::Button::new(RichText::new(tl!("Make film")).color(Color32::WHITE).strong()).fill(t.accent).min_size(vec2(110.0, 26.0));
        let m = ui.add_enabled(!running && (wish || d.plan_is_current()), make);
        reg(auto, ui, "regie.make", &m, tl!("Make film"));
        if m.clicked() {
            actions.push(Action::Make);
        }
    });
}

/// While a film is being made: progress, what is happening, Cancel.
fn progress(ui: &mut egui::Ui, d: &mut RegieUi, auto: &mut Registry, t: &Tokens, actions: &mut Vec<Action>) {
    let block = ui
        .vertical(|ui| {
            ui.label(RichText::new(tl!("Making your film…")).color(t.text).strong());
            let status = if d.status.is_empty() { tl!("Starting…").to_string() } else { crate::i18n::t(&d.status).to_string() };
            let bar = ui.add(egui::ProgressBar::new(d.progress.clamp(0.0, 1.0) as f32).show_percentage());
            reg(auto, ui, "regie.progress", &bar, &status);
            ui.add(egui::Label::new(RichText::new(status.as_str()).color(t.text_dim)).wrap());
            let c = ui.add_enabled(!d.cancelling, egui::Button::new(tl!("Cancel")));
            reg(auto, ui, "regie.cancel", &c, tl!("Cancel"));
            if c.clicked() {
                actions.push(Action::Cancel);
            }
        })
        .response;
    if std::mem::take(&mut d.reveal) {
        ui.scroll_to_rect(block.rect, None);
    }
}

/// The last film: what was made, Play, Undo, Reel version, Explain how.
fn result(ui: &mut egui::Ui, d: &mut RegieUi, auto: &mut Registry, t: &Tokens, film: Film, actions: &mut Vec<Action>) {
    let Film { present, last, can_vary } = film;
    let Some(r) = d.result.as_ref() else { return };
    let explain_open = d.is_open(EXPLAIN);
    let mut toggle_explain = false;
    let block = ui
        .vertical(|ui| {
            let heading = if r.undone { tl!("The film was taken back.") } else { tl!("Your film is ready.") };
            let resp = icon_line(ui, Icon::Director, if r.undone { t.text_faint } else { t.accent }, RichText::new(heading).color(t.text).strong());
            let label = std::iter::once(heading.to_string()).chain(r.summary.iter().cloned()).collect::<Vec<_>>().join("\n");
            reg(auto, ui, "regie.result", &resp, &label);
            for line in &r.summary {
                ui.add(egui::Label::new(RichText::new(format!("• {line}")).color(t.text)).wrap());
            }
            let clips = if r.clips == 1 { tl!("1 clip").to_string() } else { tlf!("{n} clips", n = r.clips) };
            let mut facts = vec![tlf!("Length: {time}", time = fmt_length(r.seconds)), clips];
            match r.chapters.len() {
                0 => {}
                1 => facts.push(tl!("1 chapter").to_string()),
                n => facts.push(tlf!("{n} chapters", n)),
            }
            ui.label(RichText::new(facts.join(" · ")).color(t.text_dim));
            let names: Vec<&str> = r.chapters.iter().map(String::as_str).filter(|c| !c.is_empty()).take(12).collect();
            if !names.is_empty() {
                ui.add(egui::Label::new(RichText::new(names.join(" · ")).color(t.text_faint)).wrap());
            }
            if !r.searched.is_empty() {
                let folders = r.searched.iter().take(8).map(String::as_str).collect::<Vec<_>>().join(", ");
                ui.add(egui::Label::new(RichText::new(tlf!("Searched in: {folders}", folders)).color(t.text_faint)).wrap());
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let p = ui.add_enabled(present && !r.undone, egui::Button::new(tl!("Play")));
                reg(auto, ui, "regie.play", &p, tl!("Play"));
                if p.clicked() {
                    actions.push(Action::Play);
                }
                let u = ui
                    .add_enabled(last, egui::Button::new(tl!("Undo")))
                    .on_disabled_hover_text(tl!("Other changes came after the film. Use Edit ▸ Undo or the History panel."));
                reg(auto, ui, "regie.undo", &u, tl!("Undo"));
                if u.clicked() {
                    actions.push(Action::Undo);
                }
                let v = ui
                    .add_enabled(can_vary && !r.undone, egui::Button::new(tl!("Reel version (9:16)")))
                    .on_hover_text(tl!("Cut a 30-second 9:16 reel from the same media and music, without searching again."));
                reg(auto, ui, "regie.reel", &v, tl!("Reel version (9:16)"));
                if v.clicked() {
                    actions.push(Action::Reel);
                }
                let e = ui.add(egui::Button::new(tl!("Explain how")).selected(explain_open));
                reg(auto, ui, "regie.explain", &e, tl!("Explain how"));
                if e.clicked() {
                    toggle_explain = true;
                }
            });
            if explain_open {
                ui.add_space(4.0);
                ui.label(RichText::new(tl!("What I did, and how you can do it by hand:")).color(t.text).strong());
                if r.explain.is_empty() {
                    ui.label(RichText::new(tl!("No steps were reported.")).color(t.text_faint));
                }
                for (i, line) in r.explain.iter().enumerate() {
                    ui.add(egui::Label::new(RichText::new(format!("{}. {line}", i + 1)).color(t.text)).wrap());
                }
            }
        })
        .response;
    if toggle_explain {
        d.set_open(EXPLAIN, !explain_open);
    }
    if std::mem::take(&mut d.reveal) {
        ui.scroll_to_rect(block.rect, None);
    }
}

/// What I understood: the plan's lines, its key fields to change, its open questions.
fn plan_section(ui: &mut egui::Ui, d: &mut RegieUi, auto: &mut Registry, t: &Tokens, running: bool) {
    let title = tl!("What I understood");
    let (hr, open) = crate::widgets::section_header(ui, egui::Id::new("regie-plan"), title, d.is_open(PLAN), t, true);
    reg(auto, ui, "regie.section.plan", &hr, title);
    d.set_open(PLAN, open);
    if !open {
        return;
    }
    let current = d.plan_is_current();
    let Some(plan) = d.plan.as_mut() else {
        ui.add(egui::Label::new(RichText::new(tl!("Press Understand to see what I make of your wish.")).color(t.text_faint)).wrap());
        return;
    };
    if !current {
        ui.add(egui::Label::new(RichText::new(tl!("You changed the wish. Press Understand to update this.")).color(t.text_dim).italics()).wrap());
    }
    if plan.understood.is_empty() {
        ui.add(egui::Label::new(RichText::new(tl!("Nothing specific yet: the settings below are used.")).color(t.text_faint)).wrap());
    }
    for line in plan.understood.iter().take(MAX_LINES) {
        ui.add(egui::Label::new(RichText::new(format!("• {}", clip(line))).color(t.text)).wrap());
    }
    ui.add_space(4.0);
    // the fields take what the labels leave, within reason
    let width = (ui.available_width() - 130.0).clamp(120.0, 240.0);
    ui.add_enabled_ui(!running, |ui| {
        egui::Grid::new("regie-plan-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            plan_fields(ui, plan, auto, t, width);
        });
    });
    for q in plan.questions.iter().take(MAX_LINES) {
        ui.add(egui::Label::new(RichText::new(format!("? {}", clip(q))).color(t.text_faint).italics()).wrap());
    }
}

/// The plan's key fields, as rows of a two-column grid; `width` is a field's.
fn plan_fields(ui: &mut egui::Ui, plan: &mut Plan, auto: &mut Registry, t: &Tokens, width: f32) {
    let uhd = is_uhd(plan.width, plan.height);

    field_label(ui, t, tl!("Aspect ratio"));
    let shown = aspect_label(&plan.aspect);
    let cb = egui::ComboBox::from_id_salt("regie-aspect").selected_text(shown.as_str()).width(width).show_ui(ui, |ui| {
        for (value, key) in ASPECTS {
            let text = aspect_label(value);
            let r = ui.selectable_label(plan.aspect == value, text.as_str());
            reg(auto, ui, &format!("regie.aspect.{key}"), &r, &text);
            if r.clicked() {
                plan.aspect = value.to_string();
                (plan.width, plan.height) = frame_size(value, uhd);
            }
        }
    });
    reg(auto, ui, "regie.aspect", &cb.response, &shown);
    ui.end_row();

    field_label(ui, t, tl!("Resolution"));
    let size_text = |uhd: bool| {
        let (w, h) = frame_size(&plan.aspect, uhd);
        format!("{} {w}×{h}", if uhd { "4K" } else { "HD" })
    };
    let shown = if (plan.width, plan.height) == frame_size(&plan.aspect, uhd) { size_text(uhd) } else { format!("{}×{}", plan.width, plan.height) };
    let options = [("hd", false, size_text(false)), ("uhd", true, size_text(true))];
    let cb = egui::ComboBox::from_id_salt("regie-resolution").selected_text(shown.as_str()).width(width).show_ui(ui, |ui| {
        for (key, big, text) in &options {
            let r = ui.selectable_label(shown == *text, text.as_str());
            reg(auto, ui, &format!("regie.resolution.{key}"), &r, text);
            if r.clicked() {
                (plan.width, plan.height) = frame_size(&plan.aspect, *big);
            }
        }
    });
    reg(auto, ui, "regie.resolution", &cb.response, &shown);
    ui.end_row();

    field_label(ui, t, tl!("Frame rate"));
    let shown = fps_text(plan.fps);
    let cb = egui::ComboBox::from_id_salt("regie-fps").selected_text(shown.as_str()).width(width).show_ui(ui, |ui| {
        for rate in RATES {
            let text = fps_text(f64::from(rate));
            let r = ui.selectable_label((plan.fps - f64::from(rate)).abs() < 1e-6, text.as_str());
            reg(auto, ui, &format!("regie.fps.{rate}"), &r, &text);
            if r.clicked() {
                plan.fps = f64::from(rate);
            }
        }
    });
    reg(auto, ui, "regie.fps", &cb.response, &shown);
    ui.end_row();

    field_label(ui, t, tl!("Length"));
    ui.horizontal(|ui| {
        let mut secs = plan.seconds.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(0.0).min(MAX_SECONDS);
        let r = ui.add(crate::widgets::number_drag(&mut secs).range(0.0..=MAX_SECONDS).speed(1.0).fixed_decimals(0).suffix(" s"));
        reg(auto, ui, "regie.seconds", &r, &format!("{secs:.0}"));
        if r.changed() {
            plan.seconds = (secs.is_finite() && secs > 0.0).then_some(secs);
        }
        let note = if secs > 0.0 { format!("({})", fmt_length(secs)) } else { tl!("0 = automatic").to_string() };
        ui.label(RichText::new(note).color(t.text_faint));
    });
    ui.end_row();

    field_label(ui, t, tl!("Dates"));
    ui.horizontal(|ui| {
        date_field(ui, auto, t, &mut plan.from, "regie.from", "regie-from");
        ui.label(RichText::new("–").color(t.text_dim));
        date_field(ui, auto, t, &mut plan.to, "regie.to", "regie-to");
    });
    ui.end_row();

    field_label(ui, t, tl!("Mood"));
    let shown = mood_label(&plan.mood);
    let cb = egui::ComboBox::from_id_salt("regie-mood").selected_text(shown.as_str()).width(width).show_ui(ui, |ui| {
        for mood in MOODS {
            let text = mood_label(mood);
            let r = ui.selectable_label(plan.mood == mood, text.as_str());
            reg(auto, ui, &format!("regie.mood.{mood}"), &r, &text);
            if r.clicked() {
                plan.mood = mood.to_string();
            }
        }
    });
    reg(auto, ui, "regie.mood", &cb.response, &shown);
    ui.end_row();

    field_label(ui, t, tl!("Title"));
    let mut title = plan.title.clone().unwrap_or_default();
    let r = ui.add(egui::TextEdit::singleline(&mut title).id_salt("regie-title").hint_text(tl!("Made from the content")).desired_width(width).char_limit(200));
    reg(auto, ui, "regie.title", &r, &title);
    if r.changed() {
        plan.title = (!title.is_empty()).then_some(title);
    }
    ui.end_row();
}

/// A date field (`YYYY-MM-DD`; empty: open-ended), red while it doesn't read as a date.
fn date_field(ui: &mut egui::Ui, auto: &mut Registry, t: &Tokens, value: &mut Option<String>, id: &str, salt: &str) {
    let mut text = value.clone().unwrap_or_default();
    let bad = !text.trim().is_empty() && !valid_date(text.trim());
    let edit = egui::TextEdit::singleline(&mut text).id_salt(salt).hint_text(tl!("YYYY-MM-DD")).desired_width(100.0).char_limit(10);
    let r = ui.add(edit.text_color(if bad { t.danger } else { t.text }));
    reg(auto, ui, id, &r, &text);
    if r.changed() {
        *value = (!text.is_empty()).then_some(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_partial_json_takes_defaults() {
        let mut d = RegieUi { request: "Holiday recap".into(), folders: vec!["/media/holiday".into()], ..Default::default() };
        d.plan = Some(Plan { request: "Holiday recap".into(), seconds: Some(180.0), ..Plan::default() });
        d.result = Some(RegieResult { sequence: Some(7), clips: 12, seconds: 181.5, summary: vec!["12 clips".into()], ..Default::default() });
        d.set_open(EXPLAIN, true);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["request"], "Holiday recap");
        assert_eq!(v["openSections"], json!(["folders", "plan", "explain"]));
        assert_eq!(v["plan"]["aspect"], "16:9");
        assert!(v.get("reveal").is_none(), "runtime only");
        let back: RegieUi = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
        // every field is optional
        let partial: RegieUi = serde_json::from_value(json!({"request": "x", "plan": {"aspect": "9:16"}})).unwrap();
        assert_eq!(partial.request, "x");
        assert_eq!(partial.open_sections, RegieUi::default().open_sections);
        let plan = partial.plan.unwrap();
        assert_eq!((plan.aspect.as_str(), plan.width, plan.fps), ("9:16", 1920, 25.0));
        // wrong types are an error, not a panic
        assert!(serde_json::from_value::<RegieUi>(json!({"folders": "not a list"})).is_err());
        assert!(serde_json::from_value::<RegieUi>(json!({"job": -1})).is_err());
    }

    #[test]
    fn sanitized_bounds_what_an_agent_sets() {
        let d = RegieUi {
            request: "x".repeat(MAX_REQUEST_CHARS + 50),
            folders: (0..500).map(|i| format!("/f{}", i % 3)).chain(["  ".into()]).collect(),
            progress: 7.0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(d.request.chars().count(), MAX_REQUEST_CHARS);
        assert_eq!(d.folders, ["/f0", "/f1", "/f2"]);
        assert_eq!(d.progress, 1.0);
        let mut d = RegieUi::default();
        for i in 0..MAX_FOLDERS + 3 {
            d.add_folder(&format!("/m{i}"));
        }
        assert_eq!(d.folders.len(), MAX_FOLDERS);
        assert!(!d.error.is_empty());
        assert!(!d.add_folder("/m0") && !d.add_folder("   "));
    }

    #[test]
    fn sizes_follow_the_aspect() {
        assert_eq!(frame_size("16:9", false), (1920, 1080));
        assert_eq!(frame_size("16:9", true), (3840, 2160));
        assert_eq!(frame_size("9:16", true), (2160, 3840));
        assert_eq!(frame_size("1:1", false), (1080, 1080));
        assert_eq!(frame_size("4:5", true), (2160, 2700));
        assert_eq!(frame_size("21:9", false), (1920, 1080));
        assert!(is_uhd(3840, 2160) && is_uhd(2160, 3840) && !is_uhd(1920, 1080) && !is_uhd(0, 0) && !is_uhd(u32::MAX, 1080));
    }

    #[test]
    fn dates_lengths_and_rates_read_safely() {
        for ok in ["2026-01-01", "2024-02-29", "2026-12-31"] {
            assert!(valid_date(ok), "{ok}");
        }
        for bad in ["", "2026-1-01", "2025-02-29", "2026-13-01", "2026-04-31", "2026-00-10", "2026/01/01", "abcd-ef-gh", "2026-01-0ä", "２０２６-01-01"] {
            assert!(!valid_date(bad), "{bad}");
        }
        assert_eq!(fmt_length(185.4), "3:05");
        assert_eq!(fmt_length(3723.0), "1:02:03");
        assert_eq!(fmt_length(f64::NAN), "–");
        assert_eq!(fmt_length(-1.0), "–");
        assert!(!fmt_length(f64::MAX).is_empty());
        assert_eq!(fps_text(25.0), "25 fps");
        assert_eq!(fps_text(29.97), "29.97 fps");
        assert_eq!(fps_text(f64::INFINITY), "–");
        assert_eq!(fps_text(0.0), "–");
    }

    #[test]
    fn results_are_read_leniently() {
        let r = RegieResult::from_json(&json!({
            "sequence": 42, "clips": 17, "seconds": 183.2,
            "chapters": ["Mykonos · September", {"name": "Kreta · Oktober"}, 5],
            "summary": ["A recap", 3, null], "explain": ["File ▸ New ▸ Sequence…"], "folders": ["/home/me/Pictures"]
        }))
        .unwrap();
        assert_eq!(r.sequence, Some(42));
        assert_eq!(r.clips, 17);
        assert_eq!(r.chapters, ["Mykonos · September", "Kreta · Oktober", ""]);
        assert_eq!(r.summary, ["A recap"]);
        assert_eq!(r.explain.len(), 1);
        assert_eq!(r.searched, ["/home/me/Pictures"]);
        // hostile shapes and sizes
        let r = RegieResult::from_json(&json!({"sequence": -3, "clips": [1, 2], "seconds": -5, "summary": vec!["x".repeat(5000); 1000]})).unwrap();
        assert_eq!((r.sequence, r.clips, r.seconds), (None, 2, 0.0));
        assert_eq!(r.summary.len(), MAX_LINES);
        assert_eq!(r.summary[0].chars().count(), MAX_LINE_CHARS);
        assert!(RegieResult::from_json(&json!(null)).is_none());
        assert!(RegieResult::from_json(&json!([1])).is_none());
    }

    #[test]
    fn make_film_sends_the_current_plan_or_the_wish() {
        let mut d = RegieUi { request: "  Holiday recap  ".into(), ..Default::default() };
        // no plan yet: the wish
        assert_eq!(run_params(&d).unwrap(), json!({"request": "Holiday recap", "wait": false}));
        // a plan of this wish, edited in the panel, with the folders
        d.folders = vec!["/media/holiday".into()];
        d.plan = Some(Plan {
            request: "Holiday recap".into(),
            aspect: "9:16".into(),
            from: Some(" 2026-06-01 ".into()),
            title: Some("  ".into()),
            ..Plan::default()
        });
        let p = run_params(&d).unwrap();
        assert_eq!(p["wait"], false);
        assert_eq!(p["folders"], json!(["/media/holiday"]));
        assert!(p.get("request").is_none());
        assert_eq!(p["plan"]["aspect"], "9:16");
        assert_eq!(p["plan"]["from"], "2026-06-01");
        assert_eq!(p["plan"]["title"], Value::Null);
        assert_eq!(p["plan"]["folders"], json!(["/media/holiday"]));
        // a date that isn't one is refused before anything runs
        if let Some(plan) = d.plan.as_mut() {
            plan.to = Some("2026-02-30".into());
        }
        assert!(run_params(&d).unwrap_err().contains("2026-02-30"));
        // the wish changed: the stale plan is not sent
        d.request = "Birthday slideshow".into();
        assert!(!d.plan_is_current());
        assert_eq!(run_params(&d).unwrap()["request"], "Birthday slideshow");
        d.request = "   ".into();
        assert!(run_params(&d).is_err());
    }

    #[test]
    fn errors_read_as_friendly_lines() {
        assert!(friendly(&EngineError::UnknownCommand("regie.run".into())).contains("can't do this yet"));
        assert!(friendly(&EngineError::Disabled("regie.run".into(), "no project".into())).contains("no project"));
        assert!(friendly(&EngineError::Other("disk full".into())).contains("disk full"));
    }

    #[test]
    fn sections_open_and_close() {
        let mut d = RegieUi::default();
        assert!(d.is_open(FOLDERS) && d.is_open(PLAN) && !d.is_open(EXPLAIN));
        d.set_open(PLAN, false);
        d.set_open(EXPLAIN, true);
        d.set_open(EXPLAIN, true);
        assert_eq!(d.open_sections, ["folders", "explain"]);
    }
}
