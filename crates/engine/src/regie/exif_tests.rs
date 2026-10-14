//! Tests of the photo header readers on hand-made files: EXIF in both byte orders, DNG raw
//! sizes, every still format's size, and damaged structures (cyclic and out-of-range offsets,
//! absurd counts). The builders are shared with the library tests.

use super::*;

/// A TIFF field value for [`tiff`].
#[derive(Clone, Debug)]
pub(crate) enum Val {
    Short(Vec<u16>),
    Long(Vec<u32>),
    /// ASCII; the terminating NUL is added.
    Ascii(&'static str),
    Rational(Vec<(u32, u32)>),
    SRational(Vec<(i32, i32)>),
    /// A pointer to the directory with this index (type LONG).
    Ifd(usize),
    /// Any type, count and value field, unchecked (for damaged entries).
    Raw(u16, u32, [u8; 4]),
}

fn b16(le: bool, v: u16) -> [u8; 2] {
    if le { v.to_le_bytes() } else { v.to_be_bytes() }
}
fn b32(le: bool, v: u32) -> [u8; 4] {
    if le { v.to_le_bytes() } else { v.to_be_bytes() }
}

/// A TIFF structure: the header, then the directories in order (IFD0 first; each with a zero
/// "next" link), then the values that do not fit in their entries.
pub(crate) fn tiff(le: bool, ifds: &[Vec<(u16, Val)>]) -> Vec<u8> {
    let mut offsets = Vec::new();
    let mut at = 8u32;
    for d in ifds {
        offsets.push(at);
        at += 2 + 12 * d.len() as u32 + 4;
    }
    let data_start = at;
    let mut out: Vec<u8> = if le { b"II".to_vec() } else { b"MM".to_vec() };
    out.extend(b16(le, 42));
    out.extend(b32(le, 8));
    let mut data: Vec<u8> = Vec::new();
    for d in ifds {
        out.extend(b16(le, d.len() as u16));
        for (tag, v) in d {
            let (typ, count, bytes): (u16, u32, Vec<u8>) = match v {
                Val::Short(s) => (3, s.len() as u32, s.iter().flat_map(|&x| b16(le, x)).collect()),
                Val::Long(s) => (4, s.len() as u32, s.iter().flat_map(|&x| b32(le, x)).collect()),
                Val::Ascii(s) => (2, s.len() as u32 + 1, s.bytes().chain([0]).collect()),
                Val::Rational(s) => (5, s.len() as u32, s.iter().flat_map(|&(n, d)| b32(le, n).into_iter().chain(b32(le, d))).collect()),
                Val::SRational(s) => (10, s.len() as u32, s.iter().flat_map(|&(n, d)| b32(le, n as u32).into_iter().chain(b32(le, d as u32))).collect()),
                Val::Ifd(k) => (4, 1, b32(le, offsets[*k]).to_vec()),
                Val::Raw(typ, count, field) => {
                    out.extend(b16(le, *tag));
                    out.extend(b16(le, *typ));
                    out.extend(b32(le, *count));
                    out.extend(field);
                    continue;
                }
            };
            out.extend(b16(le, *tag));
            out.extend(b16(le, typ));
            out.extend(b32(le, count));
            if bytes.len() <= 4 {
                let mut f = bytes.clone();
                f.resize(4, 0);
                out.extend(f);
            } else {
                out.extend(b32(le, data_start + data.len() as u32));
                data.extend(&bytes);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
        }
        out.extend(b32(le, 0));
    }
    assert_eq!(out.len() as u32, data_start);
    out.extend(data);
    out
}

/// A phone photo's EXIF: taken 2026-09-05 18:22:00 at UTC+03:00, orientation 6 (turn 90°
/// clockwise), at 33°51'31.56" S, 151°12'40.2" W.
pub(crate) fn phone_exif(le: bool) -> Vec<u8> {
    tiff(
        le,
        &[
            vec![(0x010F, Val::Ascii("FilmCraft")), (ORIENTATION, Val::Short(vec![6])), (EXIF_IFD, Val::Ifd(1)), (GPS_IFD, Val::Ifd(2))],
            vec![
                (DATE_TIME_ORIGINAL, Val::Ascii("2026:09:05 18:22:00")),
                (DATE_TIME_DIGITIZED, Val::Ascii("2026:09:05 18:22:01")),
                (OFFSET_TIME_ORIGINAL, Val::Ascii("+03:00")),
            ],
            vec![
                (GPS_LATITUDE_REF, Val::Ascii("S")),
                (GPS_LATITUDE, Val::Rational(vec![(33, 1), (51, 1), (3156, 100)])),
                (GPS_LONGITUDE_REF, Val::Ascii("W")),
                (GPS_LONGITUDE, Val::Rational(vec![(151, 1), (12, 1), (402, 10)])),
            ],
        ],
    )
}

/// EXIF with only DateTimeOriginal (and OffsetTimeOriginal when given) and an orientation.
pub(crate) fn dated_exif(date: &'static str, offset: Option<&'static str>, orientation: u16) -> Vec<u8> {
    let mut ex = vec![(DATE_TIME_ORIGINAL, Val::Ascii(date))];
    if let Some(o) = offset {
        ex.push((OFFSET_TIME_ORIGINAL, Val::Ascii(o)));
    }
    tiff(false, &[vec![(ORIENTATION, Val::Short(vec![orientation])), (EXIF_IFD, Val::Ifd(1))], ex])
}

/// The latitude and longitude [`phone_exif`] says.
pub(crate) const PHONE_PLACE: [f64; 2] = [-(33.0 + 51.0 / 60.0 + 31.56 / 3600.0), -(151.0 + 12.0 / 60.0 + 40.2 / 3600.0)];

fn segment(v: &mut Vec<u8>, marker: u8, payload: &[u8]) {
    v.extend([0xFF, marker]);
    v.extend((payload.len() as u16 + 2).to_be_bytes());
    v.extend(payload);
}

/// A baseline JPEG's marker structure: JFIF, an optional EXIF `APP1` with `tiff`, a
/// quantization table, the frame header (`width` × `height` as stored), a scan and EOI.
pub(crate) fn jpeg(tiff: Option<&[u8]>, width: u16, height: u16) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    segment(&mut v, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    if let Some(t) = tiff {
        let mut p = b"Exif\0\0".to_vec();
        p.extend(t);
        segment(&mut v, 0xE1, &p);
    }
    segment(&mut v, 0xDB, &[0u8; 65]);
    let mut sof = vec![8];
    sof.extend(height.to_be_bytes());
    sof.extend(width.to_be_bytes());
    sof.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    segment(&mut v, 0xC0, &sof);
    segment(&mut v, 0xDA, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]);
    v.extend([0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD9]);
    v
}

/// A PNG's chunk structure: `IHDR` (`width` × `height`), an optional `eXIf`, `IDAT`, `IEND`
/// (CRCs are not checked by the reader and left zero).
pub(crate) fn png(width: u32, height: u32, exif: Option<&[u8]>) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        v.extend((data.len() as u32).to_be_bytes());
        v.extend(kind);
        v.extend(data);
        v.extend([0; 4]);
    };
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &[0x78, 0x9C, 0, 0]);
    if let Some(e) = exif {
        chunk(b"eXIf", e);
    }
    chunk(b"IEND", &[]);
    v
}

fn webp(chunk: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend((4 + 8 + body.len() as u32).to_le_bytes());
    v.extend(b"WEBP");
    v.extend(chunk);
    v.extend((body.len() as u32).to_le_bytes());
    v.extend(body);
    v
}

/// One hand-made file of every still format with its stored size: (bytes, format, width, height).
pub(crate) fn stills() -> Vec<(Vec<u8>, StillFormat, u32, u32)> {
    let mut gif = b"GIF89a".to_vec();
    gif.extend(320u16.to_le_bytes());
    gif.extend(200u16.to_le_bytes());
    gif.extend([0; 3]);
    let vp8x = webp(b"VP8X", &[0x08, 0, 0, 0, 0x7F, 0x07, 0x00, 0x37, 0x04, 0x00]); // 1920 × 1080
    // VP8L: 0x2F, then width − 1 (14 bits) and height − 1 (14 bits), little-endian
    let bits: u32 = (99 & 0x3FFF) | ((49 & 0x3FFF) << 14);
    let mut l = vec![0x2F];
    l.extend(bits.to_le_bytes());
    let vp8l = webp(b"VP8L", &l);
    let mut k = vec![0x50, 0x01, 0x00, 0x9D, 0x01, 0x2A];
    k.extend(640u16.to_le_bytes());
    k.extend(480u16.to_le_bytes());
    let vp8 = webp(b"VP8 ", &k);
    let mut bmp = b"BM".to_vec();
    bmp.extend([0; 12]);
    bmp.extend(40u32.to_le_bytes());
    bmp.extend(300i32.to_le_bytes());
    bmp.extend((-150i32).to_le_bytes());
    let mut core = b"BM".to_vec();
    core.extend([0; 12]);
    core.extend(12u32.to_le_bytes());
    core.extend(64u16.to_le_bytes());
    core.extend(48u16.to_le_bytes());
    let mut tga = vec![0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    tga.extend(720u16.to_le_bytes());
    tga.extend(576u16.to_le_bytes());
    tga.extend([24, 0]);
    let dpx = |big: bool| {
        let mut d = vec![0u8; 800];
        d[..4].copy_from_slice(if big { b"SDPX" } else { b"XPDS" });
        let (w, h) = if big { (2048u32.to_be_bytes(), 1556u32.to_be_bytes()) } else { (2048u32.to_le_bytes(), 1556u32.to_le_bytes()) };
        d[772..776].copy_from_slice(&w);
        d[776..780].copy_from_slice(&h);
        d
    };
    let tif = tiff(true, &[vec![(IMAGE_WIDTH, Val::Long(vec![1200])), (IMAGE_LENGTH, Val::Short(vec![800])), (ORIENTATION, Val::Short(vec![8]))]]);
    vec![
        (jpeg(Some(&phone_exif(false)), 4000, 3000), StillFormat::Jpeg, 4000, 3000),
        (png(800, 600, None), StillFormat::Png, 800, 600),
        (gif, StillFormat::Gif, 320, 200),
        (vp8x, StillFormat::Webp, 1920, 1080),
        (vp8l, StillFormat::Webp, 100, 50),
        (vp8, StillFormat::Webp, 640, 480),
        (bmp, StillFormat::Bmp, 300, 150),
        (core, StillFormat::Bmp, 64, 48),
        (tga, StillFormat::Tga, 720, 576),
        (dpx(true), StillFormat::Dpx, 2048, 1556),
        (dpx(false), StillFormat::Dpx, 2048, 1556),
        (tif, StillFormat::Tiff, 1200, 800),
        (dng(), StillFormat::Tiff, 5976, 3984),
    ]
}

/// A DNG's structure: IFD0 a 256 × 171 preview (NewSubfileType 1) with a SubIFD holding the
/// 6000 × 4000 raw image whose DefaultCropSize is 5976 × 3984.
pub(crate) fn dng() -> Vec<u8> {
    tiff(
        false,
        &[
            vec![
                (NEW_SUBFILE_TYPE, Val::Long(vec![1])),
                (IMAGE_WIDTH, Val::Short(vec![256])),
                (IMAGE_LENGTH, Val::Short(vec![171])),
                (SUB_IFDS, Val::Ifd(1)),
                (EXIF_IFD, Val::Ifd(2)),
            ],
            vec![
                (NEW_SUBFILE_TYPE, Val::Long(vec![0])),
                (IMAGE_WIDTH, Val::Long(vec![6000])),
                (IMAGE_LENGTH, Val::Long(vec![4000])),
                (DEFAULT_CROP_SIZE, Val::Rational(vec![(5976, 1), (3984, 1)])),
            ],
            vec![(DATE_TIME_ORIGINAL, Val::Ascii("2025:12:24 17:00:00"))],
        ],
    )
}

#[test]
fn exif_of_a_jpeg_in_both_byte_orders() {
    for le in [true, false] {
        let file = jpeg(Some(&phone_exif(le)), 4000, 3000);
        let s = still(file.as_slice(), StillFormat::Jpeg);
        assert_eq!((s.width, s.height), (4000, 3000), "stored size (le {le})");
        assert_eq!(s.exif.orientation, Some(6));
        assert_eq!(s.exif.original, Some(("2026:09:05 18:22:00".to_string(), Some("+03:00".to_string()))));
        assert_eq!(s.exif.digitized, Some(("2026:09:05 18:22:01".to_string(), None)));
        let [lat, lon] = s.exif.location.unwrap();
        assert!((lat - PHONE_PLACE[0]).abs() < 1e-9 && (lon - PHONE_PLACE[1]).abs() < 1e-9, "{lat} {lon}");
    }
    // no EXIF: the size alone
    let s = still(jpeg(None, 640, 480).as_slice(), StillFormat::Jpeg);
    assert_eq!((s.width, s.height, s.exif), (640, 480, Exif::default()));
}

#[test]
fn every_still_format_gives_its_size() {
    for (i, (bytes, format, w, h)) in stills().into_iter().enumerate() {
        let s = still(bytes.as_slice(), format);
        assert_eq!((s.width, s.height), (w, h), "still {i} ({format:?})");
    }
    // the TIFF's orientation and the DNG's capture date come along
    let tif = still(stills()[11].0.as_slice(), StillFormat::Tiff);
    assert_eq!(tif.exif.orientation, Some(8));
    let raw = still(dng().as_slice(), StillFormat::Tiff);
    assert_eq!(raw.exif.original, Some(("2025:12:24 17:00:00".to_string(), None)));
}

#[test]
fn png_exif_with_and_without_the_jpeg_prefix() {
    let tiff = phone_exif(true);
    let mut prefixed = b"Exif\0\0".to_vec();
    prefixed.extend(&tiff);
    for e in [tiff, prefixed] {
        let s = still(png(1024, 768, Some(&e)).as_slice(), StillFormat::Png);
        assert_eq!((s.width, s.height), (1024, 768));
        assert_eq!(s.exif.orientation, Some(6));
        assert_eq!(s.exif.original.map(|(d, _)| d), Some("2026:09:05 18:22:00".to_string()));
    }
}

#[test]
fn gps_references_ranges_and_zero_denominators() {
    let place = |lat_ref: &'static str, lat: Val, lon_ref: &'static str, lon: Val| {
        let t = tiff(true, &[vec![(GPS_IFD, Val::Ifd(1))], vec![(1, Val::Ascii(lat_ref)), (2, lat), (3, Val::Ascii(lon_ref)), (4, lon)]]);
        read_exif(t.as_slice(), 0).location
    };
    let r = |v: &[(u32, u32)]| Val::Rational(v.to_vec());
    // north / east, and lower-case references
    assert_eq!(place("N", r(&[(48, 1), (30, 1), (0, 1)]), "E", r(&[(2, 1), (15, 1), (0, 1)])), Some([48.5, 2.25]));
    assert_eq!(place("s", r(&[(10, 1)]), "w", r(&[(20, 1)])), Some([-10.0, -20.0]));
    // a negative signed value stays negative
    assert_eq!(place("N", Val::SRational(vec![(-10, 1), (30, 1)]), "E", r(&[(20, 1)])), Some([-10.5, 20.0]));
    // a zero denominator, a latitude beyond 90°, 0°/0° (no fix): unknown
    assert_eq!(place("N", r(&[(48, 0), (30, 1), (0, 1)]), "E", r(&[(2, 1)])), None);
    assert_eq!(place("N", r(&[(95, 1)]), "E", r(&[(2, 1)])), None);
    assert_eq!(place("N", r(&[(0, 1), (0, 1), (0, 1)]), "E", r(&[(0, 1), (0, 1), (0, 1)])), None);
    // the wrong field type (LONG for RATIONAL): unknown
    assert_eq!(place("N", Val::Long(vec![48]), "E", r(&[(2, 1)])), None);
}

#[test]
fn blank_dates_and_out_of_range_orientation() {
    let t = tiff(true, &[vec![(ORIENTATION, Val::Short(vec![9])), (EXIF_IFD, Val::Ifd(1))], vec![(DATE_TIME_ORIGINAL, Val::Ascii("    :  :     :  :  "))]]);
    let x = read_exif(t.as_slice(), 0);
    assert_eq!(x.orientation, None);
    // kept raw: the date parser turns it into "unknown"
    assert_eq!(x.original, Some((":  :     :  :".to_string(), None)));
}

#[test]
fn cyclic_and_out_of_range_offsets_end_quietly() {
    let le = true;
    let at = |v: u32| b32(le, v);
    // every pointer leads back to IFD0 (offset 8), past the end or nowhere; IFD0 claims to be a
    // preview so its SubIFDs are visited; a "next IFD" loop is never followed
    let t = tiff(
        le,
        &[vec![
            (NEW_SUBFILE_TYPE, Val::Long(vec![1])),
            (ORIENTATION, Val::Short(vec![3])),
            (SUB_IFDS, Val::Long(vec![8, 8, 0xFFFF_FFF0, u32::MAX])),
            (EXIF_IFD, Val::Raw(4, 1, at(8))),
            (GPS_IFD, Val::Raw(4, 1, at(u32::MAX))),
            // a value of 4 billion RATIONALs, and one far past the end
            (0x9000, Val::Raw(5, u32::MAX, at(16))),
            (0x9001, Val::Raw(2, 64, at(0x7FFF_FFFF))),
        ]],
    );
    let mut looped = t.clone();
    // IFD0's next link points at itself
    let next = 8 + 2 + 12 * 7;
    looped[next..next + 4].copy_from_slice(&at(8));
    for t in [t, looped] {
        let x = read_exif(t.as_slice(), 0);
        assert_eq!(x.orientation, Some(3));
        assert_eq!((x.original, x.location), (None, None));
    }
    // IFD0 beyond the end, at u32::MAX, a header that is no TIFF, an entry count of 65535
    // with no entries behind it
    let mut far = tiff(le, &[vec![(ORIENTATION, Val::Short(vec![6]))]]);
    far[4..8].copy_from_slice(&at(u32::MAX));
    assert_eq!(read_exif(far.as_slice(), 0), Exif::default());
    assert_eq!(read_exif(b"II*\0\x08\0\0\0\xFF\xFF".as_slice(), 0), Exif::default());
    assert_eq!(read_exif(b"XX*\0\x08\0\0\0".as_slice(), 0), Exif::default());
    assert_eq!(read_exif(b"II".as_slice(), 0), Exif::default());
    assert_eq!(read_exif(b"".as_slice(), u64::MAX), Exif::default());
}

#[test]
fn jpeg_marker_walk_is_bounded() {
    // fill bytes, a zero-length segment, a segment running past the end, endless fill bytes
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xFF];
    segment(&mut v, 0xE1, b"Exif\0\0II*\0");
    v.extend([0xFF, 0xC0, 0x00, 0x00]);
    assert_eq!(still(v.as_slice(), StillFormat::Jpeg), Still::default());
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xF0, b'E', b'x'];
    v.extend([0u8; 16]);
    assert_eq!(still(v.as_slice(), StillFormat::Jpeg).width, 0);
    let v: Vec<u8> = [0xFF, 0xD8].into_iter().chain(std::iter::repeat_n(0xFF, 10_000)).collect();
    assert_eq!(still(v.as_slice(), StillFormat::Jpeg), Still::default());
}
