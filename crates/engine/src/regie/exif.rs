//! Photo headers for KI-Regie: EXIF capture date, offset, place and orientation (CIPA DC-008
//! Exif 2.32 / 3.0, stored as TIFF 6.0 image file directories) from JPEG `APP1` segments, PNG
//! `eXIf` chunks and TIFF / DNG files, and the picture size of the still formats FilmCraft
//! imports: JPEG frame headers (ITU-T T.81 B.2.2), PNG `IHDR` (PNG 3rd edition), GIF89a's logical
//! screen, WebP's `VP8 ` / `VP8L` / `VP8X` chunks (RFC 9649), the BMP DIB header, the Truevision
//! TGA 2.0 header and the DPX image information header (SMPTE ST 268). Written from those
//! specifications only.
//!
//! Every byte is hostile. Reads go through [`ByteSource::read_at`] at checked offsets, nothing is
//! sized by the file beyond small fixed caps ([`MAX_EXIF`], [`MAX_ENTRIES`]), and directories are
//! followed one link deep from IFD0 at most (never along "next IFD" chains), so cyclic or
//! out-of-range offsets end in "unknown", never in a panic or a loop.

use filmcraft_isobmff::ByteSource;

/// Most bytes of EXIF data read from a PNG `eXIf` chunk (a JPEG segment holds at most 64 KiB).
pub(crate) const MAX_EXIF: usize = 1 << 20;
/// Most entries read from one image file directory (cameras write well under a hundred).
pub(crate) const MAX_ENTRIES: usize = 512;
/// Most JPEG marker segments or PNG chunks walked before giving up.
const MAX_SEGMENTS: usize = 1024;

// TIFF 6.0 tags (IFD0 and SubIFDs).
const NEW_SUBFILE_TYPE: u16 = 0x00FE;
const IMAGE_WIDTH: u16 = 0x0100;
const IMAGE_LENGTH: u16 = 0x0101;
const ORIENTATION: u16 = 0x0112;
const SUB_IFDS: u16 = 0x014A;
const DEFAULT_CROP_SIZE: u16 = 0xC620;
// Exif pointers (in IFD0) and Exif IFD tags (CIPA DC-008 §4.6).
const EXIF_IFD: u16 = 0x8769;
const GPS_IFD: u16 = 0x8825;
const DATE_TIME_ORIGINAL: u16 = 0x9003;
const DATE_TIME_DIGITIZED: u16 = 0x9004;
const OFFSET_TIME_ORIGINAL: u16 = 0x9011;
const OFFSET_TIME_DIGITIZED: u16 = 0x9012;
// GPS IFD tags.
const GPS_LATITUDE_REF: u16 = 1;
const GPS_LATITUDE: u16 = 2;
const GPS_LONGITUDE_REF: u16 = 3;
const GPS_LONGITUDE: u16 = 4;

/// What a photo's EXIF data says (raw where the caller interprets it).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Exif {
    /// Orientation (tag 274): 1–8; `None` when absent or out of range.
    pub orientation: Option<u16>,
    /// DateTimeOriginal ("2026:09:05 18:22:00") with OffsetTimeOriginal ("+03:00") when present.
    pub original: Option<(String, Option<String>)>,
    /// DateTimeDigitized with OffsetTimeDigitized.
    pub digitized: Option<(String, Option<String>)>,
    /// [latitude, longitude] in degrees, south and west negative.
    pub location: Option<[f64; 2]>,
    /// The main image's size in a TIFF / DNG file (IFD0, or the DNG raw SubIFD); 0 when unknown.
    pub width: u32,
    pub height: u32,
}

/// What a still image's headers say: its stored size (orientation not applied) and EXIF data.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Still {
    pub width: u32,
    pub height: u32,
    pub exif: Exif,
}

/// Still formats whose headers this module reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StillFormat {
    Jpeg,
    Png,
    Gif,
    Webp,
    Tiff,
    Bmp,
    Tga,
    Dpx,
}

/// The headers of a still image of format `f`.
pub(crate) fn still<S: ByteSource + ?Sized>(src: &S, f: StillFormat) -> Still {
    match f {
        StillFormat::Jpeg => jpeg(src),
        StillFormat::Png => png(src),
        StillFormat::Gif => gif(src),
        StillFormat::Webp => webp(src),
        StillFormat::Tiff => {
            let exif = read_exif(src, 0);
            Still { width: exif.width, height: exif.height, exif }
        }
        StillFormat::Bmp => bmp(src),
        StillFormat::Tga => tga(src),
        StillFormat::Dpx => dpx(src),
    }
}

// ---------- byte access ----------

/// `N` bytes at `at`, or `None` when the source does not hold them.
pub(crate) fn read<const N: usize, S: ByteSource + ?Sized>(src: &S, at: u64) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    src.read_at(at, &mut b).ok()?;
    Some(b)
}

/// Up to `len` bytes at `at`, never more than `max` and never past the end of the source (fewer
/// there); `None` when `at` is past the end or the read fails.
pub(crate) fn read_upto<S: ByteSource + ?Sized>(src: &S, at: u64, len: u64, max: usize) -> Option<Vec<u8>> {
    let avail = src.len().checked_sub(at)?;
    let n = usize::try_from(len.min(avail)).unwrap_or(usize::MAX).min(max);
    let mut v = vec![0u8; n];
    src.read_at(at, &mut v).ok()?;
    Some(v)
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?))
}
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}
fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(3)?)?;
    Some(s.iter().rev().fold(0u32, |a, &c| (a << 8) | u32::from(c)))
}

// ---------- TIFF / EXIF ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    Little,
    Big,
}

impl Order {
    fn u16(self, b: [u8; 2]) -> u16 {
        match self {
            Order::Little => u16::from_le_bytes(b),
            Order::Big => u16::from_be_bytes(b),
        }
    }
    fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            Order::Little => u32::from_le_bytes(b),
            Order::Big => u32::from_be_bytes(b),
        }
    }
}

/// One image file directory entry (TIFF 6.0 §2): tag, field type, value count and the four
/// bytes holding the value itself (when it fits) or its offset.
#[derive(Clone, Copy, Debug)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    field: [u8; 4],
}

/// Bytes per value of a TIFF field type (TIFF 6.0 §2; 13 = IFD, TIFF Technical Note 1).
fn type_size(typ: u16) -> Option<u64> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

fn find(entries: &[Entry], tag: u16) -> Option<&Entry> {
    entries.iter().find(|e| e.tag == tag)
}

/// A TIFF structure whose header is at `base` in `src` (offsets in it count from `base`).
struct Tiff<'a, S: ?Sized> {
    src: &'a S,
    base: u64,
    order: Order,
}

impl<'a, S: ByteSource + ?Sized> Tiff<'a, S> {
    /// The header: byte order ("II" / "MM"), 42 and the offset of IFD0.
    fn open(src: &'a S, base: u64) -> Option<(Self, u32)> {
        let h: [u8; 8] = read(src, base)?;
        let order = match [h[0], h[1]] {
            [b'I', b'I'] => Order::Little,
            [b'M', b'M'] => Order::Big,
            _ => return None,
        };
        if order.u16([h[2], h[3]]) != 42 {
            return None;
        }
        Some((Tiff { src, base, order }, order.u32([h[4], h[5], h[6], h[7]])))
    }

    /// The entries of the directory at `offset` (at most [`MAX_ENTRIES`]; those past the end of
    /// the data are dropped).
    fn ifd(&self, offset: u32) -> Vec<Entry> {
        let Some(at) = self.base.checked_add(u64::from(offset)) else { return Vec::new() };
        let Some(n) = read::<2, S>(self.src, at).map(|b| self.order.u16(b)) else { return Vec::new() };
        let n = usize::from(n).min(MAX_ENTRIES);
        let Some(bytes) = read_upto(self.src, at.saturating_add(2), n as u64 * 12, n * 12) else { return Vec::new() };
        bytes
            .as_chunks::<12>()
            .0
            .iter()
            .map(|e| Entry {
                tag: self.order.u16([e[0], e[1]]),
                typ: self.order.u16([e[2], e[3]]),
                count: self.order.u32([e[4], e[5], e[6], e[7]]),
                field: [e[8], e[9], e[10], e[11]],
            })
            .collect()
    }

    /// The bytes of `e`'s value, at most `max` of them.
    fn value(&self, e: &Entry, max: usize) -> Option<Vec<u8>> {
        let size = type_size(e.typ)?.checked_mul(u64::from(e.count))?;
        if size <= 4 {
            let n = usize::try_from(size).ok()?.min(max);
            return Some(e.field.get(..n)?.to_vec());
        }
        let at = self.base.checked_add(u64::from(self.order.u32(e.field)))?;
        read_upto(self.src, at, size, max)
    }

    /// The first value of a SHORT, LONG or IFD entry.
    fn uint(&self, e: &Entry) -> Option<u32> {
        if e.count == 0 {
            return None;
        }
        match e.typ {
            3 => Some(u32::from(self.order.u16([e.field[0], e.field[1]]))),
            4 | 13 => Some(self.order.u32(e.field)),
            _ => None,
        }
    }

    /// Up to `max` values of a SHORT, LONG or IFD entry.
    fn uints(&self, e: &Entry, max: usize) -> Vec<u32> {
        let n = usize::try_from(e.count).unwrap_or(usize::MAX).min(max);
        match e.typ {
            3 => self.value(e, n * 2).map(|b| b.as_chunks::<2>().0.iter().map(|c| u32::from(self.order.u16(*c))).collect()).unwrap_or_default(),
            4 | 13 => self.value(e, n * 4).map(|b| b.as_chunks::<4>().0.iter().map(|c| self.order.u32(*c)).collect()).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// An ASCII value up to its first NUL, trimmed; `None` when empty. (Some cameras store
    /// OffsetTime* as UNDEFINED, so that type is read as text too.)
    fn ascii(&self, e: &Entry) -> Option<String> {
        if e.typ != 2 && e.typ != 7 {
            return None;
        }
        let b = self.value(e, 64)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        let s = String::from_utf8_lossy(b.get(..end)?).trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// Up to `max` RATIONAL / SRATIONAL values; `None` when a denominator is zero.
    fn rationals(&self, e: &Entry, max: usize) -> Option<Vec<f64>> {
        let signed = match e.typ {
            5 => false,
            10 => true,
            _ => return None,
        };
        let n = usize::try_from(e.count).ok()?.min(max);
        let b = self.value(e, n * 8)?;
        b.as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                let (num, den) = (self.order.u32([c[0], c[1], c[2], c[3]]), self.order.u32([c[4], c[5], c[6], c[7]]));
                let (num, den) = if signed { (f64::from(num as i32), f64::from(den as i32)) } else { (f64::from(num), f64::from(den)) };
                (den != 0.0).then_some(num / den)
            })
            .collect()
    }

    /// ImageWidth × ImageLength of a directory (0 × 0 when missing).
    fn size(&self, entries: &[Entry]) -> (u32, u32) {
        let get = |tag| find(entries, tag).and_then(|e| self.uint(e)).unwrap_or(0);
        (get(IMAGE_WIDTH), get(IMAGE_LENGTH))
    }

    /// A DNG raw directory's DefaultCropSize (the picture the raw develops to), else its size.
    fn crop_size(&self, entries: &[Entry]) -> (u32, u32) {
        if let Some(e) = find(entries, DEFAULT_CROP_SIZE) {
            let v: Vec<u32> = match e.typ {
                5 | 10 => self
                    .rationals(e, 2)
                    .unwrap_or_default()
                    .iter()
                    .map(|&f| if f.is_finite() && f >= 0.0 { f.min(f64::from(u32::MAX)) as u32 } else { 0 })
                    .collect(),
                _ => self.uints(e, 2),
            };
            if let &[w, h] = v.as_slice()
                && w > 0
                && h > 0
            {
                return (w, h);
            }
        }
        self.size(entries)
    }
}

/// Degrees from up to three GPS rationals (degrees, minutes, seconds), negative when `negative`
/// (a south / west reference) or when the writer stored a negative value itself.
fn coordinate(parts: &[f64], negative: bool) -> Option<f64> {
    let deg = *parts.first()?;
    let min = parts.get(1).copied().unwrap_or(0.0);
    let sec = parts.get(2).copied().unwrap_or(0.0);
    let v = deg.abs() + min.abs() / 60.0 + sec.abs() / 3600.0;
    let negative = negative || deg < 0.0;
    v.is_finite().then_some(if negative { -v } else { v })
}

/// [latitude, longitude] from a GPS directory; `None` when incomplete, out of range or exactly
/// 0°, 0° (what receivers without a fix write).
fn gps<S: ByteSource + ?Sized>(t: &Tiff<'_, S>, entries: &[Entry]) -> Option<[f64; 2]> {
    let lat = t.rationals(find(entries, GPS_LATITUDE)?, 3)?;
    let lon = t.rationals(find(entries, GPS_LONGITUDE)?, 3)?;
    let is = |tag, c: char| find(entries, tag).and_then(|e| t.ascii(e)).is_some_and(|r| r.starts_with([c, c.to_ascii_lowercase()]));
    let lat = coordinate(&lat, is(GPS_LATITUDE_REF, 'S'))?;
    let lon = coordinate(&lon, is(GPS_LONGITUDE_REF, 'W'))?;
    let valid = (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) && !(lat == 0.0 && lon == 0.0);
    valid.then_some([lat, lon])
}

/// The EXIF data of the TIFF structure at `base` (a JPEG `APP1` payload after "Exif\0\0", a PNG
/// `eXIf` chunk, or a whole TIFF / DNG file at 0).
pub(crate) fn read_exif<S: ByteSource + ?Sized>(src: &S, base: u64) -> Exif {
    let mut x = Exif::default();
    let Some((t, ifd0)) = Tiff::open(src, base) else { return x };
    let e0 = t.ifd(ifd0);
    x.orientation = find(&e0, ORIENTATION).and_then(|e| t.uint(e)).and_then(|v| u16::try_from(v).ok()).filter(|v| (1..=8).contains(v));
    (x.width, x.height) = t.size(&e0);
    // a DNG's IFD0 is a reduced-resolution preview (NewSubfileType bit 0); the raw image is the
    // SubIFD of NewSubfileType 0
    if find(&e0, NEW_SUBFILE_TYPE).and_then(|e| t.uint(e)).is_some_and(|v| v & 1 != 0) {
        for off in find(&e0, SUB_IFDS).map(|e| t.uints(e, 8)).unwrap_or_default() {
            let sub = t.ifd(off);
            if !sub.is_empty() && find(&sub, NEW_SUBFILE_TYPE).and_then(|e| t.uint(e)).unwrap_or(0) == 0 {
                (x.width, x.height) = t.crop_size(&sub);
                break;
            }
        }
    }
    if let Some(off) = find(&e0, EXIF_IFD).and_then(|e| t.uint(e)) {
        let ex = t.ifd(off);
        let stamp = |date, offset| Some((find(&ex, date).and_then(|e| t.ascii(e))?, find(&ex, offset).and_then(|e| t.ascii(e))));
        x.original = stamp(DATE_TIME_ORIGINAL, OFFSET_TIME_ORIGINAL);
        x.digitized = stamp(DATE_TIME_DIGITIZED, OFFSET_TIME_DIGITIZED);
    }
    if let Some(off) = find(&e0, GPS_IFD).and_then(|e| t.uint(e)) {
        x.location = gps(&t, &t.ifd(off));
    }
    x
}

// ---------- still formats ----------

/// JPEG (ITU-T T.81 Annex B): the marker segments up to the first scan. The first `APP1` that
/// starts with "Exif\0" holds the EXIF data; the first frame header (SOF0–SOF15 except DHT, JPG
/// and DAC) the size.
fn jpeg<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    let (mut have_exif, mut have_size) = (false, false);
    let mut pos: u64 = 2;
    for _ in 0..MAX_SEGMENTS {
        let Some([ff, marker]) = read::<2, S>(src, pos) else { break };
        if ff != 0xFF {
            break;
        }
        match marker {
            // fill byte before a marker
            0xFF => {
                pos = pos.saturating_add(1);
                continue;
            }
            // markers without a length: TEM, RST0–7, SOI
            0x01 | 0xD0..=0xD8 => {
                pos = pos.saturating_add(2);
                continue;
            }
            // end of image, start of scan: the headers are over
            0xD9 | 0xDA => break,
            _ => {}
        }
        let Some(len) = read::<2, S>(src, pos.saturating_add(2)).map(u16::from_be_bytes) else { break };
        if len < 2 {
            break;
        }
        let body = pos.saturating_add(4);
        if marker == 0xE1 && !have_exif {
            if let Some(b) = read_upto(src, body, u64::from(len) - 2, usize::from(u16::MAX))
                && b.starts_with(b"Exif\0")
            {
                out.exif = read_exif(b.as_slice(), 6);
                have_exif = true;
            }
        } else if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) && !have_size {
            // precision, number of lines, samples per line
            if let Some([_, h0, h1, w0, w1]) = read::<5, S>(src, body) {
                out.height = u32::from(u16::from_be_bytes([h0, h1]));
                out.width = u32::from(u16::from_be_bytes([w0, w1]));
                have_size = true;
            }
        }
        if have_exif && have_size {
            break;
        }
        pos = pos.saturating_add(2).saturating_add(u64::from(len));
    }
    out
}

/// PNG: the size from `IHDR` (always the first chunk), EXIF from an `eXIf` chunk.
fn png<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    if let Some(h) = read::<16, S>(src, 8)
        && [h[4], h[5], h[6], h[7]] == *b"IHDR"
    {
        out.width = u32::from_be_bytes([h[8], h[9], h[10], h[11]]);
        out.height = u32::from_be_bytes([h[12], h[13], h[14], h[15]]);
    }
    let mut pos: u64 = 8;
    for _ in 0..MAX_SEGMENTS {
        let Some(h) = read::<8, S>(src, pos) else { break };
        let len = u64::from(u32::from_be_bytes([h[0], h[1], h[2], h[3]]));
        match &[h[4], h[5], h[6], h[7]] {
            b"IEND" => break,
            b"eXIf" => {
                if let Some(b) = read_upto(src, pos.saturating_add(8), len, MAX_EXIF) {
                    // the chunk starts with the TIFF header; some writers keep JPEG's "Exif\0\0"
                    out.exif = read_exif(b.as_slice(), if b.starts_with(b"Exif\0") { 6 } else { 0 });
                }
                break;
            }
            _ => {}
        }
        // length, type, data, CRC
        pos = pos.saturating_add(12).saturating_add(len);
    }
    out
}

/// GIF: the logical screen size.
fn gif<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    if let Some(h) = read::<10, S>(src, 0)
        && h.starts_with(b"GIF8")
    {
        out.width = u32::from(u16::from_le_bytes([h[6], h[7]]));
        out.height = u32::from(u16::from_le_bytes([h[8], h[9]]));
    }
    out
}

/// WebP (RFC 9649): the canvas of an extended file, else the lossy or lossless bitstream's size.
fn webp<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    let Some(h) = read_upto(src, 0, 30, 30) else { return out };
    if h.get(0..4) != Some(&b"RIFF"[..]) || h.get(8..12) != Some(&b"WEBP"[..]) {
        return out;
    }
    let size = match h.get(12..16) {
        // VP8X: 24-bit canvas width − 1 and height − 1
        Some(b"VP8X") => le24(&h, 24).zip(le24(&h, 27)).map(|(w, ht)| (w + 1, ht + 1)),
        // VP8L: signature 0x2F, then 14-bit width − 1 and height − 1
        Some(b"VP8L") if h.get(20) == Some(&0x2F) => le32(&h, 21).map(|b| ((b & 0x3FFF) + 1, ((b >> 14) & 0x3FFF) + 1)),
        // VP8: frame tag, start code 9D 01 2A, then 14-bit width and height
        Some(b"VP8 ") if h.get(23..26) == Some(&[0x9D, 0x01, 0x2A][..]) => {
            le16(&h, 26).zip(le16(&h, 28)).map(|(w, ht)| (u32::from(w & 0x3FFF), u32::from(ht & 0x3FFF)))
        }
        _ => None,
    };
    (out.width, out.height) = size.unwrap_or((0, 0));
    out
}

/// BMP: the DIB header (OS/2 `BITMAPCOREHEADER` of 12 bytes, or 40 bytes and longer; a
/// negative height means rows top-down).
fn bmp<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    let Some(h) = read_upto(src, 0, 26, 26) else { return out };
    if !h.starts_with(b"BM") {
        return out;
    }
    let size = match le32(&h, 14) {
        Some(12) => le16(&h, 18).zip(le16(&h, 20)).map(|(w, ht)| (u32::from(w), u32::from(ht))),
        Some(n) if n >= 40 => le32(&h, 18).zip(le32(&h, 22)).map(|(w, ht)| ((w as i32).unsigned_abs(), (ht as i32).unsigned_abs())),
        _ => None,
    };
    (out.width, out.height) = size.unwrap_or((0, 0));
    out
}

/// Targa: the image specification of the 18-byte header.
fn tga<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    if let Some(h) = read::<18, S>(src, 0) {
        out.width = u32::from(u16::from_le_bytes([h[12], h[13]]));
        out.height = u32::from(u16::from_le_bytes([h[14], h[15]]));
    }
    out
}

/// DPX: pixels per line and lines per element of the image information header (offset 768),
/// in the byte order the magic number says.
fn dpx<S: ByteSource + ?Sized>(src: &S) -> Still {
    let mut out = Still::default();
    let big = match read::<4, S>(src, 0) {
        Some(m) if &m == b"SDPX" => true,
        Some(m) if &m == b"XPDS" => false,
        _ => return out,
    };
    if let Some(h) = read::<8, S>(src, 772) {
        let get = |at| if big { be32(&h, at) } else { le32(&h, at) };
        out.width = get(0).unwrap_or(0);
        out.height = get(4).unwrap_or(0);
    }
    out
}

#[cfg(test)]
#[path = "exif_tests.rs"]
pub(crate) mod tests;
