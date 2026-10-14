//! Tests of [`super`]: beat grids of generated click tracks and music, highlights of generated
//! videos, and both commands with hostile parameters. No GPU, no files on disk.

use std::f64::consts::TAU;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use filmcraft_audio_dsp::remix as dsp;
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::generators::{GeneratorSource, render};
use filmcraft_media::{DemoScene, FrameRequest, Generator, MediaError, MediaInfo, MediaSource};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::{Value, json};

use super::*;

// ------------------------------------------------------------------------------------------ beats

/// A click track (mono): a short 2 kHz click on every beat from `offset` seconds, plus a 70 Hz
/// thump on the first beat of every bar; `gain(t)` scales it. Returns the samples and the beat
/// times.
fn clicks(rate: u32, bpm: f64, seconds: f64, offset: f64, gain: impl Fn(f64) -> f32) -> (Vec<f32>, Vec<f64>) {
    let sr = rate as f64;
    let mut x = vec![0f32; (seconds * sr) as usize];
    let mut truth = Vec::new();
    for k in 0.. {
        let t = offset + k as f64 * 60.0 / bpm;
        if t >= seconds {
            break;
        }
        truth.push(t);
        let s0 = (t * sr).round() as usize;
        let g = gain(t);
        for i in 0..(0.08 * sr) as usize {
            let Some(v) = x.get_mut(s0 + i) else { break };
            let u = i as f64 / sr;
            let click = 0.6 * (TAU * 2000.0 * u).sin() * (-u * 120.0).exp();
            let thump = if k % 4 == 0 { 1.2 * (TAU * 70.0 * u).sin() * (-u * 25.0).exp() } else { 0.0 };
            *v += g * (click + thump) as f32;
        }
    }
    (x, truth)
}

/// The sections cover the piece: from 0 to `seconds`, contiguous, calm or lively.
fn assert_covers(b: &Beats) {
    let first = b.sections.first().expect("sections");
    assert_eq!(first.start, 0.0, "{:?}", b.sections);
    assert_eq!(b.sections.last().unwrap().end, b.seconds, "{:?}", b.sections);
    for w in b.sections.windows(2) {
        assert_eq!(w[0].end, w[1].start, "{:?}", b.sections);
        assert_ne!(w[0].kind, w[1].kind, "neighbours of one kind are joined: {:?}", b.sections);
    }
    for s in &b.sections {
        assert!(s.kind == "calm" || s.kind == "lively", "{s:?}");
        assert!(s.end >= s.start, "{s:?}");
    }
}

/// Every number is finite and the JSON reads back to the same grid.
fn assert_sane(b: &Beats) {
    let nums = [b.seconds, b.bpm].into_iter().chain(b.beats.iter().copied()).chain(b.bars.iter().copied()).chain(b.energy.iter().copied());
    for v in nums.chain(b.sections.iter().flat_map(|s| [s.start, s.end])) {
        assert!(v.is_finite(), "{b:?}");
    }
    assert!(b.energy.iter().all(|e| (0.0..=1.0).contains(e)), "{:?}", b.energy);
    assert_eq!(b.energy.len(), b.beats.len());
    assert!(b.beats.windows(2).all(|w| w[0] < w[1]), "{:?}", b.beats);
    let back: Beats = serde_json::from_value(json!(b)).unwrap();
    assert_eq!(&back, b);
}

#[test]
fn click_tracks_give_tempo_beats_bars_and_sections() {
    for (bpm, rate) in [(120.0, 44_100), (120.0, 48_000), (90.0, 22_050), (90.0, 96_000)] {
        let (x, truth) = clicks(rate, bpm, 30.0, 0.25, |_| 0.8);
        let b = beats_from_mono(&x, rate);
        assert_sane(&b);
        assert_covers(&b);
        assert_eq!(b.seconds, 30.0);
        assert!((b.bpm - bpm).abs() <= 1.5, "{bpm} BPM at {rate} Hz: found {}", b.bpm);
        let beat = 60.0 / bpm;
        for w in b.beats.windows(2) {
            assert!((w[1] - w[0] - beat).abs() < 0.03, "{bpm} BPM at {rate} Hz: beats {} and {} are not a beat apart", w[0], w[1]);
        }
        // every beat is found (the attack within 25 ms), the first one at 0.25 s included
        for t in &truth {
            let d = b.beats.iter().map(|x| (x - t).abs()).fold(f64::INFINITY, f64::min);
            assert!(d < 0.025, "{bpm} BPM at {rate} Hz: no beat near {t} (off by {d})");
        }
        assert!(b.beats.len() <= truth.len() + 1, "{} beats for {}", b.beats.len(), truth.len());
        // bars every four beats, on the accented beats
        let first = b.beats.iter().position(|&t| t == b.bars[0]).expect("a bar starts on a beat");
        for (j, bar) in b.bars.iter().enumerate() {
            assert_eq!(Some(bar), b.beats.get(first + 4 * j), "bar {j}");
            let accented = truth.iter().step_by(4).map(|t| (t - bar).abs()).fold(f64::INFINITY, f64::min);
            assert!(accented < 0.025, "{bpm} BPM at {rate} Hz: bar at {bar} is not on an accented beat");
        }
        assert!(b.bars.len() * 4 + 4 >= b.beats.len(), "{} bars for {} beats", b.bars.len(), b.beats.len());
        // an even click track is lively throughout
        assert_eq!(b.sections.len(), 1, "{:?}", b.sections);
        assert_eq!(b.sections[0].kind, "lively");
        eprintln!("{bpm} BPM at {rate} Hz: {} BPM, {} beats, {} bars", b.bpm, b.beats.len(), b.bars.len());
    }
}

#[test]
fn quiet_and_loud_halves_make_a_calm_and_a_lively_section() {
    let (x, _) = clicks(44_100, 120.0, 40.0, 0.0, |t| if t < 20.0 { 0.06 } else { 1.0 });
    let b = beats_from_mono(&x, 44_100);
    assert_sane(&b);
    assert_covers(&b);
    assert!((b.bpm - 120.0).abs() <= 1.5, "{}", b.bpm);
    let kinds: Vec<&str> = b.sections.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, ["calm", "lively"], "{:?}", b.sections);
    let change = b.sections[1].start;
    assert!((change - 20.0).abs() <= 2.0, "the music gets loud at 20 s, the section starts at {change}");
    assert!(b.bars.contains(&change), "sections start on a bar: {change} {:?}", b.bars);
    // loud beats have a high energy, quiet ones a low one
    let at = |t: f64| b.beats.iter().position(|&x| x >= t).map(|i| b.energy[i]).unwrap();
    assert!(at(8.0) < 0.2 && at(30.0) > 0.6, "{} {}", at(8.0), at(30.0));
}

#[test]
fn short_sections_join_their_neighbours() {
    // a loud bar inside quiet music is too short to be a section of its own
    let bar = 2.0;
    let (x, _) = clicks(22_050, 120.0, 32.0, 0.0, |t| if (12.0..12.0 + bar).contains(&t) { 1.0 } else { 0.05 });
    let b = beats_from_mono(&x, 22_050);
    assert_sane(&b);
    assert_covers(&b);
    for s in &b.sections {
        assert!(s.end - s.start >= 2.0 * bar - 0.05, "a section shorter than two bars: {:?}", b.sections);
    }
    assert!(b.sections.len() <= 2, "{:?}", b.sections);
}

#[test]
fn generated_music_has_its_tempo() {
    let (m, _) = dsp::test_music(44_100, 128.0, 16, 3);
    let mono: Vec<f32> = m[0].iter().zip(&m[1]).map(|(l, r)| 0.5 * (l + r)).collect();
    let b = beats_from_mono(&mono, 44_100);
    assert_sane(&b);
    assert_covers(&b);
    assert!((b.bpm - 128.0).abs() <= 1.5, "{}", b.bpm);
    assert!(b.beats.len() >= 60, "{} beats", b.beats.len());
    assert!(b.bars.windows(2).all(|w| (w[1] - w[0] - 4.0 * 60.0 / 128.0).abs() < 0.05), "{:?}", b.bars);
}

#[test]
fn silence_noise_short_and_damaged_audio_never_panic() {
    // silence: no beats, one calm section
    let b = beats_from_mono(&vec![0.0; 22_050 * 10], 22_050);
    assert_eq!((b.bpm, b.beats.len(), b.bars.len()), (0.0, 0, 0));
    assert_eq!(b.sections, vec![Section { kind: "calm".into(), start: 0.0, end: 10.0 }]);
    // 0.1 s of clicks
    let (x, _) = clicks(44_100, 120.0, 0.1, 0.0, |_| 1.0);
    let b = beats_from_mono(&x, 44_100);
    assert_eq!(b.seconds, 0.1);
    assert_covers(&b);
    // nothing at all
    assert_eq!(beats_from_mono(&[], 48_000), Beats::default());
    assert_eq!(beats_from_mono(&x, 0), Beats::default());
    assert_eq!(beats_from_mono(&[0.5], 1), without_beats(1.0));
    // odd and absurd rates
    let (y, _) = clicks(22_050, 120.0, 6.0, 0.1, |_| 1.0);
    for rate in [1, 3, 999, 3_999, 4_000, 7_350, 11_025, 37_800, 192_000, 384_000, 1_000_000, u32::MAX] {
        for input in [&x[..], &y[..]] {
            let b = beats_from_mono(input, rate);
            assert_sane(&b);
            assert_covers(&b);
        }
    }
    // NaN, infinities and huge values among the samples
    let (mut z, _) = clicks(22_050, 120.0, 12.0, 0.25, |_| 1.0);
    for (k, v) in z.iter_mut().enumerate().step_by(997) {
        *v = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30][k % 5];
    }
    let b = beats_from_mono(&z, 22_050);
    assert_sane(&b);
    assert_covers(&b);
    let nan = vec![f32::NAN; 44_100 * 3];
    let b = beats_from_mono(&nan, 44_100);
    assert_eq!(b.bpm, 0.0);
    assert_covers(&b);
    // white noise has no beat
    let mut seed = 7u32;
    let noise: Vec<f32> = (0..22_050 * 8)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect();
    let b = beats_from_mono(&noise, 22_050);
    assert_sane(&b);
    assert_covers(&b);
}

#[test]
fn long_audio_is_analysed_up_to_the_limit() {
    let (x, _) = clicks(8_000, 120.0, 40.0, 0.25, |_| 1.0);
    let b = beats_within(&x, 8_000, 20.0);
    assert_sane(&b);
    assert_covers(&b);
    assert_eq!(b.seconds, 20.0);
    assert!(b.beats.iter().all(|&t| t < 20.0));
    assert!((b.bpm - 120.0).abs() <= 1.5, "{}", b.bpm);
    for limit in [0.0, -5.0, f64::NAN, f64::INFINITY] {
        let b = beats_within(&x, 8_000, limit);
        assert_covers_or_empty(&b);
    }
}

fn assert_covers_or_empty(b: &Beats) {
    if b.seconds > 0.0 {
        assert_covers(b);
    } else {
        assert!(b.beats.is_empty());
    }
}

/// Stereo WAV bytes of mono samples.
fn wav(x: &[f32], rate: u32) -> Arc<[u8]> {
    let inter: Vec<f32> = x.iter().flat_map(|&v| [v, v]).collect();
    crate::previews::write_wav_f32(&inter, rate).into()
}

#[test]
fn audio_beats_command_reads_the_item() {
    let mut s = Session::default();
    let (x, _) = clicks(48_000, 120.0, 20.0, 0.25, |_| 0.8);
    let item = crate::commands::import_bytes(&mut s, "/clicks.wav", wav(&x, 48_000), None).unwrap();
    let r = s.execute("audio.beats", json!({"item": item.0})).unwrap();
    let b: Beats = serde_json::from_value(r).unwrap();
    assert_sane(&b);
    assert_covers(&b);
    assert!((b.bpm - 120.0).abs() <= 1.5, "{}", b.bpm);
    assert!((b.seconds - 20.0).abs() < 0.01, "{}", b.seconds);
    assert!(b.beats.len() >= 38, "{} beats", b.beats.len());
    // a subclip: from its In point
    let mut p = (*s.project).clone();
    let sub = p.add_item(
        "sub",
        Label::Iris,
        ItemKind::Subclip { parent: item, range: TimeRange::new(Tick::from_seconds_f64(5.0), Tick::from_seconds_f64(8.0)), restrict_trims: false },
        None,
    );
    s.project = Arc::new(p);
    let r = s.execute("audio.beats", json!({"item": sub.0})).unwrap();
    let b: Beats = serde_json::from_value(r).unwrap();
    assert_eq!(b.seconds, 8.0);
    assert_covers(&b);
    assert!(b.beats.iter().any(|t| (t - 0.25).abs() < 0.03), "the beat at 5.25 s is at 0.25 s of the subclip: {:?}", b.beats);
    // hostile parameters
    for p in
        [json!({}), json!(null), json!([]), json!({"item": null}), json!({"item": "x"}), json!({"item": -3}), json!({"item": 1e300}), json!({"item": 999_999})]
    {
        assert!(s.execute("audio.beats", p.clone()).is_err(), "{p}");
    }
    // a video without sound
    let video = add_video(&mut s, TestVideo::new(vec![Part::moving(DemoScene::Forest, 2.0)]));
    let e = s.execute("audio.beats", json!({"item": video.0})).unwrap_err();
    assert!(e.to_string().contains("no audio"), "{e}");
}

// ------------------------------------------------------------------------------------- highlights

/// One stretch of a [`TestVideo`].
#[derive(Clone, Debug)]
struct Part {
    /// Until this media time (seconds).
    until: f64,
    scene: DemoScene,
    /// How fast the scene's time runs (0: a still picture).
    speed: f32,
    /// Box-blur radius, fraction of the width.
    blur: f32,
    /// Random displacement per frame (camera shake), fraction of the width.
    shake: f32,
    /// Steady movement (a pan), widths per second.
    pan: f32,
}

impl Part {
    fn moving(scene: DemoScene, until: f64) -> Part {
        Part { until, scene, speed: 4.0, blur: 0.0, shake: 0.0, pan: 0.0 }
    }
    fn still(scene: DemoScene, until: f64) -> Part {
        Part { speed: 0.0, ..Part::moving(scene, until) }
    }
}

/// A 640 × 360, 25 fps video of generated scenes ([`Part`]s one after the other).
struct TestVideo {
    info: MediaInfo,
    parts: Vec<Part>,
    /// Rendered canvases of still parts.
    canvases: Mutex<Vec<((usize, usize, usize), Arc<Vec<u8>>)>>,
}

const TW: usize = 640;
const TH: usize = 360;

impl TestVideo {
    fn new(parts: Vec<Part>) -> TestVideo {
        let end = parts.last().map_or(1.0, |p| p.until);
        let mut info =
            GeneratorSource::new(Generator::Demo(DemoScene::Forest), TW as u32, TH as u32, FrameRate::FPS_25, Tick::from_seconds_f64(end)).info().clone();
        info.audio_streams.clear();
        TestVideo { info, parts, canvases: Mutex::new(Vec::new()) }
    }

    /// The scene of part `i` at time `t` on a canvas a quarter of the width larger on each side.
    fn canvas(&self, i: usize, p: &Part, t: f64, cw: usize, ch: usize) -> Arc<Vec<u8>> {
        let render_it = || Arc::new(render(&Generator::Demo(p.scene), cw as u32, ch as u32, t as f32 * p.speed, (t * 25.0) as i64, FrameRate::FPS_25));
        if p.speed != 0.0 {
            return render_it();
        }
        let mut c = self.canvases.lock().unwrap();
        if let Some((_, px)) = c.iter().find(|(k, _)| *k == (i, cw, ch)) {
            return px.clone();
        }
        let px = render_it();
        c.push(((i, cw, ch), px.clone()));
        px
    }
}

/// A pseudo-random number in −1..1 for frame `f`.
fn wobble(f: i64, salt: u64) -> f32 {
    let mut x = (f as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 29;
    (x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
}

/// Box blur of radius `r` (one horizontal and one vertical pass, twice).
fn blur(px: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 {
        return;
    }
    for _ in 0..2 {
        for (len, stride, count, step) in [(w, 4, h, w * 4), (h, w * 4, w, 4)] {
            for line in 0..count {
                for c in 0..3 {
                    let at = |i: usize| line * step + i * stride + c;
                    let src: Vec<u32> = (0..len).map(|i| px[at(i)] as u32).collect();
                    for i in 0..len {
                        let (a, b) = (i.saturating_sub(r), (i + r).min(len - 1));
                        px[at(i)] = (src[a..=b].iter().sum::<u32>() / (b - a + 1) as u32) as u8;
                    }
                }
            }
        }
    }
}

impl MediaSource for TestVideo {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        let t = req.time.seconds();
        let (i, p) =
            self.parts.iter().enumerate().find(|(_, p)| t < p.until).or(self.parts.iter().enumerate().next_back()).ok_or(MediaError::NoStream("video"))?;
        let start = if i == 0 { 0.0 } else { self.parts[i - 1].until };
        let s = req.scale.clamp(1.0 / 64.0, 1.0);
        let (w, h) = (((TW as f32 * s).round() as usize).max(16), ((TH as f32 * s).round() as usize).max(16));
        let m = w / 4;
        let (cw, ch) = (w + 2 * m, h + 2 * m);
        let canvas = self.canvas(i, p, t, cw, ch);
        let f = (t * 25.0).round() as i64;
        let ox = (0.05 + p.pan * (t - start) as f32) * w as f32 + p.shake * w as f32 * wobble(f, 1) + m as f32;
        let oy = p.shake * w as f32 * wobble(f, 2) + m as f32;
        let (ox, oy) = ((ox.round() as usize).min(2 * m), (oy.round() as usize).min(2 * m));
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            let from = ((y + oy) * cw + ox) * 4;
            px[y * w * 4..(y + 1) * w * 4].copy_from_slice(&canvas[from..from + w * 4]);
        }
        blur(&mut px, w, h, (p.blur * w as f32).round() as usize);
        Ok(Arc::new(VideoFrame::rgba8(w as u32, h as u32, px).with_pts(req.time)))
    }

    fn audio(&self, _start: i64, _frames: usize, _sample_rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        Err(MediaError::NoStream("audio"))
    }
}

fn run(src: &dyn MediaSource, opts: &HighlightOptions) -> Highlights {
    let calls = Mutex::new(Vec::new());
    let h = highlights(src, opts, &AtomicBool::new(false), &|done, total| calls.lock().unwrap().push((done, total))).unwrap();
    let calls = calls.into_inner().unwrap();
    assert!(!calls.is_empty());
    assert!(calls.windows(2).all(|w| w[1].0 == w[0].0 + 1 && w[1].1 == w[0].1), "progress counts up");
    assert_eq!(calls.last().map(|c| c.0), calls.last().map(|c| c.1), "progress ends at the total");
    assert_eq!(calls.len(), h.curve.len());
    h
}

/// Segments are in order, a step apart, inside the media, of the asked length (or the whole
/// media), with sane scores; the curve is in order and within 0..1.
fn assert_well_formed(h: &Highlights, opts: &HighlightOptions, start: f64) {
    let end = start + h.seconds;
    assert!(!h.segments.is_empty() && h.segments.len() <= opts.max_segments, "{h:?}");
    for s in &h.segments {
        assert!(s.start >= start - 1e-6 && s.end <= end + 1e-6 && s.end > s.start, "{s:?} in {start}..{end}");
        assert!((0.0..=1.0).contains(&s.score), "{s:?}");
        let full = (s.end - s.start - opts.segment_seconds).abs() < 0.002;
        assert!(full || h.segments.len() == 1, "{s:?}");
    }
    for w in h.segments.windows(2) {
        assert!(w[1].start >= w[0].end + opts.step - 0.002, "segments a step apart: {:?}", h.segments);
    }
    assert!(h.curve.windows(2).all(|w| w[1][0] > w[0][0]), "{:?}", h.curve);
    assert!(h.curve.iter().all(|c| (0.0..=1.0).contains(&c[1]) && c[0] >= start - 1e-6 && c[0] < end), "{:?}", h.curve);
    let back: Highlights = serde_json::from_value(json!(h)).unwrap();
    assert_eq!(&back, h);
}

fn mean_score(h: &Highlights, from: f64, to: f64) -> f64 {
    let v: Vec<f64> = h.curve.iter().filter(|c| c[0] >= from && c[0] < to).map(|c| c[1]).collect();
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn best(h: &Highlights) -> &Segment {
    h.segments.iter().max_by(|a, b| a.score.total_cmp(&b.score)).unwrap()
}

#[test]
fn the_best_moment_is_sharp_and_moving() {
    let blurred = Part { blur: 0.025, ..Part::still(DemoScene::Forest, 8.0) };
    let v = TestVideo::new(vec![blurred, Part::moving(DemoScene::Forest, 16.0)]);
    let opts = HighlightOptions::default();
    let h = run(&v, &opts);
    assert_well_formed(&h, &opts, 0.0);
    eprintln!("{:#?}", h.segments);
    eprintln!("still+blurred {:.3}, moving {:.3}", mean_score(&h, 0.0, 8.0), mean_score(&h, 8.0, 16.0));
    assert_eq!(h.seconds, 16.0);
    assert_eq!(h.curve.len(), 32);
    assert!(mean_score(&h, 8.0, 16.0) > mean_score(&h, 0.0, 8.0) + 0.2, "{:?}", h.curve);
    let top = best(&h);
    assert!(top.start >= 8.0, "the best segment {top:?} is in the sharp, moving part");
    assert!(top.reasons.iter().any(|r| r == "sharp"), "{top:?}");
    for s in h.segments.iter().filter(|s| s.end <= 8.0) {
        assert!(s.reasons.iter().any(|r| r == "blurry"), "{s:?}");
    }
}

#[test]
fn shaky_footage_scores_lower_than_a_smooth_pan() {
    let pan = Part { pan: 0.02, ..Part::still(DemoScene::Forest, 8.0) };
    let shaky = Part { shake: 0.03, ..Part::still(DemoScene::Forest, 16.0) };
    let v = TestVideo::new(vec![pan, shaky]);
    let opts = HighlightOptions { max_segments: 6, ..HighlightOptions::default() };
    let h = run(&v, &opts);
    assert_well_formed(&h, &opts, 0.0);
    eprintln!("{:#?}", h.segments);
    eprintln!("pan {:.3}, shaky {:.3}", mean_score(&h, 0.0, 8.0), mean_score(&h, 8.0, 16.0));
    assert!(mean_score(&h, 0.5, 8.0) > mean_score(&h, 8.5, 16.0) + 0.1, "{:?}", h.curve);
    let top = best(&h);
    assert!(top.end <= 8.0, "the best segment {top:?} is in the smooth part");
    assert!(top.reasons.iter().any(|r| r == "steady"), "{top:?}");
    for s in h.segments.iter().filter(|s| s.start >= 8.5) {
        assert!(s.reasons.iter().any(|r| r == "shaky"), "{s:?}");
    }
}

#[test]
fn no_segment_spans_a_hard_cut() {
    let v = TestVideo::new(vec![Part::moving(DemoScene::Forest, 5.2), Part::moving(DemoScene::CityNight, 12.0)]);
    let opts = HighlightOptions { max_segments: 10, ..HighlightOptions::default() };
    let h = run(&v, &opts);
    assert_well_formed(&h, &opts, 0.0);
    assert!(h.segments.len() >= 2, "{:?}", h.segments);
    for s in &h.segments {
        assert!(!(s.start < 5.2 && s.end > 5.2), "{s:?} spans the cut at 5.2 s");
    }
    // the night scene is dark
    let night: Vec<&Segment> = h.segments.iter().filter(|s| s.start > 5.2).collect();
    assert!(!night.is_empty() && night.iter().all(|s| s.reasons.iter().any(|r| r == "dark")), "{night:?}");
    // shots of 1.5 s leave no room for 3 s: whole shots are the candidates
    let parts: Vec<Part> = (1..=6).map(|k| Part::moving(if k % 2 == 0 { DemoScene::Forest } else { DemoScene::CityNight }, 1.6 * k as f64)).collect();
    let h = run(&TestVideo::new(parts), &opts);
    assert!(!h.segments.is_empty());
    for s in &h.segments {
        let shot = (s.start / 1.6).floor();
        assert!(s.end <= (shot + 1.0) * 1.6 + 1e-6, "{s:?} crosses a cut");
    }
}

#[test]
fn media_shorter_than_a_segment_is_one_candidate() {
    let v = TestVideo::new(vec![Part::moving(DemoScene::Dunes, 2.0)]);
    let opts = HighlightOptions::default();
    let h = run(&v, &opts);
    assert_well_formed(&h, &opts, 0.0);
    assert_eq!(h.segments.len(), 1);
    assert_eq!((h.segments[0].start, h.segments[0].end), (0.0, 2.0));
    // a range of the media
    let opts = HighlightOptions { range: Some(TimeRange::new(Tick::from_seconds_f64(0.5), Tick::from_seconds_f64(1.0))), ..HighlightOptions::default() };
    let h = run(&v, &opts);
    assert_eq!(h.seconds, 1.0);
    assert_eq!((h.segments[0].start, h.segments[0].end), (0.5, 1.5));
    assert!(h.curve.iter().all(|c| (0.5..1.5).contains(&c[0])), "{:?}", h.curve);
}

/// A source whose frames cannot be decoded.
struct Broken(MediaInfo);

impl MediaSource for Broken {
    fn info(&self) -> &MediaInfo {
        &self.0
    }
    fn video_frame(&self, _req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        Err(MediaError::Decode("damaged".into()))
    }
    fn audio(&self, _start: i64, _frames: usize, _sample_rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        Err(MediaError::NoStream("audio"))
    }
}

#[test]
fn hostile_options_and_sources_are_errors_or_clamped() {
    let v = TestVideo::new(vec![Part::moving(DemoScene::Plasma, 2.0)]);
    let cancel = AtomicBool::new(false);
    let none = |_: u64, _: u64| {};
    let with = |segment_seconds: f64, step: f64, max_segments: usize| HighlightOptions { segment_seconds, step, max_segments, range: None };
    for bad in [
        with(3.0, 0.0, 4),
        with(3.0, -1.0, 4),
        with(3.0, f64::NAN, 4),
        with(3.0, f64::INFINITY, 4),
        with(0.0, 0.5, 4),
        with(-3.0, 0.5, 4),
        with(f64::NAN, 0.5, 4),
        with(f64::NEG_INFINITY, 0.5, 4),
        with(3.0, 0.5, 0),
    ] {
        assert!(highlights(&v, &bad, &cancel, &none).is_err(), "{bad:?}");
    }
    // clamped: huge segments cover the media, tiny steps are 0.04 s, many segments are fine
    let h = highlights(&v, &with(1e300, 0.5, 4), &cancel, &none).unwrap();
    assert_eq!(h.segments.len(), 1);
    let h = highlights(&v, &with(0.5, 1e-300, usize::MAX), &cancel, &none).unwrap();
    assert_eq!(h.curve.len(), 50);
    assert!(h.segments.len() > 1);
    // a range outside the media
    let far = HighlightOptions { range: Some(TimeRange::new(Tick::from_seconds_f64(50.0), Tick::from_seconds_f64(1.0))), ..HighlightOptions::default() };
    assert!(highlights(&v, &far, &cancel, &none).is_err());
    let backwards = HighlightOptions { range: Some(TimeRange::new(Tick::from_seconds_f64(1.0), Tick::from_seconds_f64(-1.0))), ..HighlightOptions::default() };
    assert!(highlights(&v, &backwards, &cancel, &none).is_err());
    // stopped
    assert_eq!(highlights(&v, &HighlightOptions::default(), &AtomicBool::new(true), &none), Err("cancelled".to_string()));
    // no picture, no length, undecodable frames
    let tone = GeneratorSource::new(Generator::Tone { hz: 440.0, db: -12.0 }, 640, 360, FrameRate::FPS_25, Tick::from_seconds_f64(5.0));
    assert!(highlights(&tone, &HighlightOptions::default(), &cancel, &none).unwrap_err().contains("no video"));
    let empty = GeneratorSource::new(Generator::Demo(DemoScene::Forest), 640, 360, FrameRate::FPS_25, Tick::ZERO);
    assert!(highlights(&empty, &HighlightOptions::default(), &cancel, &none).is_err());
    let negative = GeneratorSource::new(Generator::Demo(DemoScene::Forest), 640, 360, FrameRate::FPS_25, Tick(-5));
    assert!(highlights(&negative, &HighlightOptions::default(), &cancel, &none).is_err());
    let broken = Broken(v.info().clone());
    assert!(highlights(&broken, &HighlightOptions::default(), &cancel, &none).unwrap_err().contains("damaged"));
    // a zero-sized picture
    let tiny = GeneratorSource::new(Generator::Demo(DemoScene::Forest), 0, 0, FrameRate::FPS_25, Tick::from_seconds_f64(1.0));
    let _ = highlights(&tiny, &HighlightOptions::default(), &cancel, &none);
}

#[test]
fn long_media_are_sampled_at_most_the_limit() {
    let black = GeneratorSource::new(Generator::BlackVideo, 1280, 720, FrameRate::FPS_25, Tick::from_seconds_f64(1500.0));
    let opts = HighlightOptions::default();
    let h = run(&black, &opts);
    assert_eq!(h.curve.len(), HIGHLIGHT_MAX_SAMPLES);
    assert_eq!(h.seconds, 1500.0);
    assert!((h.curve[1][0] - h.curve[0][0] - 0.625).abs() < 0.002, "{:?}", &h.curve[..2]);
    let s = &h.segments[0];
    assert!(s.reasons.iter().any(|r| r == "dark") && s.reasons.iter().any(|r| r == "blurry"), "{s:?}");
}

/// Timing (run with `--ignored --nocapture`): the analysis alone on a ten-minute generated clip,
/// and decoding plus analysis of an H.264 file made by our own encoder.
#[test]
#[ignore]
fn timing_of_long_clips() {
    let fresh = || GeneratorSource::new(Generator::Demo(DemoScene::Forest), 1920, 1080, FrameRate::FPS_25, Tick::from_seconds_f64(600.0));
    let t = std::time::Instant::now();
    let h = run(&fresh(), &HighlightOptions::default());
    let total = t.elapsed().as_secs_f64();
    // the generator's own rendering of the same frames (a new source: nothing cached)
    let src = fresh();
    let t = std::time::Instant::now();
    for c in &h.curve {
        let _ = src.video_frame(FrameRequest { time: Tick::from_seconds_f64(c[0]), scale: 256.0 / 1920.0 }).unwrap();
    }
    let render = t.elapsed().as_secs_f64();
    let n = h.curve.len() as f64;
    eprintln!(
        "10 min generated 1080p: {n} frames in {total:.2} s: {:.2} ms per frame, of which {:.2} ms rendering the picture",
        total * 1000.0 / n,
        render * 1000.0 / n
    );
    // a 60 s 1280 × 720 H.264 file (24 fps)
    let dir = crate::media_test_util::tmp_dir("highlights-timing");
    let path = dir.join("forest.mp4");
    crate::media_test_util::make_movie(&path, DemoScene::Forest, 1280, 720, 24 * 60);
    let (mut s, items, _) = crate::media_test_util::session_with(&[&path]);
    let t = std::time::Instant::now();
    let r = s.execute("media.highlights", json!({"item": items[0].0})).unwrap();
    let total = t.elapsed().as_secs_f64();
    let h: Highlights = serde_json::from_value(r).unwrap();
    eprintln!("60 s H.264 720p: {} frames in {total:.2} s ({:.1} ms per frame)", h.curve.len(), total * 1000.0 / h.curve.len() as f64);
    std::fs::remove_dir_all(dir).ok();
}

/// A project item for `src` (its media pool source).
fn add_video(s: &mut Session, src: TestVideo) -> filmcraft_project::ItemId {
    let mut p = (*s.project).clone();
    let clip = MediaClip {
        media: MediaRef::Generator(Generator::Demo(DemoScene::Forest)),
        info: src.info().clone(),
        interpret: Default::default(),
        mark_in: None,
        mark_out: None,
        markers: vec![],
        offline: false,
        proxy: None,
        identity: None,
    };
    let item = p.add_item("test video", Label::Iris, ItemKind::Media(clip), None);
    s.project = Arc::new(p);
    s.media.insert(item, Arc::new(src));
    item
}

#[test]
fn media_highlights_command() {
    let mut s = Session::default();
    let blurred = Part { blur: 0.025, ..Part::still(DemoScene::Forest, 4.0) };
    let item = add_video(&mut s, TestVideo::new(vec![blurred, Part::moving(DemoScene::Forest, 8.0)]));
    let r = s.execute("media.highlights", json!({"item": item.0, "segmentSeconds": 2, "maxSegments": 2, "step": 0.5})).unwrap();
    let h: Highlights = serde_json::from_value(r).unwrap();
    assert_well_formed(&h, &HighlightOptions { segment_seconds: 2.0, max_segments: 2, step: 0.5, range: None }, 0.0);
    assert!(best(&h).start >= 4.0, "{:?}", h.segments);
    // defaults
    let r = s.execute("media.highlights", json!({"item": item.0})).unwrap();
    let h: Highlights = serde_json::from_value(r).unwrap();
    assert_eq!(h.curve.len(), 16);
    // a subclip: its range only, in media time
    let mut p = (*s.project).clone();
    let sub = p.add_item(
        "sub",
        Label::Iris,
        ItemKind::Subclip { parent: item, range: TimeRange::new(Tick::from_seconds_f64(4.5), Tick::from_seconds_f64(3.0)), restrict_trims: false },
        None,
    );
    s.project = Arc::new(p);
    let r = s.execute("media.highlights", json!({"item": sub.0, "segmentSeconds": 1})).unwrap();
    let h: Highlights = serde_json::from_value(r).unwrap();
    assert_eq!(h.seconds, 3.0);
    assert!(h.segments.iter().all(|s| s.start >= 4.5 && s.end <= 7.5), "{:?}", h.segments);
    // hostile parameters
    let id = item.0;
    for p in [
        json!({}),
        json!({"item": null}),
        json!({"item": "x"}),
        json!({"item": 999_999}),
        json!({"item": -1}),
        json!({"item": id, "segmentSeconds": -1}),
        json!({"item": id, "segmentSeconds": 0}),
        json!({"item": id, "segmentSeconds": "long"}),
        json!({"item": id, "step": 0}),
        json!({"item": id, "step": -0.5}),
        json!({"item": id, "step": [1]}),
        json!({"item": id, "maxSegments": 0}),
        json!({"item": id, "maxSegments": -4}),
        json!({"item": id, "maxSegments": "x"}),
    ] {
        assert!(s.execute("media.highlights", p.clone()).is_err(), "{p}");
    }
    for p in [json!({"item": id, "segmentSeconds": 1e300}), json!({"item": id, "maxSegments": 1e300, "step": 1e-300}), json!({"item": id, "step": null})] {
        let r: Value = s.execute("media.highlights", p.clone()).unwrap_or_else(|e| panic!("{p}: {e}"));
        let h: Highlights = serde_json::from_value(r).unwrap();
        assert!(!h.segments.is_empty(), "{p}");
    }
    // an item without picture
    let (x, _) = clicks(48_000, 120.0, 3.0, 0.25, |_| 0.8);
    let audio = crate::commands::import_bytes(&mut s, "/clicks.wav", wav(&x, 48_000), None).unwrap();
    let e = s.execute("media.highlights", json!({"item": audio.0})).unwrap_err();
    assert!(e.to_string().contains("no video"), "{e}");
}
