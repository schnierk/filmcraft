//! `audio.beats` and `media.highlights`: the beat grid of the music and the best moments of each
//! video (see [the module overview](super)). Both commands are queries; their work is done by the
//! pure functions [`beats_from_mono`] and [`highlights`], which a background job can call directly.
//!
//! ## Beats (`audio.beats`, [`beats_from_mono`])
//!
//! The music is read as mono (22.05 kHz; 11.025 kHz beyond ten minutes; at most
//! [`BEATS_MAX_SECONDS`]), averaged down to at most 24 kHz, and handed to the beat tracker of
//! Clip ▸ Remix ([`filmcraft_audio_dsp::remix::beat_grid`]: log-magnitude spectral-flux onsets,
//! tempo from the autocorrelation of the onset envelope with a prior at 120 BPM, beats by dynamic
//! programming after Ellis, "Beat Tracking by Dynamic Programming", JNMR 2007). Then:
//!
//! - **bars** (4/4): of the four possible bar phases, the one whose beats carry the most onset
//!   strength in the low band (≤ 200 Hz: kick drum, bass notes) starts the bars;
//! - **energy** per beat: the RMS from the beat to the next one, smoothed over five beats (weights
//!   1 2 3 2 1), divided by the 95th percentile (1 = the loud parts) and clamped to 0..1;
//! - **sections**: bars whose mean energy is below [`CALM_BELOW`] are calm, the others lively;
//!   music before the first or after the last beat is a section of its own when it lasts two bars
//!   or more (else part of its neighbour); a run shorter than two bars joins the neighbour closer in
//!   energy. The sections cover the whole piece without gaps, at bar starts.
//!
//! Silence, noise, speech, very short audio or an unusable sample rate give no beats (`bpm` 0)
//! and one calm section over the whole piece. Times are seconds from the start of the item (a
//! subclip's In point), rounded to milliseconds.
//!
//! ## Highlights (`media.highlights`, [`highlights`])
//!
//! One frame every `step` seconds (at most [`HIGHLIGHT_MAX_SAMPLES`] frames; longer media are
//! sampled further apart) is decoded at about 256 pixels on its longest side. Per frame, on luma
//! (Rec. 709 weights on the display-encoded RGB, 0..1):
//!
//! | measure | how | score 0..1 |
//! |---|---|---|
//! | sharpness | variance of the 4-neighbour Laplacian (Pech-Pacheco et al., ICPR 2000) | log scale: 0 at 10^−4.3, 1 at 10^−2.8 |
//! | exposure | mean and standard deviation of luma | 1 at a mean of 0.45, 0 at 0.12 or 0.87; × (0.4 + 0.6 × contrast, full at σ 0.16) |
//! | motion | mean absolute luma difference to the previous frame | some is lively (best near 0.06), a still picture or chaos score low |
//! | steadiness | global shift to the previous frame (block matching, quarter then half size, sub-pixel); shake = how much that shift changes from one frame pair to the next; when no single shift explains the change (parallax, water, crowds) there is no evidence of shake | 1 up to 1 % of the picture, 0 from 4 % |
//! | hard cuts | colour-histogram and luma-grid jumps ([`filmcraft_render::scene`]) | no segment spans one |
//!
//! A frame scores 0.30 sharpness + 0.25 exposure + 0.25 steadiness + 0.20 motion; a window of
//! `segmentSeconds` scores 0.7 × the mean + 0.3 × the minimum of its frames, so one bad moment
//! costs. The best windows are taken greedily, each at least one step away from the others, and
//! returned in media order. Media shorter than one window is one candidate as a whole; when cuts
//! leave no room for a full window, whole shots are the candidates. Times are media time in
//! seconds (a subclip's frames keep the times of its parent).
//!
//! Reasons (window means): "sharp" (sharpness ≥ 0.6), "steady" (≥ 0.75), "lively" (motion score
//! ≥ 0.5), "well exposed" (exposure ≥ 0.6, mean luma 0.2…0.75); "dark" (mean luma < 0.2),
//! "overexposed" (> 0.75), "blurry" (sharpness ≤ 0.3), "shaky" (steadiness ≤ 0.45).

use std::sync::atomic::{AtomicBool, Ordering};

use filmcraft_audio_dsp::remix as dsp;
use filmcraft_frame::{PixelData, VideoFrame, linear_premul_to_srgb8};
use filmcraft_media::{FrameRequest, MediaSource};
use filmcraft_project::ItemId;
use filmcraft_render::scene::{self, SceneOptions};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, item_p};
use crate::{EngineError, Result, Session};

use super::types::{Beats, Highlights, Section, Segment};

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "audio.beats",
            label: "AI Director: Music Beats",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id}"#,
            enabled: always,
            run: run_beats,
            journal: false,
        },
        CommandSpec {
            id: "media.highlights",
            label: "AI Director: Best Moments",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"segmentSeconds":f64=3,"maxSegments":n=4,"step":f64=0.5}"#,
            enabled: always,
            run: run_highlights,
            journal: false,
        },
    ]
}

/// The `item` parameter.
fn item_param(p: &Value, cmd: &str) -> Result<ItemId> {
    item_p(p, "item").ok_or_else(|| bad(cmd, "`item` (the id of a project item) is required"))
}

/// An optional number: absent or null is `None`, anything else but a number is an error.
fn number_param(p: &Value, key: &str, cmd: &str) -> Result<Option<f64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().map(Some).ok_or_else(|| bad(cmd, format!("`{key}` must be a number"))),
    }
}

/// The media range an item shows: a subclip's range, else the whole media (media time).
fn item_range(s: &Session, item: ItemId, cmd: &str) -> Result<(filmcraft_media::SharedSource, TimeRange)> {
    let (_, _, sub) = s.project.resolve_media(item).ok_or_else(|| bad(cmd, format!("item {} is not a media item", item.0)))?;
    let src = s.source(item).ok_or_else(|| EngineError::Other(format!("the media of item {} is not available", item.0)))?;
    let range = sub.unwrap_or_else(|| TimeRange::new(Tick::ZERO, src.info().duration));
    Ok((src, range))
}

fn finite(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// Rounded to thousandths (milliseconds for times).
fn milli(v: f64) -> f64 {
    finite((v * 1000.0).round() / 1000.0)
}

fn mean(x: &[f64]) -> f64 {
    if x.is_empty() { 0.0 } else { x.iter().sum::<f64>() / x.len() as f64 }
}

fn smoothstep(a: f64, b: f64, x: f64) -> f64 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    if t.is_finite() { t * t * (3.0 - 2.0 * t) } else { 0.0 }
}

// ------------------------------------------------------------------------------------------ beats

/// Most music `audio.beats` analyses (seconds); a longer file is analysed up to here.
pub const BEATS_MAX_SECONDS: f64 = 1200.0;
/// Energy (0..1) below which music counts as calm.
pub const CALM_BELOW: f64 = 0.45;
/// Audio faster than this (Hz) is averaged down first: the beat tracker needs no more.
const WORK_RATE: u32 = 24_000;
/// Rate `audio.beats` reads music at (Hz); half of it for music longer than ten minutes.
pub(crate) const READ_RATE: u32 = 22_050;

/// The beat grid of mono audio at `rate` Hz (see the module documentation). Never fails: silence,
/// noise, very short audio or an unusable rate give no beats (`bpm` 0) and one calm section over
/// the audio; empty audio or a zero rate give an empty grid. At most [`BEATS_MAX_SECONDS`] are
/// analysed (`seconds` says how much). Times are seconds from the first sample.
pub fn beats_from_mono(samples: &[f32], rate: u32) -> Beats {
    beats_within(samples, rate, BEATS_MAX_SECONDS)
}

/// [`beats_from_mono`] analysing at most `max_seconds`.
fn beats_within(samples: &[f32], rate: u32, max_seconds: f64) -> Beats {
    if rate == 0 || samples.is_empty() {
        return Beats::default();
    }
    let max_len = (max_seconds.max(0.0) * rate as f64) as usize;
    let input = samples.get(..samples.len().min(max_len)).unwrap_or(samples);
    let seconds = input.len() as f64 / rate as f64;
    // averaged down by `d`, damaged samples silenced
    let d = rate.div_ceil(WORK_RATE).max(1);
    let work: Vec<f32> = input.chunks(d as usize).map(|c| c.iter().map(|&v| audible(v)).sum::<f32>() / c.len().max(1) as f32).collect();
    let wrate = rate as f64 / d as f64;
    match dsp::beat_grid(&work, rate / d) {
        Ok(g) => grid_beats(&g, &work, wrate, seconds),
        Err(_) => without_beats(seconds),
    }
}

/// A sample as the analysis uses it: NaN and infinities are silence, the rest is limited to ±16.
fn audible(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-16.0, 16.0) } else { 0.0 }
}

fn without_beats(seconds: f64) -> Beats {
    let end = milli(seconds);
    Beats { seconds: end, sections: vec![Section { kind: "calm".into(), start: 0.0, end }], ..Beats::default() }
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt()
}

/// Centred smoothing over five values (weights 1 2 3 2 1; fewer at the ends).
fn smooth5(x: &[f64]) -> Vec<f64> {
    const W: [f64; 5] = [1.0, 2.0, 3.0, 2.0, 1.0];
    (0..x.len())
        .map(|i| {
            let (mut s, mut ws) = (0.0, 0.0);
            for (k, w) in W.iter().enumerate() {
                if let Some(v) = (i + k).checked_sub(2).and_then(|j| x.get(j)) {
                    s += w * v;
                    ws += w;
                }
            }
            if ws > 0.0 { s / ws } else { 0.0 }
        })
        .collect()
}

/// The `q` quantile (0..1) of the finite values, nearest rank; 0 for none.
fn quantile(x: &[f64], q: f64) -> f64 {
    let mut v: Vec<f64> = x.iter().copied().filter(|v| v.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    let i = (v.len().saturating_sub(1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    v.get(i).copied().unwrap_or(0.0)
}

/// The bar phase (0..4) whose beats carry the most low-band onset strength (ties: the earlier).
fn downbeat_phase(low: &[f32]) -> usize {
    let mut best = (0, f64::NEG_INFINITY);
    for p in 0..4.min(low.len()) {
        let v: Vec<f64> = low.iter().skip(p).step_by(4).map(|&s| f64::from(s)).filter(|s| s.is_finite()).collect();
        let m = mean(&v);
        if m > best.1 {
            best = (p, m);
        }
    }
    best.0
}

fn grid_beats(g: &dsp::BeatGrid, work: &[f32], wrate: f64, seconds: f64) -> Beats {
    let n = g.beats.len();
    let period = g.period / wrate;
    if n < 4 || !period.is_finite() || period <= 0.0 {
        return without_beats(seconds);
    }
    let beat_t: Vec<f64> = g.beats.iter().map(|&b| (b as f64 / wrate).clamp(0.0, seconds)).collect();
    // loudness from each beat to the next (the last one: a period)
    let idx = |b: i64| usize::try_from(b).unwrap_or(0).min(work.len());
    let period_w = g.period.max(1.0).min(work.len().max(1) as f64) as usize;
    let loud: Vec<f64> = g
        .beats
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            let a = idx(b);
            let e = g.beats.get(i + 1).map_or_else(|| a.saturating_add(period_w).min(work.len()), |&b| idx(b));
            rms(work.get(a..e.max(a)).unwrap_or(&[]))
        })
        .collect();
    let smoothed = smooth5(&loud);
    let reference = quantile(&smoothed, 0.95);
    let norm = |v: f64| if reference > 1e-9 { (v / reference).clamp(0.0, 1.0) } else { 0.0 };
    let energy: Vec<f64> = smoothed.iter().map(|&v| norm(v)).collect();
    let phase = downbeat_phase(&g.low);
    let bars: Vec<f64> = beat_t.iter().skip(phase).step_by(4).copied().collect();
    let region = |a: f64, b: f64| {
        let (i, j) = (((a * wrate) as usize).min(work.len()), ((b * wrate) as usize).min(work.len()));
        norm(rms(work.get(i..j.max(i)).unwrap_or(&[])))
    };
    let units = bar_units(&beat_t, &energy, phase, period, seconds, &region);
    Beats {
        seconds: milli(seconds),
        bpm: finite((60.0 / period * 100.0).round() / 100.0),
        beats: beat_t.iter().map(|&t| milli(t)).collect(),
        bars: bars.iter().map(|&t| milli(t)).collect(),
        energy: energy.iter().map(|&e| milli(e)).collect(),
        sections: sections_of(&units, 8.0 * period),
    }
}

/// A bar, or beatless music before the first bar or after the last, with its energy.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Unit {
    start: f64,
    end: f64,
    energy: f64,
}

/// The bars of the beat grid (bar starts every four beats from `phase`), from 0 to `seconds`.
fn bar_units(beat_t: &[f64], energy: &[f64], phase: usize, period: f64, seconds: f64, region: &dyn Fn(f64, f64) -> f64) -> Vec<Unit> {
    let n = beat_t.len();
    let two_bars = 8.0 * period;
    let starts: Vec<usize> = (phase..n).step_by(4).collect();
    let mut units: Vec<Unit> = Vec::with_capacity(starts.len() + 2);
    for (j, &i0) in starts.iter().enumerate() {
        let i1 = starts.get(j + 1).copied().unwrap_or(n);
        let Some(&start) = beat_t.get(i0) else { continue };
        let end = match beat_t.get(i1) {
            Some(&t) => t,
            None => beat_t.last().map_or(start, |&t| (t + period).min(seconds)).max(start),
        };
        units.push(Unit { start, end, energy: energy.get(i0..i1).map_or(0.0, mean) });
    }
    // before the first bar (a pickup, or an intro without beats)
    match units.first().map(|u| u.start) {
        Some(s) if s >= two_bars => units.insert(0, Unit { start: 0.0, end: s, energy: region(0.0, s) }),
        Some(_) => {
            if let Some(u) = units.first_mut() {
                u.start = 0.0;
            }
        }
        None => units.push(Unit { start: 0.0, end: seconds, energy: region(0.0, seconds) }),
    }
    // after the last bar
    match units.last().map(|u| u.end) {
        Some(e) if seconds - e >= two_bars => units.push(Unit { start: e, end: seconds, energy: region(e, seconds) }),
        _ => {
            if let Some(u) = units.last_mut() {
                u.end = seconds;
            }
        }
    }
    units
}

/// A run of units of one kind.
#[derive(Clone, Copy, Debug)]
struct Run {
    start: f64,
    end: f64,
    calm: bool,
    /// Energy × duration, summed.
    weight: f64,
}

impl Run {
    fn energy(&self) -> f64 {
        let d = self.end - self.start;
        if d > 0.0 { self.weight / d } else { 0.0 }
    }
}

/// Join neighbouring runs of the same kind.
fn coalesce(runs: Vec<Run>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(runs.len());
    for r in runs {
        match out.last_mut() {
            Some(l) if l.calm == r.calm => {
                l.end = r.end;
                l.weight += r.weight;
            }
            _ => out.push(r),
        }
    }
    out
}

/// Calm and lively sections of at least `min_len` seconds (unless the music is shorter) from the
/// units, contiguous and in order.
fn sections_of(units: &[Unit], min_len: f64) -> Vec<Section> {
    let mut runs = coalesce(
        units.iter().map(|u| Run { start: u.start, end: u.end, calm: u.energy < CALM_BELOW, weight: u.energy * (u.end - u.start).max(0.0) }).collect(),
    );
    // the shortest run that is too short joins the neighbour closer in energy, until none is left
    while runs.len() > 1 {
        let short = runs.iter().enumerate().filter(|(_, r)| r.end - r.start < min_len).min_by(|a, b| (a.1.end - a.1.start).total_cmp(&(b.1.end - b.1.start)));
        let Some((i, r)) = short.map(|(i, r)| (i, *r)) else { break };
        let before = i.checked_sub(1).and_then(|j| runs.get(j)).map(Run::energy);
        let after = runs.get(i + 1).map(Run::energy);
        let into = match (before, after) {
            (Some(b), Some(a)) => {
                if (b - r.energy()).abs() <= (a - r.energy()).abs() {
                    i - 1
                } else {
                    i + 1
                }
            }
            (Some(_), None) => i - 1,
            (None, Some(_)) => i + 1,
            (None, None) => break,
        };
        if let Some(t) = runs.get_mut(into) {
            t.start = t.start.min(r.start);
            t.end = t.end.max(r.end);
            t.weight += r.weight;
        }
        runs.remove(i);
        runs = coalesce(runs);
    }
    runs.iter().map(|r| Section { kind: if r.calm { "calm" } else { "lively" }.into(), start: milli(r.start), end: milli(r.end) }).collect()
}

/// The item's audio (its range, at most [`BEATS_MAX_SECONDS`]) as mono at `rate`.
pub(crate) fn read_mono(src: &dyn MediaSource, range: TimeRange, rate: u32) -> std::result::Result<Vec<f32>, String> {
    let secs = range.duration.seconds().clamp(0.0, BEATS_MAX_SECONDS);
    let total = (secs * rate as f64) as usize;
    let start = range.start.max(Tick::ZERO).to_units_floor(i64::from(rate));
    let chunk = (rate as usize).saturating_mul(10).max(1);
    let mut mono = Vec::with_capacity(total);
    while mono.len() < total {
        let n = chunk.min(total - mono.len());
        let at = start.saturating_add(i64::try_from(mono.len()).unwrap_or(i64::MAX));
        let buf = src.audio(at, n, rate).map_err(|e| format!("reading the audio: {e}"))?;
        let k = 1.0 / buf.channels.len().max(1) as f32;
        mono.extend((0..n).map(|i| buf.channels.iter().map(|c| c.get(i).copied().unwrap_or(0.0)).sum::<f32>() * k));
    }
    Ok(mono)
}

/// `audio.beats {item}`: the beat grid of an item's audio.
fn run_beats(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "audio.beats";
    let item = item_param(p, CMD)?;
    let (src, range) = item_range(s, item, CMD)?;
    if !src.info().has_audio() {
        return Err(bad(CMD, format!("item {} has no audio", item.0)));
    }
    let rate = if range.duration.seconds() > 600.0 { READ_RATE / 2 } else { READ_RATE };
    let mono = read_mono(src.as_ref(), range, rate).map_err(EngineError::Other)?;
    Ok(json!(beats_from_mono(&mono, rate)))
}

// ------------------------------------------------------------------------------------- highlights

/// Most frames [`highlights`] decodes; longer media are sampled further apart.
pub const HIGHLIGHT_MAX_SAMPLES: usize = 2400;
/// Longest side (pixels) of the pictures analysed.
const SIDE: usize = 256;
/// Supported `step` (seconds); values outside are clamped.
const STEP_RANGE: (f64, f64) = (0.04, 60.0);
/// Supported `segmentSeconds`; values outside are clamped.
const SEGMENT_RANGE: (f64, f64) = (0.1, 600.0);
/// Most segments returned.
const MAX_SEGMENTS: usize = 100;
/// Below this luma standard deviation a picture has too little texture to find its shift.
const MIN_TEXTURE: f64 = 0.02;
/// Shake counted when the picture moved further than the shift search reaches.
const FAST_SHAKE: f64 = 0.025;
/// Shake (change of the frame-to-frame shift, fraction of the longest side) below which footage
/// is steady and from which it is shaky.
const SHAKE: (f64, f64) = (0.01, 0.04);
/// A shift is trusted when its match costs at most this share of the average over all shifts.
const MATCH_QUALITY: f64 = 0.45;
/// Motion (mean absolute luma difference between frames) that looks liveliest.
const LIVELY: f64 = 0.06;
/// Mid-grey luma, the best mean for exposure.
const MID_GREY: f64 = 0.45;
/// log10 of the Laplacian variance where the sharpness score starts and is full.
const SHARP_LOG: (f64, f64) = (-4.3, -2.8);

/// Settings of [`highlights`] (the `media.highlights` parameters).
#[derive(Clone, Debug, PartialEq)]
pub struct HighlightOptions {
    /// Length of a segment, seconds (0.1 … 600).
    pub segment_seconds: f64,
    /// Most segments returned (1 … 100).
    pub max_segments: usize,
    /// Seconds between the frames analysed (0.04 … 60; raised when the media would need more than
    /// [`HIGHLIGHT_MAX_SAMPLES`] frames).
    pub step: f64,
    /// Media range to analyse (media time); `None`: the whole media.
    pub range: Option<TimeRange>,
}

impl Default for HighlightOptions {
    fn default() -> Self {
        HighlightOptions { segment_seconds: 3.0, max_segments: 4, step: 0.5, range: None }
    }
}

impl HighlightOptions {
    /// The options with non-finite or non-positive seconds and zero segments refused and the rest
    /// clamped to the supported ranges.
    pub fn checked(&self) -> std::result::Result<HighlightOptions, String> {
        if !self.step.is_finite() || self.step <= 0.0 {
            return Err("`step` must be a positive number of seconds".into());
        }
        if !self.segment_seconds.is_finite() || self.segment_seconds <= 0.0 {
            return Err("`segmentSeconds` must be a positive number of seconds".into());
        }
        if self.max_segments == 0 {
            return Err("`maxSegments` must be at least 1".into());
        }
        Ok(HighlightOptions {
            segment_seconds: self.segment_seconds.clamp(SEGMENT_RANGE.0, SEGMENT_RANGE.1),
            max_segments: self.max_segments.min(MAX_SEGMENTS),
            step: self.step.clamp(STEP_RANGE.0, STEP_RANGE.1),
            range: self.range,
        })
    }
}

/// The best moments of a video (see the module documentation). Decodes one frame per step from
/// `src`, checks `cancel` before each (an error "cancelled" when set) and reports
/// `progress(frames done, frames)` after each. Errors on bad options, media without picture or
/// length, and frames that cannot be decoded. Safe to call from any thread.
pub fn highlights(src: &dyn MediaSource, opts: &HighlightOptions, cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> std::result::Result<Highlights, String> {
    let o = opts.checked()?;
    let info = src.info();
    let video = info.video.as_ref().ok_or("the media has no video")?;
    let whole = TimeRange::new(Tick::ZERO, info.duration);
    if whole.is_empty() {
        return Err("the media has no length".into());
    }
    let range = match o.range {
        Some(r) => r.intersect(&whole).ok_or("the range to analyse lies outside the media")?,
        None => whole,
    };
    let dur = range.duration.0.max(1);
    let max = HIGHLIGHT_MAX_SAMPLES as i64;
    let mut step = Tick::from_seconds_f64(o.step).0.max(1);
    if (dur - 1) / step + 1 > max {
        step = (dur - 1) / max + 1;
    }
    let n = usize::try_from((dur - 1) / step + 1).unwrap_or(HIGHLIGHT_MAX_SAMPLES).min(HIGHLIGHT_MAX_SAMPLES);
    let scale = (SIDE as f32 / video.width.max(video.height).max(1) as f32).min(1.0);
    let mut looks = Vec::with_capacity(n);
    let mut prev: Option<Thumb> = None;
    for k in 0..n {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let t = range.start + Tick(step.saturating_mul(k as i64));
        let frame = src.video_frame(FrameRequest { time: t, scale }).map_err(|e| format!("can't decode the frame at {:.2} s: {e}", t.seconds()))?;
        let thumb = Thumb::of(&frame).ok_or_else(|| format!("the frame at {:.2} s has no picture", t.seconds()))?;
        looks.push(thumb.look(prev.as_ref()));
        prev = Some(thumb);
        progress(k as u64 + 1, n as u64);
    }
    Ok(pick(&looks, range, Tick(step).seconds(), &o))
}

/// A luma picture.
#[derive(Clone, Debug, Default, PartialEq)]
struct Plane {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

impl Plane {
    fn new(w: usize, h: usize, px: Vec<f32>) -> Option<Plane> {
        (w > 0 && h > 0 && Some(px.len()) == w.checked_mul(h)).then_some(Plane { w, h, px })
    }

    fn row(&self, y: usize) -> &[f32] {
        self.px.get(y * self.w..(y + 1) * self.w).unwrap_or(&[])
    }

    fn same_size(&self, o: &Plane) -> bool {
        (self.w, self.h) == (o.w, o.h)
    }

    /// Half the size (2 × 2 means; at least one pixel).
    fn half(&self) -> Plane {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            let (a, b) = (self.row(2 * y), self.row((2 * y + 1).min(self.h.saturating_sub(1))));
            for x in 0..w {
                let (x0, x1) = (2 * x, (2 * x + 1).min(self.w.saturating_sub(1)));
                let at = |r: &[f32], i: usize| r.get(i).copied().unwrap_or(0.0);
                px.push(0.25 * (at(a, x0) + at(a, x1) + at(b, x0) + at(b, x1)));
            }
        }
        Plane { w, h, px }
    }

    fn mean_sd(&self) -> (f64, f64) {
        let n = self.px.len().max(1) as f64;
        let m = self.px.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
        let var = self.px.iter().map(|&v| (f64::from(v) - m).powi(2)).sum::<f64>() / n;
        (m, var.sqrt())
    }

    /// The picture minus its mean (matching then ignores a change of brightness).
    fn centred(&self) -> Plane {
        let m = self.mean_sd().0 as f32;
        Plane { w: self.w, h: self.h, px: self.px.iter().map(|v| v - m).collect() }
    }
}

/// Variance of the 4-neighbour Laplacian over the inner pixels: small for blurred or flat pictures.
fn laplacian_var(p: &Plane) -> f64 {
    if p.w < 3 || p.h < 3 {
        return 0.0;
    }
    let (mut s, mut s2, mut n) = (0f64, 0f64, 0f64);
    for y in 1..p.h - 1 {
        let (up, row, dn) = (p.row(y - 1), p.row(y), p.row(y + 1));
        for ((win, u), d) in row.windows(3).zip(up.iter().skip(1)).zip(dn.iter().skip(1)) {
            let &[l, c, r] = win else { continue };
            let lap = f64::from(4.0 * c - l - r - u - d);
            s += lap;
            s2 += lap * lap;
            n += 1.0;
        }
    }
    if n > 0.0 { (s2 / n - (s / n).powi(2)).max(0.0) } else { 0.0 }
}

/// Mean absolute difference of `b` moved by (dx, dy) against `a` (b(x + dx, y + dy) − a(x, y))
/// where both exist; `None` without overlap or for different sizes.
fn mad(a: &Plane, b: &Plane, dx: isize, dy: isize) -> Option<f64> {
    if !a.same_size(b) {
        return None;
    }
    let (w, h) = (isize::try_from(a.w).ok()?, isize::try_from(a.h).ok()?);
    let (x0, x1) = ((-dx).max(0), (w - dx).min(w));
    let (y0, y1) = ((-dy).max(0), (h - dy).min(h));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (mut s, mut n) = (0f64, 0usize);
    for y in y0..y1 {
        let ra = a.row(y as usize).get(x0 as usize..x1 as usize)?;
        let rb = b.row((y + dy) as usize).get((x0 + dx) as usize..(x1 + dx) as usize)?;
        s += f64::from(ra.iter().zip(rb).map(|(p, q)| (p - q).abs()).sum::<f32>());
        n += ra.len();
    }
    (n > 0).then(|| s / n as f64)
}

/// How the picture moved since the previous frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Shift {
    /// No usable estimate: too little texture, or no single shift explains the change.
    #[default]
    Unknown,
    /// Further than the search reaches (an eighth of the picture).
    Fast,
    /// By (x, y), in fractions of the picture's longest side.
    By(f64, f64),
}

/// The global shift from `a` to `b`: block matching of the mean-removed quarter-size pictures
/// over ±⅛ of their size, refined by ±2 pixels at half size with a parabola through the costs
/// (sub-pixel). The best match must explain the change: content that moves by itself (parallax,
/// water, a crowd) matches no single shift much better than the others, and then the shift is
/// unknown rather than shake.
fn global_shift(a: &Thumb, b: &Thumb) -> Shift {
    if a.mid.mean_sd().1 < MIN_TEXTURE || b.mid.mean_sd().1 < MIN_TEXTURE {
        return Shift::Unknown;
    }
    let (ac, bc) = (a.coarse.centred(), b.coarse.centred());
    let rx = isize::try_from((ac.w / 8).max(2)).unwrap_or(2);
    let ry = isize::try_from((ac.h / 8).max(2)).unwrap_or(2);
    // the smaller shift wins a tie
    let tie = |dx: isize, dy: isize| 1e-4 * (dx.abs() + dy.abs()) as f64;
    let mut best: Option<(f64, isize, isize)> = None;
    let (mut sum, mut count) = (0.0, 0.0);
    for dy in -ry..=ry {
        for dx in -rx..=rx {
            let Some(c) = mad(&ac, &bc, dx, dy) else { continue };
            sum += c;
            count += 1.0;
            if best.is_none_or(|(b, _, _)| c + tie(dx, dy) < b) {
                best = Some((c + tie(dx, dy), dx, dy));
            }
        }
    }
    let Some((cost, bx, by)) = best else { return Shift::Unknown };
    if count == 0.0 || cost > MATCH_QUALITY * sum / count {
        return Shift::Unknown;
    }
    if bx.abs() == rx || by.abs() == ry {
        return Shift::Fast;
    }
    let (am, bm) = (a.mid.centred(), b.mid.centred());
    let (cx, cy) = (2 * bx, 2 * by);
    // costs[j][i] at (cx + i − 2, cy + j − 2)
    let mut costs = [[f64::INFINITY; 5]; 5];
    let mut at = (2usize, 2usize, f64::INFINITY);
    for (j, row) in costs.iter_mut().enumerate() {
        for (i, c) in row.iter_mut().enumerate() {
            let (dx, dy) = (cx + i as isize - 2, cy + j as isize - 2);
            if let Some(m) = mad(&am, &bm, dx, dy) {
                *c = m;
                if m + tie(dx, dy) < at.2 {
                    at = (i, j, m + tie(dx, dy));
                }
            }
        }
    }
    let (i, j) = (at.0, at.1);
    let cost_at = |i: Option<usize>, j: Option<usize>| i.zip(j).and_then(|(i, j)| costs.get(j).and_then(|r| r.get(i))).copied().unwrap_or(f64::INFINITY);
    let sx = parabola(cost_at(i.checked_sub(1), Some(j)), cost_at(Some(i), Some(j)), cost_at(Some(i + 1), Some(j)));
    let sy = parabola(cost_at(Some(i), j.checked_sub(1)), cost_at(Some(i), Some(j)), cost_at(Some(i), Some(j + 1)));
    let long = am.w.max(am.h).max(1) as f64;
    Shift::By(((cx + i as isize - 2) as f64 + sx) / long, ((cy + j as isize - 2) as f64 + sy) / long)
}

/// Offset (−½ … ½) of the minimum of the parabola through three equally spaced costs.
fn parabola(left: f64, mid: f64, right: f64) -> f64 {
    let den = left - 2.0 * mid + right;
    if left.is_finite() && mid.is_finite() && right.is_finite() && den > 1e-12 { (0.5 * (left - right) / den).clamp(-0.5, 0.5) } else { 0.0 }
}

/// What one analysed frame looks like.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Look {
    /// Laplacian variance.
    sharp: f64,
    /// Mean and standard deviation of luma.
    mean: f64,
    contrast: f64,
    /// Mean absolute luma difference to the previous frame.
    motion: Option<f64>,
    shift: Shift,
    /// Scene difference to the previous frame (0 … 1).
    diff: f32,
}

/// One frame, reduced for the analysis.
struct Thumb {
    /// Luma, about [`SIDE`] pixels on the longest side.
    luma: Plane,
    /// Half and quarter size (the shift search).
    mid: Plane,
    coarse: Plane,
    sig: scene::Signature,
}

impl Thumb {
    fn of(f: &VideoFrame) -> Option<Thumb> {
        let (w, h, rgba) = small_rgba(f, SIDE)?;
        let sig = scene::signature(&rgba, w, h);
        let px = rgba.as_chunks::<4>().0.iter().map(|&[r, g, b, _]| (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0).collect();
        let luma = Plane::new(w, h, px)?;
        let mid = luma.half();
        let coarse = mid.half();
        Some(Thumb { luma, mid, coarse, sig })
    }

    fn look(&self, prev: Option<&Thumb>) -> Look {
        let (mean, contrast) = self.luma.mean_sd();
        let mut l = Look { sharp: laplacian_var(&self.luma), mean, contrast, ..Look::default() };
        if let Some(p) = prev {
            l.diff = scene::difference(&p.sig, &self.sig);
            if p.luma.same_size(&self.luma) {
                l.motion = mad(&p.luma, &self.luma, 0, 0);
                l.shift = global_shift(p, self);
            }
        }
        l
    }
}

/// The frame as straight sRGB RGBA8, reduced by a whole factor to about `side` pixels on its
/// longest side when the source returned it larger: (width, height, pixels).
fn small_rgba(f: &VideoFrame, side: usize) -> Option<(usize, usize, Vec<u8>)> {
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let n = (w.max(h) / side.max(1)).max(1);
    let (ow, oh, px) = if n < 2 {
        (w, h, f.to_rgba8())
    } else {
        match &f.data {
            PixelData::Yuv8 { .. } | PixelData::Yuv16 { .. } => {
                let d = f.box_decimated(n)?;
                (d.width as usize, d.height as usize, d.to_rgba8())
            }
            PixelData::RgbaF32(_) => {
                let (dw, dh, lin) = f.to_linear_f32_decimated(n);
                let mut out = vec![0u8; dw.checked_mul(dh)?.checked_mul(4)?];
                linear_premul_to_srgb8(&lin, &mut out);
                (dw, dh, out)
            }
            PixelData::Rgba8(d) => box_rgba8(d, w, h, n)?,
        }
    };
    (ow > 0 && oh > 0 && Some(px.len()) == ow.checked_mul(oh).and_then(|v| v.checked_mul(4))).then_some((ow, oh, px))
}

/// `n` × `n` box average of a `w` × `h` RGBA8 picture.
fn box_rgba8(d: &[u8], w: usize, h: usize, n: usize) -> Option<(usize, usize, Vec<u8>)> {
    let n = n.max(1);
    let (ow, oh) = (w / n, h / n);
    if ow == 0 || oh == 0 || d.len() < w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    let mut out = Vec::with_capacity(ow * oh * 4);
    let mut acc = vec![[0u64; 4]; ow];
    let count = (n as u64).saturating_mul(n as u64).max(1);
    for oy in 0..oh {
        acc.iter_mut().for_each(|a| *a = [0; 4]);
        for y in oy * n..(oy + 1) * n {
            let row = d.get(y * w * 4..(y + 1) * w * 4)?;
            for (block, a) in row.chunks_exact(n.checked_mul(4)?).zip(acc.iter_mut()) {
                for p in block.as_chunks::<4>().0 {
                    for (s, v) in a.iter_mut().zip(p) {
                        *s += u64::from(*v);
                    }
                }
            }
        }
        out.extend(acc.iter().flat_map(|a| a.map(|s| ((s + count / 2) / count).min(255) as u8)));
    }
    Some((ow, oh, out))
}

/// The scores of one frame.
#[derive(Clone, Copy, Debug, Default)]
struct Parts {
    sharp: f64,
    exposure: f64,
    steady: f64,
    lively: f64,
    /// Mean luma.
    level: f64,
    score: f64,
}

fn exposure_score(mean: f64, sd: f64) -> f64 {
    let level = if mean < MID_GREY { 1.0 - ((MID_GREY - mean) / 0.33).powi(2) } else { 1.0 - ((mean - MID_GREY) / 0.42).powi(2) };
    finite(level).clamp(0.0, 1.0) * (0.4 + 0.6 * finite(sd / 0.16).clamp(0.0, 1.0))
}

fn liveliness(motion: f64) -> f64 {
    if motion > 0.0 { finite((-0.5 * (motion / LIVELY).ln().powi(2)).exp()) } else { 0.0 }
}

fn parts_of(l: &Look, motion: f64, shake: Option<f64>) -> Parts {
    let sharp = smoothstep(SHARP_LOG.0, SHARP_LOG.1, l.sharp.max(1e-12).log10());
    let exposure = exposure_score(l.mean, l.contrast);
    let steady = shake.map_or(1.0, |s| 1.0 - smoothstep(SHAKE.0, SHAKE.1, s));
    let lively = liveliness(motion);
    let score = (0.30 * sharp + 0.25 * exposure + 0.25 * steady + 0.20 * lively).clamp(0.0, 1.0);
    Parts { sharp, exposure, steady, lively, level: finite(l.mean), score }
}

/// Short words saying why a stretch scored as it did.
fn reasons(p: &[Parts]) -> Vec<String> {
    let avg = |f: fn(&Parts) -> f64| mean(&p.iter().map(f).collect::<Vec<_>>());
    let (sharp, steady, lively, exposure, level) = (avg(|p| p.sharp), avg(|p| p.steady), avg(|p| p.lively), avg(|p| p.exposure), avg(|p| p.level));
    let mut r = Vec::new();
    let mut say = |cond: bool, word: &str| {
        if cond {
            r.push(word.to_string());
        }
    };
    say(sharp >= 0.6, "sharp");
    say(steady >= 0.75, "steady");
    say(lively >= 0.5, "lively");
    say(exposure >= 0.6 && (0.2..=0.75).contains(&level), "well exposed");
    say(level < 0.2, "dark");
    say(level > 0.75, "overexposed");
    say(sharp <= 0.3, "blurry");
    say(steady <= 0.45, "shaky");
    r
}

/// A stretch that may become a segment: samples `from..to`.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    start: f64,
    end: f64,
    from: usize,
    to: usize,
}

/// Scores per frame, the windows and the best of them.
fn pick(looks: &[Look], range: TimeRange, step: f64, o: &HighlightOptions) -> Highlights {
    let n = looks.len();
    let t0 = range.start.seconds();
    let len = range.duration.seconds();
    let time = |k: usize| t0 + k as f64 * step;
    // shots: each hard cut starts a new one
    let diffs: Vec<f32> = looks.iter().map(|l| l.diff).collect();
    let cuts = scene::detect_cuts(&diffs, &SceneOptions { sensitivity: 50.0, min_shot_frames: 1 });
    let mut shot = Vec::with_capacity(n);
    let mut current = 0usize;
    for k in 0..n {
        if cuts.binary_search(&k).is_ok() {
            current += 1;
        }
        shot.push(current);
    }
    let same_shot = |a: usize, b: usize| shot.get(a).is_some_and(|s| shot.get(b) == Some(s));
    let first_of_shot = |k: usize| k == 0 || !same_shot(k, k - 1);
    // motion to the previous frame of the shot; a shot's first frame borrows its successor's
    let motion: Vec<f64> = (0..n)
        .map(|k| {
            let own = looks.get(k).and_then(|l| l.motion).filter(|_| !first_of_shot(k));
            own.or_else(|| looks.get(k + 1).and_then(|l| l.motion).filter(|_| same_shot(k, k + 1) && !first_of_shot(k + 1))).unwrap_or(0.0)
        })
        .collect();
    // shake: how much the frame-to-frame shift changes, over frames k − 2, k − 1, k of one shot
    let shift = |k: usize| if first_of_shot(k) { Shift::Unknown } else { looks.get(k).map_or(Shift::Unknown, |l| l.shift) };
    let jerk = |k: usize| -> Option<f64> {
        if k < 2 || !same_shot(k, k - 2) {
            return None;
        }
        match (shift(k - 1), shift(k)) {
            (Shift::By(ax, ay), Shift::By(bx, by)) => Some((bx - ax).hypot(by - ay)),
            (Shift::Fast, _) | (_, Shift::Fast) => Some(FAST_SHAKE),
            _ => None,
        }
    };
    let raw: Vec<Option<f64>> = (0..n).map(|k| jerk(k + 1).or_else(|| jerk(k))).collect();
    let shake: Vec<Option<f64>> = (0..n)
        .map(|k| {
            let v: Vec<f64> = (k.saturating_sub(1)..=k + 1).filter(|&j| same_shot(j, k)).filter_map(|j| raw.get(j).copied().flatten()).collect();
            (!v.is_empty()).then(|| mean(&v))
        })
        .collect();
    let parts: Vec<Parts> =
        looks.iter().enumerate().map(|(k, l)| parts_of(l, motion.get(k).copied().unwrap_or(0.0), shake.get(k).copied().flatten())).collect();
    let window_score = |c: &Candidate| {
        let s: Vec<f64> = parts.get(c.from..c.to).unwrap_or(&[]).iter().map(|p| p.score).collect();
        if s.is_empty() {
            return 0.0;
        }
        0.7 * mean(&s) + 0.3 * s.iter().copied().fold(f64::INFINITY, f64::min)
    };
    // windows of the segment length inside one shot
    let seg = o.segment_seconds;
    let inside = (seg / step - 1e-9).ceil().max(1.0) as usize;
    let end_all = t0 + len;
    let mut cands: Vec<Candidate> = Vec::new();
    if len + 1e-9 < seg {
        cands.push(Candidate { start: t0, end: end_all, from: 0, to: n });
    } else {
        for k in 0..n {
            let start = time(k);
            if start + seg > end_all + 1e-6 {
                break;
            }
            // the frames inside and the first after the end (a cut before it could fall inside)
            let to = k.saturating_add(inside).min(n);
            if same_shot(k, to.min(n.saturating_sub(1))) {
                cands.push(Candidate { start, end: start + seg, from: k, to });
            }
        }
        if cands.is_empty() {
            // cuts everywhere: whole shots of two frames or more
            let mut k = 0;
            while k < n {
                let mut e = k;
                while e + 1 < n && same_shot(k, e + 1) {
                    e += 1;
                }
                if e > k {
                    cands.push(Candidate { start: time(k), end: time(e), from: k, to: e + 1 });
                }
                k = e + 1;
            }
        }
        if cands.is_empty() {
            cands.push(Candidate { start: t0, end: end_all, from: 0, to: n });
        }
    }
    let mut ranked: Vec<(f64, Candidate)> = cands.iter().map(|c| (window_score(c), *c)).collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.start.total_cmp(&b.1.start)));
    let mut chosen: Vec<(f64, Candidate)> = Vec::new();
    for (score, c) in ranked {
        if chosen.len() >= o.max_segments {
            break;
        }
        if chosen.iter().all(|(_, p)| c.start >= p.end + step - 1e-6 || c.end + step - 1e-6 <= p.start) {
            chosen.push((score, c));
        }
    }
    chosen.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
    let segments = chosen
        .iter()
        .map(|(score, c)| Segment { start: milli(c.start), end: milli(c.end), score: milli(*score), reasons: reasons(parts.get(c.from..c.to).unwrap_or(&[])) })
        .collect();
    let curve = parts.iter().enumerate().map(|(k, p)| [milli(time(k)), milli(p.score)]).collect();
    Highlights { seconds: milli(len), segments, curve }
}

/// `media.highlights {item, segmentSeconds?, maxSegments?, step?}`: the best moments of a video.
fn run_highlights(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "media.highlights";
    let item = item_param(p, CMD)?;
    let mut opts = HighlightOptions::default();
    if let Some(v) = number_param(p, "segmentSeconds", CMD)? {
        opts.segment_seconds = v;
    }
    if let Some(v) = number_param(p, "step", CMD)? {
        opts.step = v;
    }
    if let Some(v) = number_param(p, "maxSegments", CMD)? {
        if v.is_nan() || v < 1.0 {
            return Err(bad(CMD, "`maxSegments` must be at least 1"));
        }
        opts.max_segments = v.min(MAX_SEGMENTS as f64) as usize;
    }
    let (src, range) = item_range(s, item, CMD)?;
    opts.range = Some(range);
    let opts = opts.checked().map_err(|e| bad(CMD, e))?;
    if src.info().video.is_none() {
        return Err(bad(CMD, format!("item {} has no video", item.0)));
    }
    let cancel = AtomicBool::new(false);
    let h = highlights(src.as_ref(), &opts, &cancel, &|_, _| {}).map_err(EngineError::Other)?;
    Ok(json!(h))
}

#[cfg(test)]
#[path = "analysis_tests.rs"]
mod tests;
