//! Tests of `media.captureInfo` and `regie.findMedia`: capture date, place and size from
//! hand-made photos, sounds and movies (and from ffmpeg-made MOV / M4A files with Apple and
//! Android metadata when ffmpeg is installed), the folder walk with its filters and limits, the
//! user's folders, the commands with hostile parameters, and mutated headers (never a panic).
//! Every search runs on temporary folders made here, never on the user's own folders.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use super::exif::tests::{PHONE_PLACE, dated_exif, dng, jpeg, phone_exif, png, stills};
use super::*;
use crate::Session;
use crate::media_test_util::tmp_dir;

fn write(path: &Path, bytes: &[u8]) {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// Set a file's modification time to an RFC 3339 UTC time.
fn touch(path: &Path, rfc3339: &str) {
    let secs = Stamp::parse(rfc3339).unwrap().instant();
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)).unwrap();
}

/// The capture info of a file's bytes (`ext` as the file's extension).
fn info_of(bytes: &[u8], ext: &str) -> CaptureInfo {
    let mut info = CaptureInfo { kind: kind_of_extension(ext).unwrap_or_default().into(), ..Default::default() };
    read_info(bytes, ext, &mut info);
    info
}

fn near(a: [f64; 2], b: [f64; 2]) -> bool {
    (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6
}

// ---------- hand-made sound files ----------

fn wav_1s() -> Vec<u8> {
    filmcraft_media::wav::write_wav16(&vec![0.0; 96_000], 2, 48_000)
}

/// RF64: sizes in `ds64`, 0.1 s of 48 kHz 16-bit stereo.
fn rf64() -> Vec<u8> {
    let data = 19_200u64;
    let mut v = b"RF64".to_vec();
    v.extend(u32::MAX.to_le_bytes());
    v.extend(b"WAVEds64");
    v.extend(28u32.to_le_bytes());
    v.extend((data + 72).to_le_bytes());
    v.extend(data.to_le_bytes());
    v.extend((data / 4).to_le_bytes());
    v.extend(0u32.to_le_bytes());
    v.extend(b"fmt ");
    v.extend(16u32.to_le_bytes());
    v.extend([1, 0, 2, 0]);
    v.extend(48_000u32.to_le_bytes());
    v.extend(192_000u32.to_le_bytes());
    v.extend([4, 0, 16, 0]);
    v.extend(b"data");
    v.extend(u32::MAX.to_le_bytes());
    v.extend(vec![0u8; data as usize]);
    v
}

/// AIFF: 44 100 frames at 44.1 kHz (1 s).
fn aiff() -> Vec<u8> {
    let mut comm = 2u16.to_be_bytes().to_vec();
    comm.extend(44_100u32.to_be_bytes());
    comm.extend(16u16.to_be_bytes());
    // 44100 as an 80-bit extended number: exponent 16383 + 15, mantissa 0xAC44 << 48
    comm.extend([0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
    let mut v = b"FORM".to_vec();
    v.extend((4 + 8 + comm.len() as u32 + 16).to_be_bytes());
    v.extend(b"AIFFCOMM");
    v.extend((comm.len() as u32).to_be_bytes());
    v.extend(comm);
    v.extend(b"SSND");
    v.extend(8u32.to_be_bytes());
    v.extend([0; 8]);
    v
}

/// FLAC after an ID3v2 tag: 96 000 samples at 48 kHz (2 s).
fn flac() -> Vec<u8> {
    let mut v = b"ID3\x04\x00\x00\x00\x00\x00\x0A".to_vec();
    v.extend([0; 10]);
    v.extend(b"fLaC");
    v.extend([0x80, 0, 0, 34]);
    v.extend(4096u16.to_be_bytes());
    v.extend(4096u16.to_be_bytes());
    v.extend([0; 6]);
    let packed: u64 = (48_000 << 44) | (1 << 41) | (15 << 36) | 96_000;
    v.extend(packed.to_be_bytes());
    v.extend([0; 16]);
    v
}

// ---------- hand-made movies ----------

fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| <[u8]>::len(p)).sum();
    let mut v = ((len + 8) as u32).to_be_bytes().to_vec();
    v.extend(kind);
    for p in parts {
        v.extend(*p);
    }
    v
}

fn full(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let mut all: Vec<&[u8]> = vec![&[0, 0, 0, 0]];
    all.extend(parts);
    bx(kind, &all)
}

/// What [`movie`] writes.
struct Movie {
    /// `mvhd` creation time as RFC 3339 (None: unset).
    created: Option<&'static str>,
    /// `tkhd` matrix.
    matrix: [i32; 9],
    size: (u32, u32),
    video: bool,
    /// `moov/meta` keys and values (QuickTime metadata).
    keys: Vec<(&'static str, &'static str)>,
    /// `udta` text atoms `©xyz` and `©day`.
    xyz: Option<&'static str>,
    day: Option<&'static str>,
}

const UPRIGHT: [i32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];
/// 90° clockwise (a portrait phone video stored landscape).
const QUARTER_TURN: [i32; 9] = [0, 0x10000, 0, -0x10000, 0, 0, 0, 0, 0x4000_0000];

/// A MOV with one video (or sound) track of 2 s, its sample table empty, and the metadata
/// `m` asks for; the `moov` comes after the `mdat` as in camera files.
fn movie(m: &Movie) -> Vec<u8> {
    let be = |v: u32| v.to_be_bytes();
    let created = m.created.map_or(0, |c| (Stamp::parse(c).unwrap().instant() as u64 + MP4_EPOCH_TO_UNIX) as u32);
    let mut mvhd = be(created).to_vec();
    mvhd.extend(be(created));
    mvhd.extend(be(600));
    mvhd.extend(be(1200));
    mvhd.extend([0u8; 80]);
    let mut tkhd = [be(created), be(created), be(1), be(0), be(1200)].concat();
    tkhd.extend([0u8; 16]);
    for x in m.matrix {
        tkhd.extend(x.to_be_bytes());
    }
    tkhd.extend(be(m.size.0 << 16));
    tkhd.extend(be(m.size.1 << 16));
    let mut mdhd = [be(0), be(0), be(600), be(1200)].concat();
    mdhd.extend([0x55, 0xC4, 0, 0]);
    let (handler, entry) = if m.video {
        let mut e = vec![0u8; 6];
        e.extend([0, 1]);
        e.extend([0u8; 16]);
        e.extend((m.size.0 as u16).to_be_bytes());
        e.extend((m.size.1 as u16).to_be_bytes());
        e.extend(be(0x0048_0000));
        e.extend(be(0x0048_0000));
        e.extend([0, 0, 0, 0, 0, 1]);
        e.extend([0u8; 32]);
        e.extend([0, 0x18, 0xFF, 0xFF]);
        (b"vide", bx(b"mp4v", &[&e]))
    } else {
        let mut e = vec![0u8; 6];
        e.extend([0, 1]);
        e.extend([0u8; 8]);
        e.extend([0, 2, 0, 16, 0, 0, 0, 0]);
        e.extend(be(48_000 << 16));
        (b"soun", bx(b"mp4a", &[&e]))
    };
    let mut hdlr = be(0).to_vec();
    hdlr.extend(handler);
    hdlr.extend([0u8; 12]);
    hdlr.extend(b"Handler\0");
    let stbl = bx(b"stbl", &[&full(b"stsd", &[&be(1), &entry])]);
    let mdia = bx(b"mdia", &[&full(b"mdhd", &[&mdhd]), &full(b"hdlr", &[&hdlr]), &bx(b"minf", &[&stbl])]);
    let trak = bx(b"trak", &[&full(b"tkhd", &[&tkhd]), &mdia]);
    let text = |kind: &[u8; 4], s: &str| {
        let mut p = (s.len() as u16).to_be_bytes().to_vec();
        p.extend([0x55, 0xC4]);
        p.extend(s.as_bytes());
        bx(kind, &[&p])
    };
    let atoms: Vec<Vec<u8>> = [m.xyz.map(|x| text(b"\xA9xyz", x)), m.day.map(|d| text(b"\xA9day", d))].into_iter().flatten().collect();
    let udta = bx(b"udta", &atoms.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let mut keys = be(m.keys.len() as u32).to_vec();
    for (k, _) in &m.keys {
        keys.extend(be(8 + k.len() as u32));
        keys.extend(b"mdta");
        keys.extend(k.as_bytes());
    }
    let items: Vec<Vec<u8>> = m
        .keys
        .iter()
        .enumerate()
        .map(|(i, (_, v))| {
            let data = [&be(1)[..], &be(0), v.as_bytes()].concat();
            bx(&be(i as u32 + 1), &[&bx(b"data", &[&data])])
        })
        .collect();
    let mut mh = be(0).to_vec();
    mh.extend(b"mdta");
    mh.extend([0u8; 13]);
    let meta = bx(b"meta", &[&full(b"hdlr", &[&mh]), &full(b"keys", &[&keys]), &bx(b"ilst", &items.iter().map(Vec::as_slice).collect::<Vec<_>>())]);
    let moov = bx(b"moov", &[&full(b"mvhd", &[&mvhd]), &trak, &udta, &meta]);
    [bx(b"ftyp", &[b"qt  ", &[0, 0, 2, 0], b"qt  "]), bx(b"mdat", &[&[0u8; 16]]), moov].concat()
}

/// An iPhone export: the `mvhd` time is the export, the capture time and place are Apple keys,
/// the picture is turned a quarter.
fn iphone() -> Movie {
    Movie {
        created: Some("2026-10-01T08:00:00Z"),
        matrix: QUARTER_TURN,
        size: (1920, 1080),
        video: true,
        keys: vec![("com.apple.quicktime.creationdate", "2026-09-05T18:22:00+0300"), ("com.apple.quicktime.location.ISO6709", "+37.4467+025.3289+012.345/")],
        xyz: None,
        day: None,
    }
}

/// An Android phone video: UTC `mvhd` time and a `©xyz` place.
fn android() -> Movie {
    Movie { created: Some("2026-08-01T10:00:00Z"), matrix: UPRIGHT, size: (1280, 720), video: true, keys: vec![], xyz: Some("+48.8584+002.2945/"), day: None }
}

/// A song: its `©day` is a release date, not a capture time.
fn song() -> Movie {
    Movie { created: Some("2026-07-01T09:00:00Z"), matrix: UPRIGHT, size: (0, 0), video: false, keys: vec![], xyz: None, day: Some("2019-01-01T00:00:00Z") }
}

// ---------- dates and places ----------

#[test]
fn dates_parse_from_every_form_files_use() {
    let rfc = |s: &str| Stamp::parse(s).map(|t| t.rfc3339());
    assert_eq!(rfc("2026:09:05 18:22:00").as_deref(), Some("2026-09-05T18:22:00"));
    assert_eq!(rfc("2026-09-05T18:22:00+0300").as_deref(), Some("2026-09-05T18:22:00+03:00"));
    assert_eq!(rfc("2026-09-05T18:22:00.123456Z").as_deref(), Some("2026-09-05T18:22:00Z"));
    assert_eq!(rfc("2026-09-05 18:22:00-05:30").as_deref(), Some("2026-09-05T18:22:00-05:30"));
    assert_eq!(rfc(" 2026-09-05t18:22:00+03 \0").as_deref(), Some("2026-09-05T18:22:00+03:00"));
    assert_eq!(rfc("2024-02-29T23:59:60Z").as_deref(), Some("2024-02-29T23:59:59Z"));
    assert_eq!(rfc("2026-09-05T18:22:00 (local)").as_deref(), Some("2026-09-05T18:22:00"));
    assert_eq!(Stamp::exif("2026:09:05 18:22:00", Some("+03:00")).map(|t| t.rfc3339()).as_deref(), Some("2026-09-05T18:22:00+03:00"));
    assert_eq!(Stamp::exif("2026:09:05 18:22:00", Some("   :  ")).map(|t| t.rfc3339()).as_deref(), Some("2026-09-05T18:22:00"));
    for bad in [
        "",
        "    :  :     :  :  ",
        "0000:00:00 00:00:00",
        "2026-02-29T00:00:00",
        "2026-13-01T00:00:00",
        "2026-09-31T00:00:00",
        "2026-09-05T24:00:00",
        "2026-09-05T18:60:00",
        "2026-09-05T18:22:61",
        "2026-09-05",
        "2026/09/05 18:22:00",
        "２０２６-09-05T18:22:00",
        "-026-09-05T18:22:00",
    ] {
        assert_eq!(rfc(bad), None, "{bad:?}");
    }
    // the same instant written in two zones; local time counts as UTC
    let at = |s: &str| Stamp::parse(s).unwrap().instant();
    assert_eq!(at("1970-01-01T00:00:00Z"), 0);
    assert_eq!(at("2026-09-05T18:22:00+03:00"), at("2026-09-05T15:22:00Z"));
    assert_eq!(at("2026-09-05T15:22:00"), at("2026-09-05T15:22:00Z"));
    assert_eq!(at("2000-03-01T00:00:00Z") - at("2000-02-28T00:00:00Z"), 2 * 86_400);
    // MP4 times count from 1904 (UTC); unset and pre-1970 times are unknown
    assert_eq!(Stamp::mp4(3_871_466_520).map(|t| t.rfc3339()).as_deref(), Some("2026-09-05T15:22:00Z"));
    for unknown in [0, 1000, MP4_EPOCH_TO_UNIX, u64::MAX] {
        assert_eq!(Stamp::mp4(unknown), None, "{unknown}");
    }
    assert_eq!(rfc3339_utc(0).as_deref(), Some("1970-01-01T00:00:00Z"));
    assert_eq!(rfc3339_utc(MAX_UNIX).as_deref(), Some("9999-12-31T23:59:59Z"));
    assert_eq!(rfc3339_utc(MAX_UNIX + 1), None);
    assert_eq!(parse_date(" 2026-09-05 ").as_deref(), Some("2026-09-05"));
    for bad in ["2026-9-5", "2026-02-30", "05.09.2026", "2026-09-05T10:00:00", "yesterday"] {
        assert_eq!(parse_date(bad), None, "{bad}");
    }
}

#[test]
fn iso6709_positions() {
    let p = |s: &str| parse_iso6709(s);
    assert_eq!(p("+37.4467+025.3289+012.345/"), Some([37.4467, 25.3289]));
    assert_eq!(p("-33.8587-151.2111/"), Some([-33.8587, -151.2111]));
    assert_eq!(p("+48.8584+002.2945CRSWGS_84/"), Some([48.8584, 2.2945]));
    // degrees and minutes; degrees, minutes and seconds
    assert!(near(p("+3726.802+02519.734/").unwrap(), [37.0 + 26.802 / 60.0, 25.0 + 19.734 / 60.0]));
    assert!(near(p("-335131.56-1511240.2/").unwrap(), PHONE_PLACE));
    for bad in [
        "",
        "/",
        "37.4467+025.3289/",
        "+91.0000+000.0000/",
        "+10.0+181.0/",
        "+00.0000+000.0000/",
        "+37.4467",
        "+3790.0+02519.7/",
        "+3.5+25.3/",
        "+37.44.67+025.3289/",
        "+37.4467ä+025.3289/",
    ] {
        assert_eq!(p(bad), None, "{bad:?}");
    }
}

// ---------- capture info ----------

#[test]
fn a_phone_photo_gives_date_offset_place_and_upright_size() {
    let dir = tmp_dir("regie-photo");
    for le in [true, false] {
        let path = dir.join(format!("IMG_{le}.JPG"));
        write(&path, &jpeg(Some(&phone_exif(le)), 4000, 3000));
        let info = capture_info(&path);
        assert_eq!(info.kind, "photo");
        assert_eq!(info.created.as_deref(), Some("2026-09-05T18:22:00+03:00"));
        let place = info.location.unwrap();
        assert!(place[0] < 0.0 && place[1] < 0.0 && near(place, PHONE_PLACE), "{place:?}");
        // stored 4000 × 3000, orientation 6: shown upright 3000 × 4000
        assert_eq!((info.width, info.height, info.orientation), (3000, 4000, 6));
        assert_eq!(info.seconds, None);
        let modified = info.modified.as_deref().unwrap();
        assert!(modified.ends_with('Z') && Stamp::parse(modified).is_some(), "{modified}");
    }
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn stills_and_sounds_give_size_date_and_length() {
    let ext = |f: StillFormat| match f {
        StillFormat::Jpeg => "jpg",
        StillFormat::Png => "png",
        StillFormat::Gif => "gif",
        StillFormat::Webp => "webp",
        StillFormat::Tiff => "tif",
        StillFormat::Bmp => "bmp",
        StillFormat::Tga => "tga",
        StillFormat::Dpx => "dpx",
        StillFormat::Heif => "heic",
    };
    for (bytes, f, w, h) in stills() {
        let info = info_of(&bytes, ext(f));
        assert_eq!(info.kind, "photo", "{f:?}");
        // the JPEG (orientation 6) and the TIFF (orientation 8) stand upright
        let upright = if matches!(f, StillFormat::Jpeg) || (f == StillFormat::Tiff && w == 1200) { (h, w) } else { (w, h) };
        assert_eq!((info.width, info.height), upright, "{f:?}");
    }
    let raw = info_of(&dng(), "dng");
    assert_eq!((raw.width, raw.height, raw.created.as_deref()), (5976, 3984, Some("2025-12-24T17:00:00")));
    let shot = info_of(&png(640, 480, Some(&dated_exif("2026:09:06 10:00:00", None, 1))), "png");
    assert_eq!((shot.width, shot.height, shot.created.as_deref(), shot.orientation), (640, 480, Some("2026-09-06T10:00:00"), 1));
    // DateTimeDigitized stands in for a blank DateTimeOriginal
    let t = super::exif::tests::tiff(
        true,
        &[
            vec![(0x8769, super::exif::tests::Val::Ifd(1))],
            vec![(0x9003, super::exif::tests::Val::Ascii("    :  :     :  :  ")), (0x9004, super::exif::tests::Val::Ascii("2026:09:07 08:00:00"))],
        ],
    );
    assert_eq!(info_of(&jpeg(Some(&t), 8, 8), "jpg").created.as_deref(), Some("2026-09-07T08:00:00"));
    // sounds
    for (bytes, ext, secs) in [(wav_1s(), "wav", 1.0), (rf64(), "wav", 0.1), (aiff(), "aif", 1.0), (flac(), "flac", 2.0)] {
        let info = info_of(&bytes, ext);
        assert_eq!(info.kind, "audio", "{ext}");
        let s = info.seconds.unwrap_or(-1.0);
        assert!((s - secs).abs() < 1e-9, "{ext}: {s}");
    }
    assert_eq!(info_of(b"\xFF\xFB\x90\x00", "mp3"), CaptureInfo { kind: "audio".into(), ..Default::default() });
    // content decides for an unknown extension; nothing for a file that is no media
    assert_eq!(info_of(&jpeg(None, 64, 48), "").kind, "photo");
    assert_eq!(info_of(&wav_1s(), "bin").kind, "audio");
    assert_eq!(info_of(b"packing list", "txt"), CaptureInfo::default());
}

#[test]
fn movies_give_capture_time_place_turned_size_and_length() {
    let i = info_of(&movie(&iphone()), "mov");
    assert_eq!((i.kind.as_str(), i.created.as_deref()), ("video", Some("2026-09-05T18:22:00+03:00")));
    assert!(near(i.location.unwrap(), [37.4467, 25.3289]));
    assert_eq!((i.width, i.height, i.seconds, i.orientation), (1080, 1920, Some(2.0), 0));
    let a = info_of(&movie(&android()), "mp4");
    assert_eq!((a.created.as_deref(), a.width, a.height), (Some("2026-08-01T10:00:00Z"), 1280, 720));
    assert!(near(a.location.unwrap(), [48.8584, 2.2945]));
    let s = info_of(&movie(&song()), "m4a");
    assert_eq!((s.kind.as_str(), s.created.as_deref(), s.width, s.seconds), ("audio", Some("2026-07-01T09:00:00Z"), 0, Some(2.0)));
    // a camera's `©day` with its offset; no time at all
    let cam = Movie { created: None, day: Some("2026-07-14T09:30:00+02:00"), ..android() };
    assert_eq!(info_of(&movie(&cam), "mov").created.as_deref(), Some("2026-07-14T09:30:00+02:00"));
    let none = Movie { created: None, xyz: None, ..android() };
    let n = info_of(&movie(&none), "mov");
    assert_eq!((n.kind.as_str(), n.created, n.location), ("video", None, None));
    // an audio-only .mp4 is sound
    assert_eq!(info_of(&movie(&song()), "mp4").kind, "audio");
}

/// A source that claims to be huge and answers every read with box headers made up on the spot.
struct Endless {
    len: u64,
    /// Answer for offset 0 (then `free` boxes of `step` bytes everywhere).
    first: Vec<u8>,
    step: u32,
}

impl ByteSource for Endless {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut head = [0u8; 16];
        if offset == 0 && !self.first.is_empty() {
            head[..self.first.len().min(16)].copy_from_slice(&self.first[..self.first.len().min(16)]);
        } else {
            head[..4].copy_from_slice(&self.step.to_be_bytes());
            head[4..8].copy_from_slice(b"free");
        }
        let n = buf.len().min(16);
        buf[..n].copy_from_slice(&head[..n]);
        Ok(())
    }
}

#[test]
fn huge_or_endless_mp4_headers_are_not_read() {
    // a 2 GiB `moov`: refused before the demuxer allocates it
    let mut first = (2u32 << 30).to_be_bytes().to_vec();
    first.extend(b"moov");
    let big = Endless { len: 8 << 30, first, step: 8 };
    assert!(!mp4_headers_fit(&big));
    let mut info = CaptureInfo::default();
    read_info(&big, "mov", &mut info);
    assert_eq!(info, CaptureInfo::default());
    // a `moov` claiming 2 GiB in a small file counts what is there
    let mut small = movie(&android());
    let moov = small.windows(4).position(|w| w == b"moov").unwrap() - 4;
    small[moov..moov + 4].copy_from_slice(&(2u32 << 30).to_be_bytes());
    assert!(mp4_headers_fit(small.as_slice()));
    let _ = info_of(&small, "mp4");
    // millions of 8-byte boxes: the walk gives up
    let crowd = Endless { len: 1 << 40, first: Vec::new(), step: 8 };
    assert!(!mp4_headers_fit(&crowd));
    // a 64-bit size in a box cut short, a size below the header
    assert!(!mp4_headers_fit(b"\0\0\0\x01mdat\0\0".as_slice()));
    assert!(!mp4_headers_fit(b"\0\0\0\x04free\0\0\0\0".as_slice()));
}

#[test]
fn what_cannot_be_read_stays_unknown() {
    let dir = tmp_dir("regie-unknown");
    assert_eq!(capture_info(&dir), CaptureInfo::default());
    assert_eq!(capture_info(&dir.join("missing.jpg")), CaptureInfo { kind: "photo".into(), ..Default::default() });
    write(&dir.join("empty.mov"), b"");
    let empty = capture_info(&dir.join("empty.mov"));
    assert_eq!((empty.kind.as_str(), empty.created, empty.width), ("video", None, 0));
    assert!(empty.modified.is_some());
    write(&dir.join("short.jpg"), b"\xFF\xD8\xFF");
    assert_eq!(capture_info(&dir.join("short.jpg")).width, 0);
    std::fs::remove_dir_all(dir).ok();
}

// ---------- ffmpeg-made movies ----------

fn ffmpeg_ok(ff: &Path, args: &[&str]) -> bool {
    std::process::Command::new(ff).args(["-y", "-loglevel", "error"]).args(args).status().is_ok_and(|s| s.success())
}

#[test]
fn ffmpeg_movies_with_apple_and_android_metadata() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let dir = filmcraft_testkit::fixtures_dir("engine/regie");
    let make = |name: &str, args: &[&str]| {
        filmcraft_testkit::fixtures::generate(&dir.join(name), |tmp| ffmpeg_ok(&ff, &[args, &[tmp.to_str().unwrap()][..]].concat()))
    };
    let picture = ["-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25", "-t", "1", "-c:v", "mpeg4"];
    // an iPhone-like export: the `mvhd` time is the export, Apple keys hold capture time and place
    let apple = [
        "-metadata",
        "creation_time=2026-10-01T08:00:00Z",
        "-movflags",
        "use_metadata_tags",
        "-metadata",
        "com.apple.quicktime.creationdate=2026-09-05T18:22:00+0300",
        "-metadata",
        "com.apple.quicktime.location.ISO6709=+37.4467+025.3289+012.345/",
    ];
    let iphone = make("iphone-keys.mov", &[&picture[..], &apple[..]].concat());
    // an Android-like video: a UTC `mvhd` time and `©xyz`
    let android =
        make("android-xyz.mov", &[&picture[..], &["-metadata", "creation_time=2026-08-01T10:00:00Z", "-metadata", "location=+48.8584+002.2945/"][..]].concat());
    let sound = make(
        "voice.m4a",
        &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "1", "-c:a", "aac", "-metadata", "creation_time=2026-07-01T09:00:00Z"],
    );
    let (Some(iphone), Some(android), Some(sound)) = (iphone, android, sound) else {
        filmcraft_testkit::skip("regie ffmpeg movies", "ffmpeg could not make the fixtures");
        return;
    };
    let i = capture_info(&iphone);
    assert_eq!((i.kind.as_str(), i.created.as_deref(), i.width, i.height), ("video", Some("2026-09-05T18:22:00+03:00"), 320, 180), "{i:?}");
    assert!(near(i.location.unwrap(), [37.4467, 25.3289]), "{i:?}");
    assert!(i.seconds.is_some_and(|s| (s - 1.0).abs() < 0.05), "{i:?}");
    let a = capture_info(&android);
    assert_eq!((a.kind.as_str(), a.created.as_deref(), a.width, a.height), ("video", Some("2026-08-01T10:00:00Z"), 320, 180), "{a:?}");
    assert!(near(a.location.unwrap(), [48.8584, 2.2945]), "{a:?}");
    let s = capture_info(&sound);
    assert_eq!((s.kind.as_str(), s.created.as_deref(), s.width), ("audio", Some("2026-07-01T09:00:00Z"), 0), "{s:?}");
    assert!(s.seconds.is_some_and(|x| (x - 1.0).abs() < 0.1), "{s:?}");
    // the same movie turned a quarter, as phones store portrait video (`-display_rotation` needs
    // ffmpeg 6; the hand-made movies cover the rotation otherwise)
    match make("iphone-turned.mov", &["-display_rotation:v:0", "90", "-i", iphone.to_str().unwrap(), "-c", "copy"]) {
        Some(turned) => assert_eq!((capture_info(&turned).width, capture_info(&turned).height), (180, 320)),
        None => eprintln!("NOTE (regie ffmpeg movies): this ffmpeg cannot set a display rotation; not checked against ffmpeg"),
    }
}

// ---------- finding media ----------

/// A holiday folder tree (made under the temp folder):
///
/// ```text
/// Mykonos 2026/IMG_0001.JPG   EXIF 2026-09-05 18:22 +03:00, GPS, orientation 6
/// Mykonos 2026/IMG_0002.jpg   EXIF 2026-09-06 10:00 (no offset)
/// Kreta/beach.png             no date; modified 2025-07-10
/// Kreta/song.wav              1 s; modified 2026-01-01
/// Kreta/.hidden.jpg           hidden
/// Kreta/photo.CR3             camera raw, not importable: skipped
/// .thumbnails/x.jpg, @eaDir/thumb.jpg   hidden / NAS thumbnails
/// notes.txt                   no media
/// a/b/c/d/e/f/g/h/ok.jpg      8 folders deep: found; modified 2020-01-01
/// a/b/c/d/e/f/g/h/i/deep.jpg  9 folders deep: skipped
/// ```
fn holiday_tree(name: &str) -> PathBuf {
    let root = tmp_dir(name);
    write(&root.join("Mykonos 2026/IMG_0001.JPG"), &jpeg(Some(&phone_exif(true)), 4000, 3000));
    write(&root.join("Mykonos 2026/IMG_0002.jpg"), &jpeg(Some(&dated_exif("2026:09:06 10:00:00", None, 1)), 3000, 4000));
    write(&root.join("Kreta/beach.png"), &png(800, 600, None));
    touch(&root.join("Kreta/beach.png"), "2025-07-10T12:00:00Z");
    write(&root.join("Kreta/song.wav"), &wav_1s());
    touch(&root.join("Kreta/song.wav"), "2026-01-01T00:00:00Z");
    write(&root.join("Kreta/.hidden.jpg"), &jpeg(None, 10, 10));
    write(&root.join("Kreta/photo.CR3"), b"\0\0\0\x18ftypcrx ");
    write(&root.join(".thumbnails/x.jpg"), &jpeg(None, 10, 10));
    write(&root.join("@eaDir/thumb.jpg"), &jpeg(None, 10, 10));
    write(&root.join("notes.txt"), b"packing list");
    write(&root.join("a/b/c/d/e/f/g/h/ok.jpg"), &jpeg(None, 640, 480));
    touch(&root.join("a/b/c/d/e/f/g/h/ok.jpg"), "2020-01-01T00:00:00Z");
    write(&root.join("a/b/c/d/e/f/g/h/i/deep.jpg"), &jpeg(None, 640, 480));
    root
}

fn names(r: &FindResult) -> Vec<String> {
    r.candidates.iter().map(|c| Path::new(&c.path).file_name().unwrap().to_string_lossy().into_owned()).collect()
}

fn find(root: &Path, filter: MediaFilter) -> FindResult {
    find_media(&[root.to_path_buf()], &filter, &AtomicBool::new(false))
}

#[test]
fn find_media_by_date_place_and_kind() {
    let root = holiday_tree("regie-find");
    let all = find(&root, MediaFilter::default());
    assert_eq!(names(&all), ["IMG_0001.JPG", "IMG_0002.jpg", "beach.png", "song.wav", "ok.jpg"]);
    assert_eq!((all.scanned, all.skipped, all.matched), (7, 2, 5));
    let first = &all.candidates[0];
    assert_eq!(first.path, root.join("Mykonos 2026/IMG_0001.JPG").to_string_lossy());
    assert_eq!((first.kind.as_str(), first.created.as_deref(), first.folder.as_str()), ("photo", Some("2026-09-05T18:22:00+03:00"), "Mykonos 2026"));
    assert_eq!((first.width, first.height, first.item, first.place.as_deref()), (3000, 4000, None, None));
    assert!(near(first.location.unwrap(), PHONE_PLACE));
    let song = &all.candidates[3];
    assert_eq!((song.kind.as_str(), song.seconds, song.created.as_deref(), song.modified.as_deref()), ("audio", Some(1.0), None, Some("2026-01-01T00:00:00Z")));
    assert_eq!(all.candidates[4].folder, "h");

    let dates =
        |from: Option<&str>, to: Option<&str>| names(&find(&root, MediaFilter { from: from.map(Into::into), to: to.map(Into::into), ..Default::default() }));
    assert_eq!(dates(Some("2026-09-01"), Some("2026-09-30")), ["IMG_0001.JPG", "IMG_0002.jpg"]);
    // the capture's local date: 18:22 at +03:00 on the 5th is the 5th
    assert_eq!(dates(Some("2026-09-06"), None), ["IMG_0002.jpg"]);
    assert_eq!(dates(Some("2026-09-05"), Some("2026-09-05")), ["IMG_0001.JPG"]);
    // without a capture date, the modification date
    assert_eq!(dates(None, Some("2025-12-31")), ["beach.png", "ok.jpg"]);
    assert_eq!(dates(Some("2026-01-01"), Some("2026-01-01")), ["song.wav"]);
    // a date that is no date is ignored here (the command refuses it)
    assert_eq!(dates(Some("soon"), None).len(), 5);

    let places = |words: &[&str]| names(&find(&root, MediaFilter { places: words.iter().map(|w| w.to_string()).collect(), ..Default::default() }));
    assert_eq!(places(&["mykonos"]), ["IMG_0001.JPG", "IMG_0002.jpg"]);
    assert_eq!(places(&["BEACH"]), ["beach.png"]);
    assert_eq!(places(&["Kreta", "MYKONOS"]), ["IMG_0001.JPG", "IMG_0002.jpg", "beach.png", "song.wav"]);
    assert_eq!(places(&["Rhodos"]), Vec::<String>::new());
    // the searched folder's own name and those above it do not count
    let root_name = root.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(places(&[root_name.as_str()]), Vec::<String>::new());

    let kinds = |k: &[&str]| names(&find(&root, MediaFilter { kinds: k.iter().map(|w| w.to_string()).collect(), ..Default::default() }));
    assert_eq!(kinds(&["audio"]), ["song.wav"]);
    assert_eq!(kinds(&["photo"]), ["IMG_0001.JPG", "IMG_0002.jpg", "beach.png", "ok.jpg"]);
    assert_eq!(kinds(&["video"]), Vec::<String>::new());

    // the limit keeps the earliest; `matched` still counts all
    let one = find(&root, MediaFilter { limit: 1, ..Default::default() });
    assert_eq!((names(&one), one.matched), (vec!["IMG_0001.JPG".to_string()], 5));
    let none = find(&root, MediaFilter { limit: 0, ..Default::default() });
    assert_eq!((none.candidates.len(), none.matched, none.scanned), (0, 5, 7));
    std::fs::remove_dir_all(root).ok();
}

/// An iPhone photo as HEIC: a 4032 × 3024 picture turned a quarter by `irot` (a portrait photo)
/// with its EXIF in an `Exif` item.
fn heic(exif: Option<&[u8]>) -> Vec<u8> {
    use filmcraft_testkit::heif as w;
    let mut h = w::Heif::new(b"heic");
    let size = h.property(w::ispe(4032, 3024));
    let turn = h.property(w::irot(3));
    let config = h.property(w::hvcc(4, 1, 8, &[], &[], &[]));
    h.item(w::Item::new(1, b"hvc1", vec![0; 64]).with(config, true).with(size, false).with(turn, true));
    h.primary = Some(1);
    if let Some(tiff) = exif {
        h.item(w::Item::new(2, b"Exif", w::exif(tiff)).hidden());
        h.reference(b"cdsc", 2, &[1]);
    }
    h.build()
}

#[test]
fn an_iphone_heic_gives_date_place_and_upright_size() {
    let root = tmp_dir("regie-heic");
    write(&root.join("IMG_0001.HEIC"), &heic(Some(&phone_exif(true))));
    write(&root.join("IMG_0002.heic"), &heic(None));
    let i = capture_info(&root.join("IMG_0001.HEIC"));
    assert_eq!((i.kind.as_str(), i.width, i.height), ("photo", 3024, 4032), "{i:?}");
    // the import turns it by `irot`: the EXIF orientation (6) must not turn it again
    assert_eq!(i.orientation, 0);
    assert_eq!(i.created.as_deref(), Some("2026-09-05T18:22:00+03:00"));
    assert!(near(i.location.unwrap(), PHONE_PLACE));
    // without EXIF: size only
    let j = capture_info(&root.join("IMG_0002.heic"));
    assert_eq!((j.width, j.height, j.created.as_deref()), (3024, 4032, None));
    // both are found as photos, not skipped
    let r = find(&root, MediaFilter::default());
    assert_eq!((names(&r), r.skipped), (vec!["IMG_0001.HEIC".to_string(), "IMG_0002.heic".to_string()], 0));
}

#[test]
fn screenshots_and_screen_recordings_are_left_out_unless_asked_for() {
    let root = tmp_dir("regie-screenshots");
    write(&root.join("IMG_0001.jpg"), &jpeg(Some(&phone_exif(true)), 4000, 3000));
    write(&root.join("Bildschirmfoto vom 2026-09-26 08-34-36.png"), &png(1920, 1080, None));
    write(&root.join("Screenshot_20260101-120000.png"), &png(1080, 2400, None));
    write(&root.join("Screen Recording 2026-01-01 at 12.00.00.mov"), &movie(&iphone()));
    write(&root.join("Bildschirmaufnahme 2026-01-01.mp4"), &movie(&android()));
    // a sound is never a screenshot, whatever its name
    write(&root.join("Screenshot sound.wav"), &wav_1s());
    let r = find(&root, MediaFilter::default());
    assert_eq!(names(&r), ["IMG_0001.jpg", "Screenshot sound.wav"]);
    assert_eq!((r.screenshots, r.skipped), (4, 0));
    let all = find(&root, MediaFilter { screenshots: true, ..Default::default() });
    assert_eq!((all.candidates.len(), all.screenshots), (6, 0));
}

#[test]
fn find_media_searches_each_folder_once_and_stops_when_cancelled() {
    let root = holiday_tree("regie-roots");
    // a folder inside another, the same folder twice, written differently
    let twice = find_media(&[root.join("Kreta"), root.clone(), root.join("a/../Kreta"), root.join(".")], &MediaFilter::default(), &AtomicBool::new(false));
    assert_eq!(names(&twice), ["IMG_0001.JPG", "IMG_0002.jpg", "beach.png", "song.wav", "ok.jpg"]);
    let stopped = find_media(std::slice::from_ref(&root), &MediaFilter::default(), &AtomicBool::new(true));
    assert_eq!((stopped.candidates.len(), stopped.scanned), (0, 0));
    // a folder that does not exist or cannot be read
    let missing = find_media(&[root.join("nowhere")], &MediaFilter::default(), &AtomicBool::new(false));
    assert_eq!((missing.candidates.len(), missing.skipped), (0, 1));
    // a plan's filter: its dates, places and photos only
    let plan = Plan { from: Some("2026-09-01".into()), places: vec!["mykonos".into()], videos: false, ..Plan::default() };
    let f = MediaFilter::from_plan(&plan);
    assert_eq!((f.kinds.as_slice(), f.limit), (&["photo".to_string()][..], DEFAULT_LIMIT));
    assert_eq!(names(&find(&root, f)), ["IMG_0001.JPG", "IMG_0002.jpg"]);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn links_to_folders_are_skipped_links_to_files_followed() {
    let root = tmp_dir("regie-links");
    let elsewhere = tmp_dir("regie-links-target");
    write(&elsewhere.join("far.jpg"), &jpeg(None, 64, 48));
    std::os::unix::fs::symlink(&elsewhere, root.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("far.jpg"), root.join("near.jpg")).unwrap();
    std::os::unix::fs::symlink(root.join("gone.jpg"), root.join("dangling.jpg")).unwrap();
    let r = find(&root, MediaFilter::default());
    assert_eq!(names(&r), ["near.jpg"]);
    assert_eq!((r.scanned, r.skipped), (1, 2));
    assert_eq!((r.candidates[0].width, r.candidates[0].height), (64, 48));
    // a photo nobody may read is skipped (unless the tests run as root, who reads everything)
    use std::os::unix::fs::PermissionsExt;
    let locked = root.join("locked.jpg");
    write(&locked, &jpeg(None, 64, 48));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&locked).is_err() {
        let r = find(&root, MediaFilter::default());
        assert_eq!((names(&r), r.scanned, r.skipped), (vec!["near.jpg".to_string()], 2, 3));
        assert_eq!(capture_info(&locked), CaptureInfo { kind: "photo".into(), modified: capture_info(&locked).modified, ..Default::default() });
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_dir_all(root).ok();
    std::fs::remove_dir_all(elsewhere).ok();
}

#[test]
fn user_folders_come_from_user_dirs() {
    let home = tmp_dir("regie-home");
    for d in ["Bilder", "Videos", "Musik", "Pictures", "Music", "Schreibtisch", "Fotos absolut"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    let config = home.join(".config");
    let dirs = config.join("user-dirs.dirs");
    write(
        &dirs,
        b"# This file is written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\nXDG_VIDEOS_DIR=\"$HOME/Videos\"\nXDG_MUSIC_DIR=\"$HOME/Musik\"\n",
    );
    assert_eq!(user_media_folders(&home, &config), [home.join("Bilder"), home.join("Videos"), home.join("Musik")]);
    // an absolute path; `$HOME` alone switches a folder off and a missing one falls back
    let abs = home.join("Fotos absolut");
    write(&dirs, format!("XDG_PICTURES_DIR=\"{}\"\nXDG_VIDEOS_DIR=\"$HOME/\"\nXDG_MUSIC_DIR=\"${{HOME}}/Gibt es nicht\"\n", abs.display()).as_bytes());
    assert_eq!(user_media_folders(&home, &config), [abs, home.join("Videos"), home.join("Music")]);
    // no user-dirs.dirs: the English names that exist
    std::fs::remove_file(&dirs).unwrap();
    assert_eq!(user_media_folders(&home, &config), [home.join("Pictures"), home.join("Videos"), home.join("Music")]);
    // garbage in the file
    write(&dirs, &[0xFF, 0xFE, b'=', b'\n', b'X', b'=', b'"', b'/', b'"']);
    assert_eq!(user_media_folders(&home, &config).len(), 3);
    std::fs::remove_dir_all(home).ok();
}

// ---------- commands ----------

#[test]
fn commands_report_capture_info_and_found_media() {
    let root = holiday_tree("regie-commands");
    let mut s = Session::default();
    let img = root.join("Mykonos 2026/IMG_0001.JPG");
    let v = s.execute("media.captureInfo", json!({"path": img})).unwrap();
    assert_eq!((v["kind"].as_str(), v["created"].as_str(), v["path"].as_str()), (Some("photo"), Some("2026-09-05T18:22:00+03:00"), img.to_str()));
    assert_eq!((v["width"].as_u64(), v["height"].as_u64(), v["orientation"].as_u64()), (Some(3000), Some(4000), Some(6)));
    assert!(v["modified"].is_string() && v["seconds"].is_null() && v.get("item").is_none(), "{v}");
    assert!(near([v["location"][0].as_f64().unwrap(), v["location"][1].as_f64().unwrap()], PHONE_PLACE));
    // a project item's file
    let r = s.execute("file.import", json!({"paths": [root.join("Kreta/song.wav")]})).unwrap();
    let id = r["items"][0].as_u64().unwrap();
    let v = s.execute("media.captureInfo", json!({"item": id})).unwrap();
    assert_eq!((v["kind"].as_str(), v["seconds"].as_f64(), v["item"].as_u64()), (Some("audio"), Some(1.0), Some(id)));
    // `~` and a relative path are not needed here: the result names the absolute file
    let v = s.execute("media.captureInfo", json!({"path": root.join("Kreta/../Kreta/beach.png")})).unwrap();
    assert_eq!((v["kind"].as_str(), v["width"].as_u64(), v["created"].as_str()), (Some("photo"), Some(800), None));
    assert!(v["path"].as_str().is_some_and(|p| p.ends_with("beach.png")), "{v}");
    // regie.findMedia: plural kinds, one place as text, a limit
    let v = s.execute("regie.findMedia", json!({"folders": [root], "kinds": ["photos"], "places": "mykonos", "limit": 1})).unwrap();
    assert_eq!((v["matched"].as_u64(), v["scanned"].as_u64(), v["skipped"].as_u64()), (Some(2), Some(7), Some(2)));
    assert_eq!(v["folders"], json!([root]));
    let c = &v["candidates"].as_array().unwrap()[0];
    assert_eq!((c["kind"].as_str(), c["folder"].as_str(), c["width"].as_u64()), (Some("photo"), Some("Mykonos 2026"), Some(3000)));
    assert!(c["item"].is_null() && c["place"].is_null() && c["modified"].is_string(), "{c}");
    let back: Candidate = serde_json::from_value(c.clone()).unwrap();
    assert_eq!(back.created.as_deref(), Some("2026-09-05T18:22:00+03:00"));
    // dates, a huge limit (capped)
    let v = s.execute("regie.findMedia", json!({"folders": [root], "from": "2026-09-01", "to": "2026-09-30", "limit": 1e300})).unwrap();
    assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
    // places and kinds as one text with commas
    let v = s.execute("regie.findMedia", json!({"folders": [root], "places": "Rhodos, Kreta", "kinds": "photo, audio"})).unwrap();
    assert_eq!(v["matched"].as_u64(), Some(2), "{v}");
    // pure queries: nothing in the journal, nothing to undo
    assert!(s.journal.iter().all(|(id, _)| id != "regie.findMedia" && id != "media.captureInfo"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hostile_parameters_are_errors_not_panics() {
    let root = holiday_tree("regie-hostile");
    let mut s = Session::default();
    let seq = s.execute("file.newSequence", json!({"name": "S"})).ok().and_then(|v| v["item"].as_u64().or(v.as_u64())).unwrap_or(0);
    let huge = "x".repeat(100_000);
    let folder = root.to_string_lossy().into_owned();
    let errors: Vec<(&str, Value)> = vec![
        ("media.captureInfo", json!(null)),
        ("media.captureInfo", json!([1, 2])),
        ("media.captureInfo", json!({})),
        ("media.captureInfo", json!({"path": 5})),
        ("media.captureInfo", json!({"path": ""})),
        ("media.captureInfo", json!({"path": "   "})),
        ("media.captureInfo", json!({"path": {"a": 1}})),
        ("media.captureInfo", json!({"path": "/definitely/not/here.jpg"})),
        ("media.captureInfo", json!({"path": folder})),
        ("media.captureInfo", json!({"path": huge})),
        ("media.captureInfo", json!({"item": "x"})),
        ("media.captureInfo", json!({"item": -1})),
        ("media.captureInfo", json!({"item": 1.5})),
        ("media.captureInfo", json!({"item": u64::MAX})),
        ("media.captureInfo", json!({"item": seq})),
        ("regie.findMedia", json!("everything")),
        ("regie.findMedia", json!([folder])),
        ("regie.findMedia", json!({"folders": 5})),
        ("regie.findMedia", json!({"folders": [1]})),
        ("regie.findMedia", json!({"folders": ["/definitely/not/here"]})),
        ("regie.findMedia", json!({"folders": [folder, huge]})),
        ("regie.findMedia", json!({"folders": vec![folder.clone(); 300]})),
        ("regie.findMedia", json!({"folders": [folder], "from": "2026-13-01"})),
        ("regie.findMedia", json!({"folders": [folder], "screenshots": "yes"})),
        ("regie.findMedia", json!({"folders": [folder], "from": "2026-02-30"})),
        ("regie.findMedia", json!({"folders": [folder], "from": 20260101})),
        ("regie.findMedia", json!({"folders": [folder], "to": "yesterday"})),
        ("regie.findMedia", json!({"folders": [folder], "to": huge})),
        ("regie.findMedia", json!({"folders": [folder], "from": "2026-12-01", "to": "2026-01-01"})),
        ("regie.findMedia", json!({"folders": [folder], "kinds": ["film"]})),
        ("regie.findMedia", json!({"folders": [folder], "kinds": 3})),
        ("regie.findMedia", json!({"folders": [folder], "kinds": [null]})),
        ("regie.findMedia", json!({"folders": [folder], "places": [null]})),
        ("regie.findMedia", json!({"folders": [folder], "places": {"a": 1}})),
        ("regie.findMedia", json!({"folders": [folder], "places": vec!["x"; 1000]})),
        ("regie.findMedia", json!({"folders": [folder], "places": "a,".repeat(300)})),
        ("regie.findMedia", json!({"folders": [folder, "/not/here/1", "/not/here/2", "/not/here/3", "/not/here/4", huge]})),
        ("regie.findMedia", json!({"folders": [folder], "limit": -1})),
        ("regie.findMedia", json!({"folders": [folder], "limit": "many"})),
        ("regie.findMedia", json!({"folders": [folder], "limit": true})),
        ("regie.findMedia", json!({"folders": [folder], "limit": [5]})),
    ];
    for (cmd, p) in errors {
        let shown = p.to_string().chars().take(80).collect::<String>();
        match s.execute(cmd, p) {
            Err(crate::EngineError::BadParams { msg, .. }) => assert!(msg.chars().count() < 400, "{cmd} {shown}: {msg}"),
            other => panic!("{cmd} {shown}: expected a parameter error, got {other:?}"),
        }
    }
    // clamped and lenient, not refused
    for p in [
        json!({"folders": [folder], "limit": 1e300}),
        json!({"folders": [folder], "limit": 2.7}),
        json!({"folders": folder, "kinds": "Video", "places": "", "from": "", "to": null}),
        json!({"folders": [folder, folder], "places": ["", "  "]}),
    ] {
        assert!(s.execute("regie.findMedia", p.clone()).is_ok(), "{p}");
    }
    std::fs::remove_dir_all(root).ok();
}

// ---------- robustness ----------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Every header reader on mutated files (truncated, bit-flipped, overwritten with offsets that
/// point anywhere, spliced): never a panic, and whatever comes out is in range.
#[test]
fn mutated_headers_never_panic() {
    let ext = |f: StillFormat| format!("{f:?}").to_lowercase();
    let mut seeds: Vec<(Vec<u8>, String)> = stills().into_iter().map(|(b, f, _, _)| (b, ext(f))).collect();
    seeds.extend([
        (jpeg(Some(&phone_exif(true)), 4000, 3000), "jpg".to_string()),
        (png(64, 64, Some(&phone_exif(false))), "png".into()),
        (wav_1s()[..200].to_vec(), "wav".into()),
        (rf64()[..120].to_vec(), "wav".into()),
        (aiff(), "aif".into()),
        (flac(), "flac".into()),
        (movie(&iphone()), "mov".into()),
        (movie(&android()), "mp4".into()),
        (movie(&song()), "m4a".into()),
    ]);
    let formats =
        [StillFormat::Jpeg, StillFormat::Png, StillFormat::Gif, StillFormat::Webp, StillFormat::Tiff, StillFormat::Bmp, StillFormat::Tga, StillFormat::Dpx];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut useful = 0;
    for i in 0..6000 {
        let (seed, ext) = &seeds[i % seeds.len()];
        let mut d = seed.clone();
        for _ in 0..1 + rng.below(3) {
            match rng.below(6) {
                0 => {
                    let p = rng.below(d.len());
                    d[p] ^= 1 << rng.below(8);
                }
                1 => {
                    let p = rng.below(d.len());
                    d[p] = rng.next() as u8;
                }
                // a 32-bit field (offset, count, size) set to a random or extreme value
                2 if d.len() >= 4 => {
                    let p = rng.below(d.len() - 3);
                    let v = match rng.below(4) {
                        0 => u32::MAX,
                        1 => 8,
                        2 => rng.below(d.len() + 64) as u32,
                        _ => rng.next() as u32,
                    };
                    d[p..p + 4].copy_from_slice(&if rng.below(2) == 0 { v.to_le_bytes() } else { v.to_be_bytes() });
                }
                3 => d.truncate(rng.below(d.len() + 1)),
                4 => {
                    let p = rng.below(d.len() + 1);
                    let extra: Vec<u8> = (0..1 + rng.below(16)).map(|_| rng.next() as u8).collect();
                    d.splice(p..p, extra);
                }
                _ => {
                    let a = rng.below(d.len() + 1);
                    let b = rng.below(d.len() + 1);
                    d.drain(a.min(b)..a.max(b));
                }
            }
            if d.is_empty() {
                break;
            }
        }
        let r = catch_unwind(AssertUnwindSafe(|| {
            let info = info_of(&d, ext);
            if let Some([lat, lon]) = info.location {
                assert!(lat.abs() <= 90.0 && lon.abs() <= 180.0, "{lat} {lon}");
            }
            if let Some(c) = &info.created {
                assert!(Stamp::parse(c).is_some(), "{c}");
            }
            assert!(info.seconds.is_none_or(|s| s.is_finite() && s >= 0.0));
            for f in formats {
                let _ = super::exif::still(d.as_slice(), f);
            }
            for base in [0, 6, 12] {
                let _ = super::exif::read_exif(d.as_slice(), base);
            }
            let _ = (wav_seconds(d.as_slice()), aiff_seconds(d.as_slice()), flac_seconds(d.as_slice()), mp4_headers_fit(d.as_slice()));
            let text = String::from_utf8_lossy(&d);
            let _ = (Stamp::parse(&text), parse_iso6709(&text), parse_date(&text));
            info.width > 0 || info.created.is_some() || info.seconds.is_some() || info.location.is_some()
        }));
        match r {
            Ok(found) => useful += usize::from(found),
            Err(_) => panic!("panic on mutation {i} of a {ext} file"),
        }
    }
    // most mutants still carry something the readers find: the damage reaches deep into them
    assert!(useful > 2000, "only {useful} of 6000 mutants gave any information");
}
