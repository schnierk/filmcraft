//! `media.captureInfo` and `regie.findMedia`: find the user's photos, videos and music and when,
//! where and how big each one was captured (see [the module overview](super)).
//!
//! - **MP4 / MOV / M4V / 3GP / M4A:** FilmCraft's own demuxer reads the `moov` box only
//!   ([`filmcraft_isobmff::open_headers`]: no sample tables, fragments or media data; a pre-scan
//!   of the top-level boxes refuses a `moov` beyond [`MAX_MP4_HEADERS`]). The capture time is Apple's
//!   `com.apple.quicktime.creationdate` (with the local offset; an iPhone export's `mvhd` time is
//!   the export), else a video's `©day`, else the `mvhd` creation time (UTC). The place is
//!   `com.apple.quicktime.location.ISO6709` or Android's `©xyz` (ISO 6709). The size is the
//!   first video track's presentation size with its rotation applied.
//! - **Photos:** EXIF (DateTimeOriginal + OffsetTimeOriginal, else DateTimeDigitized, GPS,
//!   Orientation) from JPEG, PNG and TIFF / DNG, and the size of every still format FilmCraft
//!   imports ([`exif`]).
//! - **Sound:** the length from WAV / RF64 / BWF, AIFF and FLAC headers and the MP4 demuxer;
//!   MP3, Ogg, Opus and AAC streams leave it unknown.
//!
//! Written from the specifications only: ISO/IEC 14496-12 and Apple's QuickTime File Format
//! (metadata keys), ISO 6709, CIPA DC-008 and TIFF 6.0 (see [`exif`] for the still formats),
//! RIFF WAVE with EBU Tech 3306 (RF64), AIFF 1.3 and RFC 9639 (FLAC).
//!
//! Only `std::fs` is used (no platform API), so this builds for the web, where there is no
//! filesystem and every answer is "unknown".

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use filmcraft_isobmff::{ByteSource, Mp4File, TrackKind};
use filmcraft_project::{ItemId, MediaRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};

use super::types::{Candidate, Plan};

#[path = "exif.rs"]
pub(crate) mod exif;

use exif::StillFormat;

/// How deep below a searched folder [`find_media`] looks (a folder 8 levels down is searched,
/// one 9 levels down is skipped).
pub const MAX_DEPTH: usize = 8;
/// How many files [`find_media`] looks at, at most, in one search.
pub const MAX_FILES: u64 = 50_000;
/// How many candidates [`find_media`] returns when the filter names no `limit`.
pub const DEFAULT_LIMIT: usize = 10_000;
/// Most bytes of an MP4 `moov` box read for capture info (a day of 4K video needs a few MB).
pub const MAX_MP4_HEADERS: u64 = 64 << 20;
/// Most top-level boxes of an MP4 file walked before giving up on it.
const MAX_TOP_BOXES: usize = 200_000;
/// Most entries of one folder looked at.
const MAX_DIR_ENTRIES: usize = 100_000;
/// Most folders one `regie.findMedia` call may name.
const MAX_FOLDERS: usize = 256;
/// Most place words or kinds one call may name.
const MAX_WORDS: usize = 256;
/// Seconds from 1904-01-01 (the MP4 epoch) to 1970-01-01.
const MP4_EPOCH_TO_UNIX: u64 = 2_082_844_800;
/// 9999-12-31T23:59:59Z in Unix seconds: later times are not dates we can write.
const MAX_UNIX: u64 = 253_402_300_799;

/// Photo and video formats FilmCraft does not import. Files of these kinds are counted as
/// skipped, so a search can say that some media could not be used.
const UNSUPPORTED: &[&str] = &[
    "heic", "heif", "hif", "avif", "jxl", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "srw", "x3f", "3fr", "iiq",
    "erf", "kdc", "dcr", "mrw", "wmv", "asf", "flv", "f4v", "3g2", "ogv", "rm", "rmvb", "divx", "wma", "amr", "ape", "wv",
];

/// How screenshots and screen recordings are named by default (desktop, phone and macOS tools, in
/// English and German), compared with the start of a file name in lower case. They are not the
/// photos and videos a film is made of, and the picture folder is often full of them.
const SCREENSHOT_NAMES: &[&str] = &["screenshot", "screen shot", "screen recording", "bildschirmfoto", "bildschirmaufnahme", "bildschirmaufzeichnung"];

/// Folder names that hold no user media (NAS thumbnail and recycle folders, system folders).
const SYSTEM_FOLDERS: &[&str] = &["@eaDir", "#recycle", "#snapshot", "$RECYCLE.BIN", "System Volume Information", "lost+found"];

// ---------- capture info ----------

/// When, where and how big one media file is, from its own metadata (`media.captureInfo`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CaptureInfo {
    /// "video", "photo" or "audio"; "" when FilmCraft does not import the file. Taken from the
    /// extension (FilmCraft's import lists), except that an MP4 / MOV without a video track is
    /// "audio", and a file with an unknown extension is classified by its content.
    pub kind: String,
    /// When it was captured, RFC 3339: with the camera's UTC offset when the file gives one
    /// (`2026-09-05T18:22:00+03:00`), in UTC (`…Z`) when only a UTC time is known (an MP4
    /// `mvhd` time), without an offset when the camera wrote its local time alone (EXIF without
    /// OffsetTimeOriginal). `None`: unknown (never the file's modification time).
    pub created: Option<String>,
    /// The file's modification time, RFC 3339 UTC.
    pub modified: Option<String>,
    /// Where it was captured: [latitude, longitude] in degrees, south and west negative.
    pub location: Option<[f64; 2]>,
    /// Picture size as displayed (EXIF orientation or the track's rotation applied); 0 when unknown.
    pub width: u32,
    pub height: u32,
    /// Length of a video or sound file in seconds.
    pub seconds: Option<f64>,
    /// A photo's EXIF orientation: 1 = upright as stored, 3 = turned 180°, 6 = to be turned 90°
    /// clockwise, 8 = 90° counter-clockwise, 2 / 4 / 5 / 7 mirrored; 0 when the file has none.
    /// FilmCraft's still import shows pixels as stored, so 3, 6 and 8 need a turn to stand
    /// upright. Videos report 0: the importer applies their rotation itself.
    pub orientation: u8,
}

/// Capture date, place, size and length of the media file at `path`, from its own metadata. Never
/// fails: what cannot be read stays unknown (`kind` is "" for a path that is no file FilmCraft
/// imports). Reads only headers: memory stays bounded whatever the file's size.
pub fn capture_info(path: &Path) -> CaptureInfo {
    probe(path).0
}

/// [`capture_info`], and whether the file could be opened (false: what is known without
/// reading it).
fn probe(path: &Path) -> (CaptureInfo, bool) {
    let ext = extension(path);
    let mut info = CaptureInfo { kind: kind_of_extension(&ext).unwrap_or_default().to_string(), ..Default::default() };
    let Ok(meta) = std::fs::metadata(path) else { return (info, false) };
    if !meta.is_file() {
        return (CaptureInfo::default(), false);
    }
    info.modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).and_then(|d| rfc3339_utc(d.as_secs()));
    let Ok(file) = std::fs::File::open(path) else { return (info, false) };
    let src = FileSource { file: RefCell::new(file), len: meta.len() };
    read_info(&src, &ext, &mut info);
    (info, true)
}

/// Fill `info` from a file's bytes (`ext`: its lower-case extension, which picks Targa, a
/// format without a magic number).
pub(crate) fn read_info<S: ByteSource + ?Sized>(src: &S, ext: &str, info: &mut CaptureInfo) {
    let head = exif::read_upto(src, 0, 32, 32).unwrap_or_default();
    let format = sniff(&head, ext);
    match format {
        Format::Mp4 => mp4_info(src, info),
        Format::Still(f) => apply_still(info, exif::still(src, f)),
        Format::Wav => info.seconds = wav_seconds(src),
        Format::Aiff => info.seconds = aiff_seconds(src),
        Format::Flac => info.seconds = flac_seconds(src),
        Format::Other => {}
    }
    if info.kind.is_empty() {
        info.kind = match format {
            Format::Still(_) => "photo",
            Format::Wav | Format::Aiff | Format::Flac => "audio",
            Format::Mp4 | Format::Other => "",
        }
        .to_string();
    }
}

/// A file's lower-case extension ("" when it has none).
fn extension(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase()
}

/// Whether a file's name says it is a screenshot or a screen recording ([`SCREENSHOT_NAMES`]).
fn is_screenshot(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    SCREENSHOT_NAMES.iter().any(|s| name.starts_with(s))
}

/// The kind FilmCraft's import gives a file with extension `ext` (lower case).
fn kind_of_extension(ext: &str) -> Option<&'static str> {
    if filmcraft_media::VIDEO_EXTENSIONS.contains(&ext) {
        Some("video")
    } else if filmcraft_media::STILL_EXTENSIONS.contains(&ext) {
        Some("photo")
    } else if filmcraft_media::AUDIO_EXTENSIONS.contains(&ext) {
        Some("audio")
    } else {
        None
    }
}

/// Extensions of ISO BMFF files: their kind comes from their tracks, not the extension.
fn is_mp4_extension(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "mov" | "3gp" | "m4a")
}

/// What a file holds, from its first bytes (and, for Targa and BMP, its extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Mp4,
    Still(StillFormat),
    Wav,
    Aiff,
    Flac,
    Other,
}

fn sniff(h: &[u8], ext: &str) -> Format {
    let at = |a: usize, b: usize| h.get(a..b).unwrap_or_default();
    if h.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Format::Still(StillFormat::Jpeg)
    } else if h.starts_with(b"\x89PNG\r\n\x1a\n") {
        Format::Still(StillFormat::Png)
    } else if h.starts_with(b"GIF8") {
        Format::Still(StillFormat::Gif)
    } else if h.starts_with(b"RIFF") && at(8, 12) == b"WEBP" {
        Format::Still(StillFormat::Webp)
    } else if (h.starts_with(b"RIFF") || h.starts_with(b"RF64") || h.starts_with(b"BW64")) && at(8, 12) == b"WAVE" {
        Format::Wav
    } else if h.starts_with(b"FORM") && (at(8, 12) == b"AIFF" || at(8, 12) == b"AIFC") {
        Format::Aiff
    } else if h.starts_with(b"II*\0") || h.starts_with(b"MM\0*") {
        Format::Still(StillFormat::Tiff)
    } else if h.starts_with(b"SDPX") || h.starts_with(b"XPDS") {
        Format::Still(StillFormat::Dpx)
    } else if h.starts_with(b"fLaC") || (h.starts_with(b"ID3") && ext == "flac") {
        Format::Flac
    } else if matches!(at(4, 8), b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip" | b"pnot") {
        Format::Mp4
    } else if h.starts_with(b"BM") && ext == "bmp" {
        Format::Still(StillFormat::Bmp)
    } else if ext == "tga" {
        Format::Still(StillFormat::Tga)
    } else {
        Format::Other
    }
}

/// Size (as displayed), capture time and place of a photo.
fn apply_still(info: &mut CaptureInfo, s: exif::Still) {
    let orientation = s.exif.orientation.unwrap_or(0);
    // orientations 5–8 turn the picture a quarter: width and height swap
    (info.width, info.height) = if (5..=8).contains(&orientation) { (s.height, s.width) } else { (s.width, s.height) };
    info.orientation = u8::try_from(orientation).unwrap_or(0);
    info.created =
        [s.exif.original, s.exif.digitized].into_iter().flatten().find_map(|(date, offset)| Stamp::exif(&date, offset.as_deref())).map(|t| t.rfc3339());
    info.location = s.exif.location;
}

/// A file read at arbitrary offsets by seek + read (no platform-specific API, so it builds for
/// the web too).
struct FileSource {
    file: RefCell<std::fs::File>,
    len: u64,
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let end = offset.checked_add(buf.len() as u64).ok_or(std::io::ErrorKind::UnexpectedEof)?;
        if end > self.len {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let mut f = self.file.try_borrow_mut().map_err(|_| std::io::Error::other("the file is being read"))?;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }
}

// ---------- MP4 / MOV ----------

/// Whether the `moov` box the demuxer reads into memory stays within [`MAX_MP4_HEADERS`] and the
/// file has at most [`MAX_TOP_BOXES`] top-level boxes, so a damaged or hostile size field can't
/// make a capture-date lookup allocate a gigabyte or crawl.
fn mp4_headers_fit<S: ByteSource + ?Sized>(src: &S) -> bool {
    let len = src.len();
    let mut off = 0u64;
    for _ in 0..MAX_TOP_BOXES {
        let left = len.saturating_sub(off);
        if left < 8 {
            return true;
        }
        let Some(h) = exif::read_upto(src, off, 16, 16) else { return false };
        let (Some(size32), Some(kind)) = (h.get(0..4).and_then(|b| <[u8; 4]>::try_from(b).ok()), h.get(4..8)) else { return false };
        let size = match u32::from_be_bytes(size32) {
            0 => left,
            1 => match h.get(8..16).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
                Some(b) => u64::from_be_bytes(b),
                None => return false,
            },
            s => u64::from(s),
        };
        if size < 8 {
            return false;
        }
        if kind == b"moov" && size.min(left) > MAX_MP4_HEADERS {
            return false;
        }
        off = off.saturating_add(size);
    }
    false
}

/// Kind, size, length, capture time and place of an MP4 / MOV file.
fn mp4_info<S: ByteSource + ?Sized>(src: &S, info: &mut CaptureInfo) {
    if !mp4_headers_fit(src) {
        return;
    }
    let Ok(f) = filmcraft_isobmff::open_headers(src) else { return };
    let video = f.tracks.iter().find(|t| t.kind == TrackKind::Video && !t.entries.is_empty());
    if let Some(t) = video {
        info.kind = "video".into();
        // the presentation size (pixel aspect applied), else the sample entry's pixel size
        let (w, h) = if t.width > 0 && t.height > 0 { (t.width, t.height) } else { t.video().map_or((0, 0), |v| (u32::from(v.width), u32::from(v.height))) };
        (info.width, info.height) = if t.display_rotation().is_some_and(|r| r % 2 == 1) { (h, w) } else { (w, h) };
    } else if f.tracks.iter().any(|t| t.kind == TrackKind::Audio && !t.entries.is_empty()) {
        info.kind = "audio".into();
    }
    info.seconds = mp4_seconds(&f);
    let md = &f.metadata;
    let apple = md.get("com.apple.quicktime.creationdate").and_then(Stamp::parse);
    // `©day` of a song is its release date, not a capture time
    let day = video.and_then(|_| md.get("©day")).and_then(Stamp::parse);
    info.created = apple.or(day).or_else(|| Stamp::mp4(f.creation_time)).map(|t| t.rfc3339());
    info.location = md.get("com.apple.quicktime.location.ISO6709").or_else(|| md.get("©xyz")).and_then(parse_iso6709);
}

/// The movie's length (`mvhd`), else its longest track's; `None` for "unknown" (all ones) or
/// absurd values.
fn mp4_seconds(f: &Mp4File) -> Option<f64> {
    let secs = |d: u64, scale: u32| (scale > 0 && d > 0 && d != u64::from(u32::MAX) && d != u64::MAX).then_some(d as f64 / f64::from(scale));
    let movie = secs(f.duration, f.timescale);
    let track = f.tracks.iter().filter_map(|t| secs(t.duration, t.timescale)).fold(None, |a: Option<f64>, s| Some(a.map_or(s, |a| a.max(s))));
    movie.or(track).filter(|s| s.is_finite() && *s > 0.0 && *s < 1e8)
}

/// A position in ISO 6709 form (`+37.4467+025.3289+012.345/`, also degrees and minutes
/// `±DDMM.MM±DDDMM.MM` or degrees, minutes and seconds `±DDMMSS.S±DDDMMSS.S`), as [latitude,
/// longitude]; `None` when malformed, out of range or exactly 0°, 0°.
pub(crate) fn parse_iso6709(s: &str) -> Option<[f64; 2]> {
    let (lat, rest) = iso6709_part(s.trim(), 2)?;
    let (lon, _) = iso6709_part(rest, 3)?;
    let valid = (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) && !(lat == 0.0 && lon == 0.0);
    valid.then_some([lat, lon])
}

/// One signed ISO 6709 coordinate with `deg` degree digits, and the text after it.
fn iso6709_part(s: &str, deg: usize) -> Option<(f64, &str)> {
    let negative = match s.as_bytes().first()? {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let body = s.get(1..)?;
    let end = body.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(body.len());
    let number = body.get(..end)?;
    let int_len = number.find('.').unwrap_or(number.len());
    let (int, frac) = (number.get(..int_len)?, number.get(int_len..)?);
    let num = |t: &str| t.parse::<f64>().ok().filter(|v| v.is_finite());
    let value = match int_len.checked_sub(deg)? {
        0 => num(number)?,
        2 => {
            let minutes = num(&format!("{}{frac}", int.get(deg..)?))?;
            if minutes >= 60.0 {
                return None;
            }
            num(int.get(..deg)?)? + minutes / 60.0
        }
        4 => {
            let minutes = num(int.get(deg..deg + 2)?)?;
            let seconds = num(&format!("{}{frac}", int.get(deg + 2..)?))?;
            if minutes >= 60.0 || seconds >= 60.0 {
                return None;
            }
            num(int.get(..deg)?)? + minutes / 60.0 + seconds / 3600.0
        }
        _ => return None,
    };
    Some((if negative { -value } else { value }, body.get(end..)?))
}

// ---------- sound ----------

/// WAV / BWF / RF64 / BW64: data size over the `fmt ` byte rate.
fn wav_seconds<S: ByteSource + ?Sized>(src: &S) -> Option<f64> {
    let len = src.len();
    let (mut pos, mut byte_rate, mut data, mut data64) = (12u64, None, None, None);
    for _ in 0..1024 {
        let h: [u8; 8] = exif::read(src, pos)?;
        let size = u64::from(u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
        let body = pos.saturating_add(8);
        match &[h[0], h[1], h[2], h[3]] {
            // RF64 / BW64: 64-bit RIFF and data sizes
            b"ds64" => data64 = exif::read::<16, S>(src, body).map(|b| u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]])),
            b"fmt " => byte_rate = exif::read::<12, S>(src, body).map(|b| u32::from_le_bytes([b[8], b[9], b[10], b[11]])),
            b"data" => {
                let n = if size == u64::from(u32::MAX) { data64.unwrap_or(u64::MAX) } else { size };
                data = Some(n.min(len.saturating_sub(body)));
            }
            _ => {}
        }
        if let (Some(rate), Some(bytes)) = (byte_rate, data) {
            return (rate > 0).then_some(bytes as f64 / f64::from(rate));
        }
        // chunks are padded to an even size
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    None
}

/// AIFF / AIFC: sample frames over the 80-bit extended sample rate of `COMM`.
fn aiff_seconds<S: ByteSource + ?Sized>(src: &S) -> Option<f64> {
    let mut pos = 12u64;
    for _ in 0..1024 {
        let h: [u8; 8] = exif::read(src, pos)?;
        let size = u64::from(u32::from_be_bytes([h[4], h[5], h[6], h[7]]));
        if [h[0], h[1], h[2], h[3]] == *b"COMM" {
            // channels (2), sample frames (4), sample size (2), sample rate (10)
            let b: [u8; 18] = exif::read(src, pos.saturating_add(8))?;
            let frames = u32::from_be_bytes([b[2], b[3], b[4], b[5]]);
            let rate = extended80([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
            return (rate.is_finite() && rate > 0.0 && frames > 0).then_some(f64::from(frames) / rate);
        }
        pos = pos.saturating_add(8).saturating_add(size).saturating_add(size & 1);
    }
    None
}

/// An IEEE 754 80-bit extended number (sign, 15-bit exponent, 64-bit mantissa with an explicit
/// integer bit), as AIFF stores its sample rate; negative values and zero read as 0.
fn extended80(b: [u8; 10]) -> f64 {
    let exp = i32::from(u16::from_be_bytes([b[0], b[1]]) & 0x7FFF);
    let mantissa = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if b[0] & 0x80 != 0 || exp == 0 || mantissa == 0 {
        return 0.0;
    }
    mantissa as f64 * 2f64.powi(exp - 16383 - 63)
}

/// FLAC: total samples over the sample rate of `STREAMINFO` (after an ID3v2 tag, if any).
fn flac_seconds<S: ByteSource + ?Sized>(src: &S) -> Option<f64> {
    let mut pos = 0u64;
    if let Some(h) = exif::read::<10, S>(src, 0)
        && h.starts_with(b"ID3")
    {
        // ID3v2: a "synchsafe" size (7 bits a byte), plus a 10-byte footer when flagged
        let size = [h[6], h[7], h[8], h[9]].iter().fold(0u64, |a, &c| (a << 7) | u64::from(c & 0x7F));
        pos = 10 + size + if h[5] & 0x10 != 0 { 10 } else { 0 };
    }
    // "fLaC", the metadata block header, STREAMINFO bytes 0–17
    let b: [u8; 26] = exif::read(src, pos)?;
    if [b[0], b[1], b[2], b[3]] != *b"fLaC" || b[4] & 0x7F != 0 {
        return None;
    }
    // sample rate (20 bits), channels − 1 (3), bits − 1 (5), total samples (36)
    let v = u64::from_be_bytes([b[18], b[19], b[20], b[21], b[22], b[23], b[24], b[25]]);
    let (rate, total) = ((v >> 44) & 0xF_FFFF, v & 0xF_FFFF_FFFF);
    (rate > 0 && total > 0).then_some(total as f64 / rate as f64)
}

// ---------- dates ----------

/// A calendar date and time as a file states it, with its UTC offset when the file gives one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    /// Minutes east of UTC; `None`: local time of an unknown zone.
    offset: Option<i16>,
}

fn is_leap(y: u16) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` as checked text (a real calendar date, year 0001–9999); `None` otherwise.
pub(crate) fn parse_date(s: &str) -> Option<String> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    Stamp::parse(&format!("{s}T00:00:00")).map(|t| t.date())
}

/// A UTC time from Unix seconds as RFC 3339 (`2026-09-05T15:22:00Z`).
fn rfc3339_utc(unix: u64) -> Option<String> {
    Stamp::unix(unix).map(|t| t.rfc3339())
}

impl Stamp {
    /// A date and time as files write them: RFC 3339 / ISO 8601 (`2026-09-05T18:22:00+03:00`,
    /// QuickTime's `…+0300`, `…Z`, fractions of a second, a space for the `T`) or EXIF
    /// (`2026:09:05 18:22:00`). `None` for anything that is no real date and time, such as
    /// EXIF's blank `    :  :     :  :  ` or `0000:00:00 00:00:00`. Text after the time that is
    /// no UTC offset is ignored (the time stays local).
    pub(crate) fn parse(s: &str) -> Option<Stamp> {
        let b = s.trim_matches(|c: char| c.is_whitespace() || c == '\0').as_bytes();
        let num = |from: usize, to: usize| -> Option<u16> {
            let d = b.get(from..to)?;
            d.iter().all(u8::is_ascii_digit).then(|| d.iter().fold(0u16, |a, &c| a * 10 + u16::from(c - b'0')))
        };
        let is = |at: usize, set: &[u8]| b.get(at).is_some_and(|c| set.contains(c));
        if !(is(4, b"-:") && is(7, b"-:") && is(10, b"Tt ") && is(13, b":") && is(16, b":")) {
            return None;
        }
        let small = |from, to| num(from, to).and_then(|v| u8::try_from(v).ok());
        let t = Stamp {
            year: num(0, 4)?,
            month: small(5, 7)?,
            day: small(8, 10)?,
            hour: small(11, 13)?,
            minute: small(14, 16)?,
            second: small(17, 19)?,
            offset: None,
        };
        let valid = t.second <= 60
            && t.year >= 1
            && (1..=12).contains(&t.month)
            && t.day >= 1
            && t.day <= days_in_month(t.year, t.month)
            && t.hour < 24
            && t.minute < 60;
        if !valid {
            return None;
        }
        let mut rest = b.get(19..).unwrap_or_default();
        if let [b'.' | b',', tail @ ..] = rest {
            rest = tail.get(tail.iter().take_while(|c| c.is_ascii_digit()).count()..).unwrap_or_default();
        }
        // a leap second (60) is kept as :59
        Some(Stamp { second: t.second.min(59), offset: parse_offset_bytes(rest), ..t })
    }

    /// EXIF DateTimeOriginal / DateTimeDigitized with its OffsetTime* tag.
    fn exif(date: &str, offset: Option<&str>) -> Option<Stamp> {
        let t = Stamp::parse(date)?;
        Some(match t.offset {
            Some(_) => t,
            None => Stamp { offset: offset.and_then(|o| parse_offset_bytes(o.trim().as_bytes())), ..t },
        })
    }

    /// A UTC time from Unix seconds (1970 to 9999).
    fn unix(secs: u64) -> Option<Stamp> {
        if secs > MAX_UNIX {
            return None;
        }
        let (y, mo, d, h, mi, s) = filmcraft_format::autosave::civil(i64::try_from(secs).ok()?, 0);
        let small = |v: u32| u8::try_from(v).ok();
        Some(Stamp { year: u16::try_from(y).ok()?, month: small(mo)?, day: small(d)?, hour: small(h)?, minute: small(mi)?, second: small(s)?, offset: Some(0) })
    }

    /// An MP4 `mvhd` time (seconds since 1904, UTC); `None` for unset (0) or pre-1970 times,
    /// which no camera wrote (some count from 1970 by mistake: they land in 1904–1970).
    fn mp4(secs: u64) -> Option<Stamp> {
        secs.checked_sub(MP4_EPOCH_TO_UNIX).filter(|&s| s > 0).and_then(Stamp::unix)
    }

    /// RFC 3339: `2026-09-05T18:22:00+03:00`, `…Z` for UTC, no offset for local time.
    pub(crate) fn rfc3339(&self) -> String {
        let offset = match self.offset {
            None => String::new(),
            Some(0) => "Z".into(),
            Some(m) => format!("{}{:02}:{:02}", if m < 0 { '-' } else { '+' }, m.unsigned_abs() / 60, m.unsigned_abs() % 60),
        };
        format!("{}T{:02}:{:02}:{:02}{offset}", self.date(), self.hour, self.minute, self.second)
    }

    /// The date as written (the local date of the capture), `YYYY-MM-DD`.
    pub(crate) fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Seconds since 1970 (UTC; a local time without offset counts as UTC).
    pub(crate) fn instant(&self) -> i64 {
        let days = days_from_civil(i64::from(self.year), i64::from(self.month), i64::from(self.day));
        days * 86_400 + i64::from(self.hour) * 3600 + i64::from(self.minute) * 60 + i64::from(self.second) - i64::from(self.offset.unwrap_or(0)) * 60
    }
}

/// A UTC offset: `Z`, `±HH:MM`, `±HHMM` or `±HH` (hours below 24, minutes below 60).
fn parse_offset_bytes(b: &[u8]) -> Option<i16> {
    let (sign, digits) = match b {
        [b'Z' | b'z', ..] => return Some(0),
        [b'+', rest @ ..] => (1, rest),
        [b'-', rest @ ..] => (-1, rest),
        _ => return None,
    };
    let two = |at: usize| -> Option<i16> {
        let d = digits.get(at..at + 2)?;
        d.iter().all(u8::is_ascii_digit).then(|| d.iter().fold(0i16, |a, &c| a * 10 + i16::from(c - b'0')))
    };
    let hours = two(0)?;
    let minutes = match digits.get(2) {
        Some(b':') => two(3)?,
        Some(c) if c.is_ascii_digit() => two(2)?,
        _ => 0,
    };
    (hours < 24 && minutes < 60).then_some(sign * (hours * 60 + minutes))
}

// ---------- finding media ----------

/// What `regie.findMedia` looks for. [`Default`] finds every photo, video and sound file (up to
/// [`DEFAULT_LIMIT`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MediaFilter {
    /// Capture dates to keep, inclusive, as `YYYY-MM-DD`, compared with the capture's local date
    /// (the file's UTC modification date when the capture time is unknown; a file without
    /// either is left out once a date is set). `None`: open-ended. A date that is not
    /// `YYYY-MM-DD` is ignored here (`regie.findMedia` refuses it).
    pub from: Option<String>,
    pub to: Option<String>,
    /// Words of which one must be in the place name, in the name of a folder below the searched
    /// folder or in the file name (case-insensitive); empty: all.
    pub places: Vec<String>,
    /// "video", "photo", "audio"; empty: all.
    pub kinds: Vec<String>,
    /// At most this many candidates, the earliest first (capped at [`MAX_FILES`]).
    pub limit: usize,
    /// Keep screenshots and screen recordings (files named as [`SCREENSHOT_NAMES`]); they are
    /// left out by default.
    pub screenshots: bool,
}

impl Default for MediaFilter {
    fn default() -> Self {
        MediaFilter { from: None, to: None, places: Vec::new(), kinds: Vec::new(), limit: DEFAULT_LIMIT, screenshots: false }
    }
}

impl MediaFilter {
    /// The filter a plan asks for: its dates, places and photos / videos (both when it asks for
    /// neither).
    pub fn from_plan(plan: &Plan) -> Self {
        let kinds = match (plan.photos, plan.videos) {
            (true, false) => vec!["photo".to_string()],
            (false, true) => vec!["video".to_string()],
            _ => vec!["photo".to_string(), "video".to_string()],
        };
        MediaFilter { from: plan.from.clone(), to: plan.to.clone(), places: plan.places.clone(), kinds, ..Default::default() }
    }
}

/// What [`find_media`] found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FindResult {
    /// Matching media by capture time (unknown last, then by path), at most the filter's limit.
    pub candidates: Vec<Candidate>,
    /// Files looked at: every visible file in the searched folders, media or not (the search
    /// stops at [`MAX_FILES`]).
    pub scanned: u64,
    /// What was left out unseen: photos and videos in formats FilmCraft does not import (HEIC,
    /// camera raw other than DNG, WMV…), files and folders that could not be read, links to
    /// folders, folders nested deeper than [`MAX_DEPTH`].
    pub skipped: u64,
    /// How many media matched the filter before the limit cut the list.
    pub matched: u64,
    /// Screenshots and screen recordings left out (see [`MediaFilter::screenshots`]).
    pub screenshots: u64,
}

/// Search `folders` (the user's picture, video and music folders when empty, see
/// [`default_folders`]) for photos, videos and sound files that pass `filter`. Looks
/// [`MAX_DEPTH`] folders deep and at [`MAX_FILES`] files at most; skips hidden entries
/// (a leading dot), NAS thumbnail / recycle and system folders, and links to folders (links to
/// files are followed). Setting `cancel` stops the search; what was found so far is returned.
pub fn find_media(folders: &[PathBuf], filter: &MediaFilter, cancel: &AtomicBool) -> FindResult {
    let roots = if folders.is_empty() { default_folders() } else { folders.to_vec() };
    let mut walk = Walk::new(filter);
    for root in distinct_roots(roots) {
        if !walk.search(&root, cancel) {
            break;
        }
    }
    walk.finish(filter.limit)
}

/// One search's prepared filter and findings.
struct Walk {
    from: Option<String>,
    to: Option<String>,
    words: Vec<String>,
    kinds: Vec<String>,
    found: Vec<(Option<i64>, Candidate)>,
    seen: std::collections::HashSet<String>,
    scanned: u64,
    skipped: u64,
    keep_screenshots: bool,
    screenshots: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Link,
}

impl Walk {
    fn new(filter: &MediaFilter) -> Self {
        Walk {
            from: filter.from.as_deref().and_then(parse_date),
            to: filter.to.as_deref().and_then(parse_date),
            words: filter.places.iter().map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty()).take(MAX_WORDS).collect(),
            kinds: filter.kinds.iter().map(|k| k.trim().to_ascii_lowercase()).filter(|k| !k.is_empty()).take(MAX_WORDS).collect(),
            found: Vec::new(),
            seen: Default::default(),
            scanned: 0,
            skipped: 0,
            keep_screenshots: filter.screenshots,
            screenshots: 0,
        }
    }

    /// Walk one searched folder; false when the search must stop (cancelled, or [`MAX_FILES`]).
    fn search(&mut self, root: &Path, cancel: &AtomicBool) -> bool {
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let Some(entries) = self.list(&dir) else {
                self.skipped += 1;
                continue;
            };
            let mut subdirs = Vec::new();
            for (name, path, kind) in entries {
                if cancel.load(Ordering::Relaxed) {
                    return false;
                }
                if name.starts_with('.') || SYSTEM_FOLDERS.contains(&name.as_str()) {
                    continue;
                }
                let kind = match kind {
                    EntryKind::Link => match std::fs::metadata(&path) {
                        Ok(m) if m.is_dir() => {
                            self.skipped += 1;
                            continue;
                        }
                        Ok(m) if m.is_file() => EntryKind::File,
                        _ => continue,
                    },
                    k => k,
                };
                if kind == EntryKind::Dir {
                    if depth < MAX_DEPTH {
                        subdirs.push(path);
                    } else {
                        self.skipped += 1;
                    }
                    continue;
                }
                if self.scanned >= MAX_FILES {
                    return false;
                }
                self.scanned += 1;
                self.consider(root, &path);
            }
            // sorted names: visit the subfolders in order (the stack pops the last first)
            stack.extend(subdirs.into_iter().rev().map(|d| (d, depth + 1)));
        }
        true
    }

    /// The entries of `dir` sorted by name: (name, path, kind). Entries that are neither files,
    /// folders nor links are left out.
    fn list(&mut self, dir: &Path) -> Option<Vec<(String, PathBuf, EntryKind)>> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(dir).ok()? {
            let Ok(e) = e else {
                self.skipped += 1;
                continue;
            };
            if v.len() >= MAX_DIR_ENTRIES {
                self.skipped += 1;
                break;
            }
            let Ok(t) = e.file_type() else {
                self.skipped += 1;
                continue;
            };
            let kind = if t.is_symlink() {
                EntryKind::Link
            } else if t.is_dir() {
                EntryKind::Dir
            } else if t.is_file() {
                EntryKind::File
            } else {
                continue;
            };
            v.push((e.file_name().to_string_lossy().into_owned(), e.path(), kind));
        }
        v.sort_by(|a, b| a.0.cmp(&b.0));
        Some(v)
    }

    /// Check one file against the filter and keep it when it passes.
    fn consider(&mut self, root: &Path, path: &Path) {
        let ext = extension(path);
        let Some(ext_kind) = kind_of_extension(&ext) else {
            if UNSUPPORTED.contains(&ext.as_str()) {
                self.skipped += 1;
            }
            return;
        };
        // the name goes into JSON and back to the importer: it must be text
        let Some(name) = path.to_str() else {
            self.skipped += 1;
            return;
        };
        if !self.seen.insert(name.to_string()) {
            return;
        }
        // what the path alone decides comes first: no file is opened for it
        if !self.kinds.is_empty() && !is_mp4_extension(&ext) && !self.kinds.iter().any(|k| k == ext_kind) {
            return;
        }
        if !self.keep_screenshots && ext_kind != "audio" && is_screenshot(path) {
            self.screenshots += 1;
            return;
        }
        if !self.words.is_empty() && !place_matches(&self.words, root, path, None) {
            return;
        }
        let (info, readable) = probe(path);
        if !readable || info.kind.is_empty() {
            // unreadable, or no longer a file (replaced while the search ran)
            self.skipped += 1;
            return;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&info.kind) {
            return;
        }
        let stamp = info.created.as_deref().and_then(Stamp::parse);
        if self.from.is_some() || self.to.is_some() {
            let Some(day) = stamp.or_else(|| info.modified.as_deref().and_then(Stamp::parse)).map(|t| t.date()) else { return };
            if self.from.as_ref().is_some_and(|f| day < *f) || self.to.as_ref().is_some_and(|t| day > *t) {
                return;
            }
        }
        let folder = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let candidate = Candidate {
            path: name.to_string(),
            kind: info.kind,
            created: info.created,
            modified: info.modified,
            location: info.location,
            place: None,
            width: info.width,
            height: info.height,
            seconds: info.seconds,
            folder,
            item: None,
        };
        self.found.push((stamp.map(|t| t.instant()), candidate));
    }

    fn finish(mut self, limit: usize) -> FindResult {
        // by capture time, unknown last, then by path
        self.found.sort_by(|(a, x), (b, y)| match (a, b) {
            (Some(a), Some(b)) => a.cmp(b).then_with(|| x.path.cmp(&y.path)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => x.path.cmp(&y.path),
        });
        let matched = self.found.len() as u64;
        let limit = limit.min(usize::try_from(MAX_FILES).unwrap_or(usize::MAX));
        let candidates = self.found.into_iter().take(limit).map(|(_, c)| c).collect();
        FindResult { candidates, scanned: self.scanned, skipped: self.skipped, matched, screenshots: self.screenshots }
    }
}

/// Whether one of `words` (lower case) is in `place`, in a folder name between `root` and `path`
/// or in the file name.
fn place_matches(words: &[String], root: &Path, path: &Path, place: Option<&str>) -> bool {
    let hit = |text: &str| {
        let text = text.to_lowercase();
        words.iter().any(|w| text.contains(w.as_str()))
    };
    if place.is_some_and(hit) {
        return true;
    }
    let below = path.strip_prefix(root).unwrap_or(path);
    below.components().any(|c| match c {
        Component::Normal(name) => hit(&name.to_string_lossy()),
        _ => false,
    })
}

/// `path` made absolute against the current folder (unchanged when there is none, on the web).
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
}

/// The folders to search: absolute, each once, without folders inside another one of them (at
/// most [`MAX_FOLDERS`]).
fn distinct_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let roots: Vec<(PathBuf, PathBuf)> = roots
        .into_iter()
        .take(MAX_FOLDERS)
        .map(|r| {
            let a = absolute(&r);
            let c = std::fs::canonicalize(&a).unwrap_or_else(|_| a.clone());
            (a, c)
        })
        .collect();
    roots
        .iter()
        .enumerate()
        .filter(|(i, (_, c))| !roots.iter().enumerate().any(|(j, (_, d))| j != *i && c.starts_with(d) && (c != d || j < *i)))
        .map(|(_, (a, _))| a.clone())
        .collect()
}

/// The user's picture, video and music folders that exist: `XDG_PICTURES_DIR`,
/// `XDG_VIDEOS_DIR` and `XDG_MUSIC_DIR` from `user-dirs.dirs` (`$XDG_CONFIG_HOME`, else
/// `~/.config`; German systems name them "Bilder", "Videos", "Musik"), else `~/Pictures`,
/// `~/Videos` (`~/Movies` on macOS) and `~/Music`.
pub fn default_folders() -> Vec<PathBuf> {
    let Some(home) = crate::media_browser::std_home_dir().map(PathBuf::from) else { return Vec::new() };
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".config"));
    user_media_folders(&home, &config)
}

/// [`default_folders`] for a given home and configuration folder.
fn user_media_folders(home: &Path, config: &Path) -> Vec<PathBuf> {
    let xdg = read_user_dirs(&config.join("user-dirs.dirs"), home);
    let mut out: Vec<PathBuf> = Vec::new();
    for (key, fallbacks) in [("XDG_PICTURES_DIR", &["Pictures"][..]), ("XDG_VIDEOS_DIR", &["Videos", "Movies"][..]), ("XDG_MUSIC_DIR", &["Music"][..])] {
        let found = xdg.iter().find(|(k, _)| k == key).map(|(_, p)| p.clone()).filter(|p| p.is_dir());
        let dirs: Vec<PathBuf> = match found {
            Some(p) => vec![p],
            None => fallbacks.iter().map(|f| home.join(f)).filter(|p| p.is_dir()).collect(),
        };
        for d in dirs {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// The `XDG_*_DIR` entries of a `user-dirs.dirs` file: `"$HOME/…"` and absolute paths. A
/// folder set to `$HOME` itself is switched off and left out.
fn read_user_dirs(file: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let Ok(f) = std::fs::File::open(file) else { return Vec::new() };
    if f.take(64 * 1024).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches('"');
            let relative = value.strip_prefix("$HOME").or_else(|| value.strip_prefix("${HOME}"));
            let path = match relative {
                Some(rest) => {
                    let rest = rest.trim_start_matches('/');
                    if rest.is_empty() {
                        return None;
                    }
                    home.join(rest)
                }
                None if value.starts_with('/') => PathBuf::from(value),
                None => return None,
            };
            Some((key.trim().to_string(), path))
        })
        .collect()
}

/// `~` or `~/…` from the home folder; anything else as given.
fn expand_home(s: &str) -> PathBuf {
    let home = || crate::media_browser::std_home_dir().map(PathBuf::from);
    if s == "~" {
        return home().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    PathBuf::from(s)
}

// ---------- commands ----------

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "media.captureInfo",
            label: "Capture Date and Place",
            menu: &[],
            shortcut: None,
            params: r#"{"path":str} | {"item":id}"#,
            enabled: always,
            run: run_capture_info,
            journal: false,
        },
        CommandSpec {
            id: "regie.findMedia",
            label: "AI Director: Find Media",
            menu: &[],
            shortcut: None,
            params: r#"{"folders":[str]?,"from":"YYYY-MM-DD"?,"to":"YYYY-MM-DD"?,"places":[str]?,"kinds":["video"|"photo"|"audio"]?,"limit":int=10000,"screenshots":bool=false}"#,
            enabled: always,
            run: run_find_media,
            journal: false,
        },
    ]
}

/// A parameter value for an error message, shortened.
fn shown(v: &Value) -> String {
    short(&v.to_string())
}

/// Text for an error message, shortened to 60 characters.
fn short(s: &str) -> String {
    if s.chars().count() > 60 { format!("{}…", s.chars().take(60).collect::<String>()) } else { s.to_string() }
}

/// `media.captureInfo`: the [`CaptureInfo`] of a file (`path`) or of a project item's file
/// (`item`), with `path` (and `item`) added.
fn run_capture_info(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "media.captureInfo";
    if !p.is_object() {
        return Err(bad(CMD, "parameters must be an object: {\"path\": file} or {\"item\": id}"));
    }
    let (path, item) = match (p.get("path").filter(|v| !v.is_null()), p.get("item").filter(|v| !v.is_null())) {
        (Some(v), _) => match v.as_str().map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => (t.to_string(), None),
            None => return Err(bad(CMD, format!("`path` must be the path of a file, got {}", shown(v)))),
        },
        (None, Some(v)) => {
            let id = v.as_u64().ok_or_else(|| bad(CMD, format!("`item` must be a project item id, got {}", shown(v))))?;
            let it = s.project.item(ItemId(id)).ok_or_else(|| bad(CMD, format!("there is no project item {id}")))?;
            match it.as_media().map(|m| &m.media) {
                Some(MediaRef::File { path }) => (path.clone(), Some(id)),
                _ => return Err(bad(CMD, format!("item {id} (\"{}\") is not a media file", short(&it.name)))),
            }
        }
        (None, None) => return Err(bad(CMD, "name a file (`path`) or a project item (`item`)")),
    };
    let file = expand_home(&path);
    if !file.is_file() {
        let name = short(&path);
        return Err(bad(CMD, if file.is_dir() { format!("{name} is a folder, not a file") } else { format!("there is no file {name}") }));
    }
    let mut v = json!(capture_info(&file));
    if let Value::Object(m) = &mut v {
        m.insert("path".into(), json!(file.to_string_lossy()));
        if let Some(id) = item {
            m.insert("item".into(), json!(id));
        }
    }
    Ok(v)
}

/// Words under `key` (`places`, `kinds`): a list of texts or one text, each split at commas
/// ("Mykonos, Kreta").
fn words_p(p: &Value, key: &str, cmd: &str) -> Result<Vec<String>> {
    let words: Vec<String> = texts_p(p, key, cmd, MAX_WORDS)?
        .iter()
        .flat_map(|t| t.split(','))
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .take(MAX_WORDS + 1)
        .map(str::to_string)
        .collect();
    if words.len() > MAX_WORDS {
        return Err(bad(cmd, format!("`{key}` names more than {MAX_WORDS} words")));
    }
    Ok(words)
}

/// A list of texts under `key`: absent or null is empty, one text a list of one.
fn texts_p(p: &Value, key: &str, cmd: &str, max: usize) -> Result<Vec<String>> {
    let list = match p.get(key) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(s)) => return Ok(vec![s.clone()]),
        Some(Value::Array(a)) => a,
        Some(v) => return Err(bad(cmd, format!("`{key}` must be a list of texts, got {}", shown(v)))),
    };
    if list.len() > max {
        return Err(bad(cmd, format!("`{key}` names {} entries; at most {max} are allowed", list.len())));
    }
    list.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad(cmd, format!("`{key}` must be a list of texts, got {}", shown(v))))).collect()
}

/// A `YYYY-MM-DD` date under `key` (absent, null or "" is none).
fn date_p(p: &Value, key: &str, cmd: &str) -> Result<Option<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => parse_date(s).map(Some).ok_or_else(|| bad(cmd, format!("`{key}` must be a date as YYYY-MM-DD, got {:?}", short(s)))),
        Some(v) => Err(bad(cmd, format!("`{key}` must be a date as YYYY-MM-DD, got {}", shown(v)))),
    }
}

/// `regie.findMedia`: search folders for media by date, place and kind.
fn run_find_media(_s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "regie.findMedia";
    if !p.is_object() && !p.is_null() {
        return Err(bad(CMD, "parameters must be an object"));
    }
    let from = date_p(p, "from", CMD)?;
    let to = date_p(p, "to", CMD)?;
    if let (Some(f), Some(t)) = (&from, &to)
        && f > t
    {
        return Err(bad(CMD, format!("`from` ({f}) is after `to` ({t})")));
    }
    let places = words_p(p, "places", CMD)?;
    let kinds = words_p(p, "kinds", CMD)?
        .into_iter()
        .map(|k| match k.to_ascii_lowercase().as_str() {
            "video" | "videos" => Ok("video".to_string()),
            "photo" | "photos" | "image" | "images" | "picture" | "pictures" => Ok("photo".to_string()),
            "audio" | "music" | "sound" => Ok("audio".to_string()),
            _ => Err(bad(CMD, format!("unknown kind {:?}; use \"video\", \"photo\" or \"audio\"", short(&k)))),
        })
        .collect::<Result<Vec<_>>>()?;
    let limit = match p.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(v) => {
            let n = v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f.min(MAX_FILES as f64) as u64));
            let n = n.ok_or_else(|| bad(CMD, format!("`limit` must be a number of candidates (0 or more), got {}", shown(v))))?;
            usize::try_from(n.min(MAX_FILES)).unwrap_or(DEFAULT_LIMIT)
        }
    };
    let mut folders = Vec::new();
    let mut missing = Vec::new();
    for f in texts_p(p, "folders", CMD, MAX_FOLDERS)? {
        let f = f.trim();
        if f.is_empty() {
            continue;
        }
        let path = absolute(&expand_home(f));
        if path.is_dir() {
            folders.push(path);
        } else {
            missing.push(format!("{:?}", short(f)));
        }
    }
    if !missing.is_empty() {
        let more = if missing.len() > 3 { format!(" and {} more", missing.len() - 3) } else { String::new() };
        return Err(bad(CMD, format!("no folder at {}{more}", missing.iter().take(3).cloned().collect::<Vec<_>>().join(", "))));
    }
    if folders.is_empty() {
        folders = default_folders();
        if folders.is_empty() {
            return Err(bad(CMD, "found no picture, video or music folder: name the folders to search in `folders`"));
        }
    }
    let screenshots = match p.get("screenshots") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, format!("`screenshots` must be true or false, got {}", shown(v))))?,
    };
    let filter = MediaFilter { from, to, places, kinds, limit, screenshots };
    let r = find_media(&folders, &filter, &AtomicBool::new(false));
    let folders: Vec<String> = distinct_roots(folders).iter().map(|f| f.to_string_lossy().into_owned()).collect();
    Ok(json!({
        "candidates": r.candidates,
        "scanned": r.scanned,
        "skipped": r.skipped,
        "matched": r.matched,
        "screenshots": r.screenshots,
        "folders": folders
    }))
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;
