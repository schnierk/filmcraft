//! HEIF / HEIC and AVIF still images: iPhone photos (HEVC tile grids), Canon / Sony `.hif`,
//! AVIF. The container is read by `filmcraft_isobmff::heif`; this module decodes the primary image
//! to straight-alpha sRGB RGBA8 with our own HEVC and AV1 decoders.
//!
//! - **Items:** a coded image (`hvc1` + `hvcC`, `av01` + `av1C`), a `grid` of coded tiles (decoded
//!   in parallel, placed row by row and cropped to the grid's output size), an `iden` derived image
//!   and a `tmap` gain-map image (its base image is shown; the gain map is ignored). Derived images
//!   are followed at most [`MAX_DEPTH`] levels deep and cycles are refused.
//! - **Transforms** in the order the standard fixes: clean aperture (`clap`), rotation (`irot`,
//!   anti-clockwise), mirror (`imir`). Exif orientation is ignored (MIAF: the item properties
//!   rule).
//! - **Colour:** Y'CbCr → R'G'B' with the `colr` `nclx` matrix and range, else what the bitstream
//!   signals (HEVC VUI, AV1 sequence header), else the MIAF defaults (BT.601 matrix, full range).
//!   Each chroma sample covers the luma samples it was subsampled from (no interpolation, as
//!   libheif and FilmCraft's video frames). 4:2:0, 4:2:2, 4:4:4 and monochrome, 8–12 bits, identity
//!   matrix (RGB coding). Wide-gamut pictures (an ICC profile with Display P3 or BT.2020 primaries
//!   and the sRGB curve, or `nclx` P3 / BT.2020 primaries) are converted to BT.709 / sRGB primaries
//!   in linear light; PQ / HLG pictures are shown as coded (no tone mapping). The HEVC decoder
//!   takes 4:2:0 only, so 4:2:2 / 4:4:4 / monochrome reach here from AV1 (and future decoders).
//! - **Alpha:** an auxiliary image (`auxl` reference, `auxC`
//!   `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` or `urn:mpeg:hevc:2015:auxid:1`), premultiplied
//!   colour (`prem`) is unpremultiplied. Depth maps, mattes and HDR gain maps are ignored.
//! - [`info`] reads only the container: display size, codec, brand, transforms, alpha, the ICC
//!   profile and the Exif metadata as TIFF bytes (capture date, GPS…), without decoding pixels.
//!
//! The input is hostile: pictures are capped at [`MAX_PIXELS`], a grid must have exactly one tile
//! per cell, tiles of one size and format that cover the output, and a coded item may not declare a
//! picture larger than its `ispe` before it is decoded.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, OnceLock};

use filmcraft_color::Primaries;
use filmcraft_color::spaces::{Gamut, gamut_matrix, to_f32};
use filmcraft_isobmff::heif::{self, HeifFile, ImageGrid, Mirror, Property};
use filmcraft_isobmff::{Av1Config, ByteSource, CleanAperture, ColorInfo as Colr, FourCc, HevcConfig};
use filmcraft_media::still::{Raster, StillSource};
use filmcraft_media::{MediaError, SharedSource};
use rayon::prelude::*;

use crate::stream_color::ColorCodes;
use crate::{CodecError, Result};

/// Largest picture decoded, in pixels: the cap of the DPX, Targa and TIFF still decoders.
pub const MAX_PIXELS: u64 = 1 << 28;
/// Most derived-image levels followed (an `iden` of a `grid`…).
pub const MAX_DEPTH: usize = 8;
/// Largest coded item read.
const MAX_ITEM_BYTES: u64 = 1 << 30;
/// Most bytes a coded item may have per pixel of its picture (lossless 16-bit 4:4:4 needs about 6),
/// plus [`ITEM_OVERHEAD`]: overlapping extents cannot make a small file read gigabytes.
const MAX_BYTES_PER_PIXEL: u64 = 8;
const ITEM_OVERHEAD: u64 = 64 << 10;
/// Largest `Exif` item read.
const MAX_EXIF_BYTES: u64 = 16 << 20;
/// Largest `grid` payload read (it has 8 or 12 bytes).
const MAX_GRID_BYTES: u64 = 64;
/// Extra rows and columns a coded picture may have beyond its item's size (coding-block padding
/// that the HEVC conformance window crops away).
const CODED_PADDING: u64 = 128;

/// `auxC` types of alpha planes: MPEG-B CICP (AVIF and newer HEIF writers) and the HEVC one.
pub const ALPHA_URNS: [&str; 2] = ["urn:mpeg:mpegB:cicp:systems:auxiliary:alpha", "urn:mpeg:hevc:2015:auxid:1"];

/// The codec of a HEIF file's primary image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeifCodec {
    Hevc,
    Av1,
}

impl HeifCodec {
    /// "HEVC" or "AV1".
    pub fn name(self) -> &'static str {
        match self {
            HeifCodec::Hevc => "HEVC",
            HeifCodec::Av1 => "AV1",
        }
    }
    /// The name of the still format: "HEIC" or "AVIF".
    pub fn format(self) -> &'static str {
        match self {
            HeifCodec::Hevc => "HEIC",
            HeifCodec::Av1 => "AVIF",
        }
    }
}

/// What a HEIF / AVIF file says about its primary image, read from the container alone ([`info`]).
#[derive(Clone, Debug, PartialEq)]
pub struct HeifInfo {
    /// Display size: after the clean aperture, the rotation and the mirror.
    pub width: u32,
    pub height: u32,
    /// Size of the reconstructed image before those transforms (`ispe`, or the grid's output size).
    pub coded_width: u32,
    pub coded_height: u32,
    pub codec: HeifCodec,
    /// The `ftyp` major brand (`heic`, `heix`, `mif1`, `avif`…).
    pub brand: FourCc,
    /// Anti-clockwise quarter turns (`irot`): 3 is the usual iPhone portrait photo.
    pub rotation: u8,
    pub mirror: Option<Mirror>,
    /// Whether a clean aperture crops the image.
    pub cropped: bool,
    /// (rows, columns) when the image is a grid of tiles.
    pub grid: Option<(u32, u32)>,
    /// Bits per luma sample.
    pub bit_depth: u8,
    pub has_alpha: bool,
    /// The embedded ICC profile (Display P3 in iPhone photos).
    pub icc_profile: Option<Vec<u8>>,
    /// The Exif metadata as TIFF bytes (from the TIFF header on: the `Exif` item without its offset
    /// field and `Exif\0\0` prefix). Its orientation tag is not used: the item properties rule.
    pub exif: Option<Vec<u8>>,
}

/// A decoded primary image: straight-alpha RGBA8 (sRGB, BT.709 primaries), top row first.
#[derive(Clone, Debug)]
pub struct HeifImage {
    pub info: HeifInfo,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Whether any pixel is not fully opaque.
    pub has_alpha: bool,
}

impl HeifImage {
    pub fn into_raster(self) -> Raster {
        Raster { width: self.width, height: self.height, rgba: self.rgba, has_alpha: self.has_alpha }
    }
}

/// Whether `head` starts like a HEIF / AVIF file (an `ftyp` naming one of their brands).
pub fn sniff(head: &[u8]) -> bool {
    heif::sniff(head)
}

/// The primary image's size, codec, transforms and metadata, without decoding any pixels.
pub fn info(bytes: &[u8]) -> Result<HeifInfo> {
    let file = open_meta(bytes)?;
    Ctx { file: &file, src: bytes }.info()
}

/// Decode the primary image.
pub fn decode(bytes: &[u8]) -> Result<HeifImage> {
    decode_from(bytes)
}

/// Decode the primary image from any byte source (only the `meta` box and the items it needs are
/// read).
pub fn decode_from<S: ByteSource + ?Sized>(src: &S) -> Result<HeifImage> {
    let file = open_meta(src)?;
    decode_file(&file, src)
}

fn decode_file<S: ByteSource + ?Sized>(file: &HeifFile, src: &S) -> Result<HeifImage> {
    let ctx = Ctx { file, src };
    let info = ctx.info()?;
    let primary = ctx.primary()?;
    let img = ctx.decode_rgba(primary, &[])?;
    let has_alpha = img.px.as_chunks::<4>().0.iter().any(|p| p[3] < 255);
    Ok(HeifImage { info, width: img.w, height: img.h, rgba: img.px, has_alpha })
}

/// [`filmcraft_media::Opener`] for HEIF / AVIF, listed before the MP4 opener (both start with an
/// `ftyp`; [`crate::mp4::sniff`] leaves HEIF brands to this one).
pub fn opener(name: &str, bytes: Arc<[u8]>) -> Option<std::result::Result<SharedSource, MediaError>> {
    if !sniff(&bytes) {
        return None;
    }
    let size = ByteSource::len(&*bytes);
    Some(open_still(name, &*bytes, size, || crate::Mp4Source::open(name, bytes.clone())))
}

/// [`filmcraft_media::ReaderOpener`] for HEIF / AVIF: reads the `meta` box and the items the
/// primary image needs, not the whole file.
pub fn reader_opener(name: &str, head: &[u8], reader: &filmcraft_media::SharedReader) -> Option<std::result::Result<SharedSource, MediaError>> {
    if !sniff(head) {
        return None;
    }
    let src = crate::Src(reader.clone());
    Some(open_still(name, &src, reader.len(), || crate::Mp4Source::open_reader(name, reader.clone())))
}

/// [`filmcraft_media::still::StillDecoder`] for image sequences of HEIF / AVIF files.
pub fn decode_still(name: &str, bytes: &[u8]) -> Option<std::result::Result<Raster, MediaError>> {
    if !sniff(bytes) {
        return None;
    }
    Some(decode(bytes).map(HeifImage::into_raster).map_err(|e| MediaError::from(with_name(name, e))))
}

/// Open a HEIF file as a still; a file with only an image sequence (`moov`, no primary image)
/// opens as a movie through the MP4 source, as before this opener existed.
fn open_still<S: ByteSource + ?Sized>(
    name: &str,
    src: &S,
    size: u64,
    movie: impl FnOnce() -> Result<crate::Mp4Source>,
) -> std::result::Result<SharedSource, MediaError> {
    let file = heif::parse(src).map_err(|e| with_name(name, container(e)))?;
    let still = file.has_meta && file.primary_item.is_some() && file.handler == b"pict";
    if !still && file.has_movie {
        return Ok(Arc::new(movie().map_err(|e| with_name(name, e))?) as SharedSource);
    }
    let file = image_file(file).map_err(|e| with_name(name, e))?;
    let img = decode_file(&file, src).map_err(|e| with_name(name, e))?;
    let codec = img.info.codec.format();
    Ok(Arc::new(StillSource::from_rgba(name, img.width, img.height, img.rgba, img.has_alpha, size).with_codec(codec)) as SharedSource)
}

fn with_name(name: &str, e: CodecError) -> CodecError {
    match e {
        CodecError::Unsupported(s) => CodecError::Unsupported(format!("{name}: {s}")),
        CodecError::Decode(s) => CodecError::Decode(format!("{name}: {s}")),
        CodecError::Container(s) => CodecError::Container(format!("{name}: {s}")),
        CodecError::Cancelled => CodecError::Cancelled,
    }
}

fn container(e: filmcraft_isobmff::Error) -> CodecError {
    match e {
        filmcraft_isobmff::Error::Unsupported(s) => CodecError::Unsupported(format!("HEIF: {s}")),
        e => CodecError::Container(format!("HEIF: {e}")),
    }
}

fn invalid(msg: String) -> CodecError {
    CodecError::Container(format!("HEIF: {msg}"))
}

fn unsupported(msg: String) -> CodecError {
    CodecError::Unsupported(format!("HEIF: {msg}"))
}

/// Parse a file and check it describes an image.
fn open_meta<S: ByteSource + ?Sized>(src: &S) -> Result<HeifFile> {
    image_file(heif::parse(src).map_err(container)?)
}

fn image_file(file: HeifFile) -> Result<HeifFile> {
    if !file.has_meta {
        return Err(invalid("no meta box".into()));
    }
    if file.handler != b"pict" {
        return Err(unsupported(format!("meta handler '{}' is not an image handler", file.handler)));
    }
    let primary = file.primary_item.ok_or_else(|| invalid("no primary item".into()))?;
    if file.item(primary).is_none() {
        return Err(invalid(format!("primary item {primary} is not described")));
    }
    Ok(file)
}

/// The transformative properties of an item, applied clean aperture → rotation → mirror.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Transforms {
    clap: Option<CleanAperture>,
    rotation: u8,
    mirror: Option<Mirror>,
}

impl Transforms {
    fn is_identity(&self) -> bool {
        self.clap.is_none() && self.rotation == 0 && self.mirror.is_none()
    }
    fn display_size(&self, w: u32, h: u32) -> (u32, u32) {
        let (w, h) = self.clap.and_then(|c| clap_rect(c, w, h)).map_or((w, h), |r| (r.2, r.3));
        if self.rotation % 2 == 1 { (h, w) } else { (w, h) }
    }
}

/// The pixel rectangle (x, y, width, height) a clean aperture keeps of a `w` × `h` image: its size,
/// centred on the image and moved by its offsets, rounded to whole pixels (ISO/IEC 14496-12 `clap`).
/// `None` when it keeps everything or makes no sense (a zero denominator, an empty or too large
/// size, a rectangle outside the image): the whole image is shown then.
fn clap_rect(c: CleanAperture, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let q = |(n, d): (u32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
    let qs = |(n, d): (i32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
    let (cw, ch, dx, dy) = (q(c.width)?.round(), q(c.height)?.round(), qs(c.horiz_offset)?, qs(c.vert_offset)?);
    let (fw, fh) = (f64::from(w), f64::from(h));
    if !(cw >= 1.0 && ch >= 1.0 && cw <= fw && ch <= fh) {
        return None;
    }
    let (x, y) = (((fw - cw) / 2.0 + dx).round(), ((fh - ch) / 2.0 + dy).round());
    // also false for NaN
    if !(x >= 0.0 && y >= 0.0 && x + cw <= fw && y + ch <= fh) {
        return None;
    }
    let r = (x as u32, y as u32, cw as u32, ch as u32);
    (r != (0, 0, w, h)).then_some(r)
}

/// Chroma format of decoded planes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chroma {
    Mono,
    C420,
    C422,
    C444,
}

impl Chroma {
    /// (horizontal, vertical) subsampling shifts.
    fn shifts(self) -> (u32, u32) {
        match self {
            Chroma::C420 => (1, 1),
            Chroma::C422 => (1, 0),
            Chroma::Mono | Chroma::C444 => (0, 0),
        }
    }
    /// Size of one chroma plane of a `w` × `h` picture (0 × 0 for monochrome).
    fn plane_size(self, w: usize, h: usize) -> (usize, usize) {
        if self == Chroma::Mono {
            return (0, 0);
        }
        let (sx, sy) = self.shifts();
        (w.div_ceil(1 << sx), h.div_ceil(1 << sy))
    }
}

/// A decoded Y'CbCr picture: tight planes of `bits`-bit samples in u16.
#[derive(Clone, Debug)]
struct Planes {
    width: u32,
    height: u32,
    bits: u8,
    chroma: Chroma,
    y: Vec<u16>,
    u: Vec<u16>,
    v: Vec<u16>,
    /// The colour the bitstream signals (HEVC VUI, AV1 sequence header).
    signalled: Option<ColorCodes>,
}

impl Planes {
    /// Whether the plane sizes fit the picture size and chroma format.
    fn consistent(&self) -> bool {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = self.chroma.plane_size(w, h);
        w.checked_mul(h) == Some(self.y.len()) && self.u.len() == cw * ch && self.v.len() == cw * ch && (1..=16).contains(&self.bits)
    }
    fn same_format(&self, o: &Planes) -> bool {
        self.bits == o.bits && self.chroma == o.chroma
    }
}

/// Straight-alpha RGBA8.
struct Rgba {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

/// The configuration of a coded item.
#[derive(Clone, Copy)]
enum Coded<'a> {
    Hevc(&'a HevcConfig),
    Av1(&'a Av1Config),
}

impl Coded<'_> {
    fn codec(self) -> HeifCodec {
        match self {
            Coded::Hevc(_) => HeifCodec::Hevc,
            Coded::Av1(_) => HeifCodec::Av1,
        }
    }
}

/// A file and where its bytes are.
struct Ctx<'a, S: ?Sized> {
    file: &'a HeifFile,
    src: &'a S,
}

impl<S: ByteSource + ?Sized> Ctx<'_, S> {
    fn primary(&self) -> Result<u32> {
        self.file.primary_item.ok_or_else(|| invalid("no primary item".into()))
    }

    fn kind(&self, id: u32) -> Result<FourCc> {
        let item = self.file.item(id).ok_or_else(|| invalid(format!("item {id} is not described")))?;
        if item.protection_index != 0 {
            return Err(unsupported(format!("item {id} is protected (encrypted)")));
        }
        Ok(item.kind)
    }

    /// Refuse an item with an essential property we do not understand (or a link to nothing).
    fn check_essential(&self, id: u32) -> Result<()> {
        for (p, essential) in self.file.item_properties(id) {
            if !essential {
                continue;
            }
            match p {
                None => return Err(invalid(format!("item {id} has an essential property that does not exist"))),
                Some(Property::Other { kind, malformed: true }) => return Err(invalid(format!("item {id}: essential property '{kind}' is damaged"))),
                Some(Property::Other { kind, .. }) => return Err(unsupported(format!("item {id}: essential property '{kind}'"))),
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn transforms(&self, id: u32) -> Transforms {
        let mut t = Transforms::default();
        for (p, _) in self.file.item_properties(id) {
            match p {
                Some(Property::CleanAperture(c)) if t.clap.is_none() => t.clap = Some(*c),
                Some(Property::Rotation(r)) => t.rotation = *r & 3,
                Some(Property::Mirror(m)) => t.mirror = Some(*m),
                _ => {}
            }
        }
        t
    }

    fn ispe(&self, id: u32) -> Option<(u32, u32)> {
        self.file.find_property(id, |p| if let Property::ImageSize { width, height } = p { Some((*width, *height)) } else { None })
    }

    /// The decoder configuration of a coded item (`hvc1` / `av01`).
    fn coded(&self, id: u32) -> Result<Coded<'_>> {
        let kind = self.kind(id)?;
        match &kind.0 {
            b"hvc1" => self.file.find_property(id, |p| if let Property::Hevc(c) = p { Some(Coded::Hevc(c)) } else { None }),
            b"av01" => self.file.find_property(id, |p| if let Property::Av1(c) = p { Some(Coded::Av1(c)) } else { None }),
            _ => return Err(unsupported(format!("item {id} of type '{kind}' is not a coded image"))),
        }
        .ok_or_else(|| invalid(format!("item {id} ('{kind}') has no decoder configuration")))
    }

    /// The size of a coded item: its `ispe`, else what its decoder configuration declares.
    fn coded_item_size(&self, id: u32) -> Result<(u32, u32)> {
        if let Some(s) = self.ispe(id) {
            return Ok(s);
        }
        match self.coded(id)? {
            Coded::Hevc(c) => {
                let nal = c.sps().into_iter().next().ok_or_else(|| invalid(format!("item {id} has neither ispe nor an SPS")))?;
                let sps = filmcraft_hevc::params::Sps::parse(&filmcraft_bitstream::unescape_rbsp(nal.get(2..).unwrap_or(&[])))
                    .map_err(|e| invalid(format!("item {id}: SPS: {e}")))?;
                let (_, _, w, h) = sps.crop_rect();
                Ok((w, h))
            }
            Coded::Av1(c) => {
                let (_, payload) = crate::hw_frame::obus(&c.config_obus)
                    .into_iter()
                    .find(|(t, _)| *t == 1)
                    .ok_or_else(|| invalid(format!("item {id} has neither ispe nor a sequence header")))?;
                let seq = filmcraft_av1::SequenceHeader::parse(payload).map_err(|e| invalid(format!("item {id}: sequence header: {e}")))?;
                Ok((seq.max_frame_width, seq.max_frame_height))
            }
        }
    }

    /// The bytes of coded item `id` of a `w` × `h` picture: at most the size of the file (with its
    /// `idat`) and [`MAX_BYTES_PER_PIXEL`] per pixel.
    fn coded_data(&self, id: u32, (w, h): (u32, u32)) -> Result<Vec<u8>> {
        let per_pixel = (u64::from(w) * u64::from(h)).saturating_mul(MAX_BYTES_PER_PIXEL).saturating_add(ITEM_OVERHEAD);
        let in_file = self.src.len().saturating_add(self.file.idat.len() as u64);
        self.file.read_item(self.src, id, per_pixel.min(in_file).min(MAX_ITEM_BYTES)).map_err(container)
    }

    /// The `dimg` inputs of a derived item.
    fn inputs(&self, id: u32) -> Vec<u32> {
        self.file.references_from(id, b"dimg")
    }

    /// The image an `iden` or `tmap` item derives from (a `tmap`'s first input is its base image).
    fn derived_source(&self, id: u32, kind: FourCc) -> Result<u32> {
        let inputs = self.inputs(id);
        match (&kind.0, inputs.as_slice()) {
            (b"iden", [one]) => Ok(*one),
            (b"tmap", [base, ..]) => Ok(*base),
            _ => Err(invalid(format!("'{kind}' item {id} has {} inputs", inputs.len()))),
        }
    }

    fn enter(&self, id: u32, chain: &[u32]) -> Result<Vec<u32>> {
        if chain.contains(&id) {
            return Err(invalid(format!("item {id} derives from itself")));
        }
        if chain.len() >= MAX_DEPTH {
            return Err(unsupported(format!("derived images nested more than {MAX_DEPTH} deep")));
        }
        let mut c = chain.to_vec();
        c.push(id);
        Ok(c)
    }

    /// Size of item `id` before its own transforms.
    fn reconstructed_size(&self, id: u32, chain: &[u32]) -> Result<(u32, u32)> {
        let chain = self.enter(id, chain)?;
        let kind = self.kind(id)?;
        match &kind.0 {
            b"hvc1" | b"av01" => self.coded_item_size(id),
            b"grid" => {
                let g = self.grid(id)?;
                Ok((g.output_width, g.output_height))
            }
            b"iden" | b"tmap" => match self.ispe(id) {
                Some(s) => Ok(s),
                None => {
                    let src = self.derived_source(id, kind)?;
                    let (w, h) = self.reconstructed_size(src, &chain)?;
                    Ok(self.transforms(src).display_size(w, h))
                }
            },
            _ => Err(unsupported(format!("image item type '{kind}'"))),
        }
    }

    fn codec(&self, id: u32, chain: &[u32]) -> Result<HeifCodec> {
        let chain = self.enter(id, chain)?;
        let kind = self.kind(id)?;
        match &kind.0 {
            b"hvc1" => Ok(HeifCodec::Hevc),
            b"av01" => Ok(HeifCodec::Av1),
            b"grid" => {
                let first = self.inputs(id).first().copied().ok_or_else(|| invalid(format!("grid {id} has no tiles")))?;
                self.codec(first, &chain)
            }
            b"iden" | b"tmap" => self.codec(self.derived_source(id, kind)?, &chain),
            _ => Err(unsupported(format!("image item type '{kind}'"))),
        }
    }

    fn grid(&self, id: u32) -> Result<ImageGrid> {
        let data = self.file.read_item(self.src, id, MAX_GRID_BYTES).map_err(container)?;
        ImageGrid::parse(&data).map_err(container)
    }

    /// The first coded item an image is made of (itself, a tile, a source).
    fn first_coded(&self, id: u32, chain: &[u32]) -> Result<u32> {
        let chain = self.enter(id, chain)?;
        let kind = self.kind(id)?;
        match &kind.0 {
            b"hvc1" | b"av01" => Ok(id),
            b"grid" => self.first_coded(self.inputs(id).first().copied().ok_or_else(|| invalid(format!("grid {id} has no tiles")))?, &chain),
            b"iden" | b"tmap" => self.first_coded(self.derived_source(id, kind)?, &chain),
            _ => Err(unsupported(format!("image item type '{kind}'"))),
        }
    }

    /// The `nclx` code points and ICC profile of an image (a grid without its own falls back to its
    /// first tile's).
    fn color_props(&self, id: u32) -> (Option<ColorCodes>, Option<&[u8]>) {
        let of = |id: u32| {
            let mut nclx = None;
            let mut icc = None;
            for (p, _) in self.file.item_properties(id) {
                match p {
                    Some(Property::Color(Colr::Nclx { primaries, transfer, matrix, full_range })) if nclx.is_none() => {
                        nclx = Some(ColorCodes::from_wide((*primaries).into(), (*transfer).into(), (*matrix).into(), Some(*full_range)))
                    }
                    Some(Property::Color(Colr::Icc { profile, .. })) if icc.is_none() => icc = Some(profile.as_slice()),
                    _ => {}
                }
            }
            (nclx, icc)
        };
        let (mut nclx, mut icc) = of(id);
        if (nclx.is_none() || icc.is_none())
            && let Ok(first) = self.first_coded(id, &[])
            && first != id
        {
            let (n, i) = of(first);
            nclx = nclx.or(n);
            icc = icc.or(i);
        }
        (nclx, icc)
    }

    /// The alpha plane image of `master`.
    fn alpha_item(&self, master: u32) -> Option<u32> {
        self.file.references_to(master, b"auxl").into_iter().find(|&a| {
            self.file
                .find_property(a, |p| if let Property::Auxiliary { urn, .. } = p { ALPHA_URNS.contains(&urn.as_str()).then_some(()) } else { None })
                .is_some()
        })
    }

    fn bit_depth(&self, id: u32) -> u8 {
        if let Some(b) = self.file.find_property(id, |p| if let Property::PixelInfo(b) = p { b.first().copied() } else { None }) {
            return b;
        }
        match self.first_coded(id, &[]).and_then(|c| self.coded(c)) {
            Ok(Coded::Hevc(c)) => c.bit_depth_luma,
            Ok(Coded::Av1(c)) => match (c.high_bitdepth, c.twelve_bit) {
                (true, true) => 12,
                (true, false) => 10,
                _ => 8,
            },
            Err(_) => 8,
        }
    }

    /// The Exif item describing `primary` (or the only one), as TIFF bytes.
    fn exif(&self, primary: u32) -> Option<Vec<u8>> {
        let exif_items: Vec<u32> = self.file.items.iter().filter(|i| i.kind == b"Exif").map(|i| i.id).collect();
        let described = self.file.references_to(primary, b"cdsc");
        let id = exif_items.iter().copied().find(|i| described.contains(i)).or_else(|| exif_items.first().copied())?;
        match self.file.read_item(self.src, id, MAX_EXIF_BYTES) {
            Ok(data) => heif::exif_tiff(&data).map(<[u8]>::to_vec),
            Err(e) => {
                log::warn!("HEIF: Exif item {id} unreadable: {e}");
                None
            }
        }
    }

    fn info(&self) -> Result<HeifInfo> {
        let primary = self.primary()?;
        let (coded_width, coded_height) = self.reconstructed_size(primary, &[])?;
        let t = self.transforms(primary);
        let (width, height) = t.display_size(coded_width, coded_height);
        let grid = if self.kind(primary)? == b"grid" {
            let g = self.grid(primary)?;
            Some((g.rows, g.columns))
        } else {
            None
        };
        Ok(HeifInfo {
            width,
            height,
            coded_width,
            coded_height,
            codec: self.codec(primary, &[])?,
            brand: self.file.major_brand,
            rotation: t.rotation,
            mirror: t.mirror,
            cropped: t.clap.and_then(|c| clap_rect(c, coded_width, coded_height)).is_some(),
            grid,
            bit_depth: self.bit_depth(primary),
            has_alpha: self.alpha_item(primary).is_some(),
            icc_profile: self.color_props(primary).1.map(<[u8]>::to_vec),
            exif: self.exif(primary),
        })
    }

    /// Decode item `id` to RGBA with its own transforms applied.
    fn decode_rgba(&self, id: u32, chain: &[u32]) -> Result<Rgba> {
        let inner = self.enter(id, chain)?;
        self.check_essential(id)?;
        let kind = self.kind(id)?;
        let img = match &kind.0 {
            b"hvc1" | b"av01" | b"grid" => {
                let planes = self.decode_planes(id, chain)?;
                let conv = self.conversion(id, &planes);
                let alpha = self.alpha_item(id).and_then(|a| match self.decode_planes(a, &inner) {
                    Ok(p) => {
                        let full = self.color_props(a).0.and_then(|c| c.full_range).or(p.signalled.and_then(|c| c.full_range)).unwrap_or(true);
                        let premultiplied = self.file.references_from(id, b"prem").contains(&a);
                        Some((p, full, premultiplied))
                    }
                    Err(e) => {
                        log::warn!("HEIF: alpha plane {a} of item {id} not decoded, shown opaque: {e}");
                        None
                    }
                });
                to_rgba(&planes, &conv, alpha.as_ref().map(|(p, full, prem)| Alpha { planes: p, full_range: *full, premultiplied: *prem }))?
            }
            b"iden" | b"tmap" => self.decode_rgba(self.derived_source(id, kind)?, &inner)?,
            _ => return Err(unsupported(format!("image item type '{kind}'"))),
        };
        Ok(apply_transforms(img, self.transforms(id)))
    }

    /// Decode a coded item or a grid of coded tiles to Y'CbCr planes (no transforms).
    fn decode_planes(&self, id: u32, chain: &[u32]) -> Result<Planes> {
        let chain = self.enter(id, chain)?;
        self.check_essential(id)?;
        let kind = self.kind(id)?;
        match &kind.0 {
            b"hvc1" | b"av01" => {
                let (w, h) = self.coded_item_size(id)?;
                check_pixels(w, h)?;
                let data = self.coded_data(id, (w, h))?;
                let p = decode_guarded(self.coded(id)?, &data, (w, h), id)?;
                if (p.width, p.height) != (w, h) {
                    log::warn!("HEIF: item {id} decodes to {}x{}, its ispe says {w}x{h}", p.width, p.height);
                }
                Ok(p)
            }
            b"grid" => self.decode_grid(id, &chain),
            _ => Err(unsupported(format!("item {id} of type '{kind}' as a coded image or grid"))),
        }
    }

    fn decode_grid(&self, id: u32, chain: &[u32]) -> Result<Planes> {
        let g = self.grid(id)?;
        let (ow, oh) = (g.output_width, g.output_height);
        check_pixels(ow, oh)?;
        let tiles = self.inputs(id);
        if tiles.len() != g.cells() {
            return Err(invalid(format!("grid {id} has {} tiles for {} × {} cells", tiles.len(), g.rows, g.columns)));
        }
        let first = *tiles.first().ok_or_else(|| invalid(format!("grid {id} has no tiles")))?;
        let (tw, th) = self.coded_item_size(first)?;
        let mut jobs = Vec::with_capacity(tiles.len());
        for &t in &tiles {
            if chain.contains(&t) {
                return Err(invalid(format!("grid {id} uses item {t} as a tile of itself")));
            }
            self.check_essential(t)?;
            let coded = self.coded(t)?;
            if coded.codec() != self.coded(first)?.codec() {
                return Err(unsupported(format!("grid {id} mixes HEVC and AV1 tiles")));
            }
            if !self.transforms(t).is_identity() {
                return Err(unsupported(format!("grid {id}: tile {t} has its own transforms")));
            }
            if self.coded_item_size(t)? != (tw, th) {
                return Err(invalid(format!("grid {id}: tile {t} is not {tw}x{th} like the first tile")));
            }
            jobs.push((t, coded));
        }
        // The tiles must cover the output, without a row or column that lies wholly outside it.
        let covers = |n: u32, tile: u32, out: u32| {
            let n = u64::from(n);
            let (tile, out) = (u64::from(tile), u64::from(out));
            tile > 0 && n * tile >= out && (n - 1) * tile < out
        };
        if !covers(g.columns, tw, ow) || !covers(g.rows, th, oh) {
            return Err(invalid(format!("grid {id}: {} × {} tiles of {tw}x{th} do not fit its {ow}x{oh} output", g.columns, g.rows)));
        }
        let canvas = (u64::from(g.columns) * u64::from(tw)).saturating_mul(u64::from(g.rows) * u64::from(th));
        if canvas > MAX_PIXELS {
            return Err(unsupported(format!("grid {id}: a {canvas}-pixel canvas is larger than {MAX_PIXELS}")));
        }
        // read the tiles one after the other, decode them in parallel
        let mut inputs = Vec::with_capacity(jobs.len());
        for (t, coded) in jobs {
            inputs.push((t, coded, self.coded_data(t, (tw, th))?));
        }
        let decoded: Vec<Planes> = inputs.par_iter().map(|(t, coded, data)| decode_guarded(*coded, data, (tw, th), *t)).collect::<Result<_>>()?;
        let reference = decoded.first().ok_or_else(|| invalid(format!("grid {id} has no tiles")))?;
        for (p, t) in decoded.iter().zip(&tiles) {
            if (p.width, p.height) != (tw, th) {
                return Err(invalid(format!("grid {id}: tile {t} decodes to {}x{}, not {tw}x{th}", p.width, p.height)));
            }
            if !p.same_format(reference) {
                return Err(unsupported(format!("grid {id}: tile {t} has another chroma format or bit depth")));
            }
        }
        let (sx, sy) = reference.chroma.shifts();
        if (sx == 1 && tw % 2 == 1) || (sy == 1 && th % 2 == 1) {
            return Err(unsupported(format!("grid {id}: odd {tw}x{th} tiles with subsampled chroma")));
        }
        assemble(&decoded, g, (tw, th))
    }

    /// How item `id`'s Y'CbCr becomes sRGB: matrix and range from `nclx`, else the bitstream, else
    /// the MIAF defaults; primaries from a recognised ICC profile or `nclx`.
    fn conversion(&self, id: u32, planes: &Planes) -> Conversion {
        let (nclx, icc) = self.color_props(id);
        let sources: Vec<ColorCodes> = [nclx, planes.signalled].into_iter().flatten().collect();
        let matrix = sources.iter().find_map(|s| MatrixKind::from_code(s.matrix)).unwrap_or(MatrixKind::Ycc { kr: 0.299, kb: 0.114 });
        let full_range = sources.iter().find_map(|s| s.full_range).unwrap_or(true);
        let (primaries, sdr) = match icc {
            Some(profile) => match filmcraft_color::icc::srgb_curve_primaries(profile) {
                Some(p) => (p, true),
                None => {
                    log::info!("HEIF: item {id}: ICC profile not recognised (not an RGB matrix profile with the sRGB curve); colours shown as sRGB");
                    (Primaries::Bt709, true)
                }
            },
            None => {
                let primaries = match sources.iter().map(|s| s.primaries).find(|&p| p != 2) {
                    Some(9) => Primaries::Bt2020,
                    Some(12) => Primaries::P3D65,
                    _ => Primaries::Bt709,
                };
                // PQ, HLG and linear light are not display-referred sRGB-like signals
                let sdr = !matches!(sources.iter().map(|s| s.transfer).find(|&t| t != 2), Some(8 | 16 | 18));
                if !sdr {
                    log::info!("HEIF: item {id}: HDR or linear transfer shown as coded (no tone mapping)");
                }
                (primaries, sdr)
            }
        };
        let gamut = (sdr && primaries != Primaries::Bt709).then(|| to_f32(&gamut_matrix(Gamut::from_primaries(primaries), Gamut::Bt709)));
        Conversion { matrix, full_range, gamut }
    }
}

fn check_pixels(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(invalid(format!("image size {w}x{h}")));
    }
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(unsupported(format!("a {w}x{h} image is larger than {MAX_PIXELS} pixels")));
    }
    Ok(())
}

/// Decode one coded item, refusing parameter sets that declare a picture larger than the item and
/// turning a decoder panic into an error.
fn decode_guarded(coded: Coded<'_>, data: &[u8], (w, h): (u32, u32), id: u32) -> Result<Planes> {
    let budget = (u64::from(w) + CODED_PADDING) * (u64::from(h) + CODED_PADDING);
    match std::panic::catch_unwind(AssertUnwindSafe(|| match coded {
        Coded::Hevc(c) => decode_hevc(c, data, budget),
        Coded::Av1(c) => decode_av1(c, data, budget),
    })) {
        Ok(r) => r.map_err(|e| with_item(id, e)),
        Err(_) => Err(CodecError::Decode(format!("HEIF: the decoder failed on item {id}"))),
    }
}

fn with_item(id: u32, e: CodecError) -> CodecError {
    match e {
        CodecError::Decode(s) => CodecError::Decode(format!("item {id}: {s}")),
        CodecError::Unsupported(s) => CodecError::Unsupported(format!("item {id}: {s}")),
        other => other,
    }
}

fn hevc_error(e: filmcraft_hevc::Error) -> CodecError {
    match e {
        filmcraft_hevc::Error::Unsupported(what) => CodecError::Unsupported(format!("HEVC: {what}")),
        e => CodecError::Decode(format!("HEVC: {e}")),
    }
}

fn decode_hevc(c: &HevcConfig, data: &[u8], budget: u64) -> Result<Planes> {
    // every SPS the decoder could activate, from the configuration and in the item's data
    let in_data = filmcraft_bitstream::length_prefixed_nals(data, usize::from(c.length_size)).unwrap_or_default();
    for nal in c.arrays.iter().flat_map(|a| a.nalus.iter().map(Vec::as_slice)).chain(in_data) {
        if nal.first().map(|h| (h >> 1) & 0x3F) == Some(33)
            && let Ok(sps) = filmcraft_hevc::params::Sps::parse(&filmcraft_bitstream::unescape_rbsp(nal.get(2..).unwrap_or(&[])))
            && u64::from(sps.width) * u64::from(sps.height) > budget
        {
            return Err(invalid(format!("an SPS declares {}x{} pictures, larger than the item", sps.width, sps.height)));
        }
    }
    let mut dec = filmcraft_hevc::Decoder::with_threads(1);
    dec.configure_hvcc(&c.to_bytes()).map_err(hevc_error)?;
    let mut pics = dec.decode(data, 0).map_err(hevc_error)?;
    pics.extend(dec.flush());
    if let Some(e) = dec.take_error() {
        return Err(hevc_error(e));
    }
    let p = pics.pop().ok_or_else(|| CodecError::Decode("HEVC: the item holds no picture".into()))?;
    let (w, h) = (p.width as usize, p.height as usize);
    let (cw, ch) = (p.chroma_width as usize, p.chroma_height as usize);
    if (cw, ch) != Chroma::C420.plane_size(w, h) {
        return Err(CodecError::Decode(format!("HEVC: {cw}x{ch} chroma planes for a {w}x{h} picture")));
    }
    let plane = |pl: &filmcraft_hevc::Plane, stride: usize, pw: usize, ph: usize| -> Result<Vec<u16>> {
        let mut out = Vec::with_capacity(pw * ph);
        for row in 0..ph {
            let start = row * stride;
            let ok = match pl {
                filmcraft_hevc::Plane::U8(v) => v.get(start..start + pw).map(|s| out.extend(s.iter().map(|&x| u16::from(x)))),
                filmcraft_hevc::Plane::U16(v) => v.get(start..start + pw).map(|s| out.extend_from_slice(s)),
            };
            ok.ok_or_else(|| CodecError::Decode("HEVC: picture plane shorter than its size".into()))?;
        }
        Ok(out)
    };
    let color = p.color;
    // The VUI is optional: unspecified code points with the range flag clear mean it says nothing
    // (HEVC infers limited range then, but HEIF readers default to full range).
    let unsignalled = color.primaries == 2 && color.transfer == 2 && color.matrix == 2 && !color.full_range;
    Ok(Planes {
        width: p.width,
        height: p.height,
        bits: u8::try_from(p.bit_depth).unwrap_or(16).clamp(8, 16),
        chroma: Chroma::C420,
        y: plane(&p.y, p.y_stride, w, h)?,
        u: plane(&p.u, p.uv_stride, cw, ch)?,
        v: plane(&p.v, p.uv_stride, cw, ch)?,
        signalled: Some(ColorCodes {
            primaries: color.primaries,
            transfer: color.transfer,
            matrix: color.matrix,
            full_range: (!unsignalled).then_some(color.full_range),
        }),
    })
}

fn decode_av1(c: &Av1Config, data: &[u8], budget: u64) -> Result<Planes> {
    let av1 = |e: filmcraft_av1::Error| match e {
        filmcraft_av1::Error::Unsupported(what) => CodecError::Unsupported(format!("AV1: {what}")),
        e => CodecError::Decode(format!("AV1: {e}")),
    };
    for (t, payload) in crate::hw_frame::obus(&c.config_obus).into_iter().chain(crate::hw_frame::obus(data)) {
        if t == 1
            && let Ok(seq) = filmcraft_av1::SequenceHeader::parse(payload)
            && u64::from(seq.max_frame_width) * u64::from(seq.max_frame_height) > budget
        {
            return Err(invalid(format!("a sequence header declares {}x{} frames, larger than the item", seq.max_frame_width, seq.max_frame_height)));
        }
    }
    let mut dec = filmcraft_av1::Decoder::with_threads(1);
    let mut pics = Vec::new();
    if !c.config_obus.is_empty() {
        pics.extend(dec.decode(&c.config_obus).map_err(av1)?);
    }
    pics.extend(dec.decode(data).map_err(av1)?);
    pics.extend(dec.flush_result().map_err(av1)?);
    // a layered image shows its best layer last
    let p = pics.pop().ok_or_else(|| CodecError::Decode("AV1: the item holds no frame".into()))?;
    let chroma = match (p.mono_chrome, p.subsampling_x, p.subsampling_y) {
        (true, _, _) => Chroma::Mono,
        (false, 1, 1) => Chroma::C420,
        (false, 1, 0) => Chroma::C422,
        (false, 0, 0) => Chroma::C444,
        (false, x, y) => return Err(CodecError::Unsupported(format!("AV1: chroma subsampling {x}/{y}"))),
    };
    let [y, u, v] = p.planes;
    let (u, v) = if chroma == Chroma::Mono { (Vec::new(), Vec::new()) } else { (u, v) };
    let planes = Planes {
        width: p.width,
        height: p.height,
        bits: p.bit_depth.clamp(8, 16),
        chroma,
        y,
        u,
        v,
        signalled: Some(ColorCodes {
            primaries: p.color_primaries,
            transfer: p.transfer_characteristics,
            matrix: p.matrix_coefficients,
            full_range: Some(p.full_range),
        }),
    };
    if !planes.consistent() {
        return Err(CodecError::Decode("AV1: frame planes do not fit the frame size".into()));
    }
    Ok(planes)
}

/// Place decoded tiles row by row on a canvas the grid's output size (tiles past the right or
/// bottom edge are cut).
fn assemble(tiles: &[Planes], g: ImageGrid, (tw, th): (u32, u32)) -> Result<Planes> {
    let first = tiles.first().ok_or_else(|| invalid("grid without tiles".into()))?;
    let (ow, oh) = (g.output_width as usize, g.output_height as usize);
    let (tw, th) = (tw as usize, th as usize);
    let chroma = first.chroma;
    let (sx, sy) = chroma.shifts();
    let (ocw, och) = chroma.plane_size(ow, oh);
    let (tcw, tch) = chroma.plane_size(tw, th);
    let mut out = Planes {
        width: g.output_width,
        height: g.output_height,
        bits: first.bits,
        chroma,
        y: vec![0; ow * oh],
        u: vec![0; ocw * och],
        v: vec![0; ocw * och],
        signalled: first.signalled,
    };
    let copy = |dst: &mut [u16], dst_w: usize, dst_h: usize, src: &[u16], src_w: usize, src_h: usize, x0: usize, y0: usize| {
        if x0 >= dst_w || y0 >= dst_h {
            return;
        }
        let n = src_w.min(dst_w - x0);
        for row in 0..src_h.min(dst_h - y0) {
            let (d, s) = ((y0 + row) * dst_w + x0, row * src_w);
            if let (Some(d), Some(s)) = (dst.get_mut(d..d + n), src.get(s..s + n)) {
                d.copy_from_slice(s);
            }
        }
    };
    let columns = g.columns as usize;
    for (k, t) in tiles.iter().enumerate() {
        if !t.consistent() {
            return Err(CodecError::Decode("HEIF: tile planes do not fit the tile size".into()));
        }
        let (x0, y0) = ((k % columns) * tw, (k / columns) * th);
        copy(&mut out.y, ow, oh, &t.y, tw, th, x0, y0);
        if chroma != Chroma::Mono {
            copy(&mut out.u, ocw, och, &t.u, tcw, tch, x0 >> sx, y0 >> sy);
            copy(&mut out.v, ocw, och, &t.v, tcw, tch, x0 >> sx, y0 >> sy);
        }
    }
    Ok(out)
}

/// Y'CbCr matrix coefficients.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MatrixKind {
    /// ISO/IEC 23091-2 code 0: the planes hold G, B, R.
    Identity,
    Ycc {
        kr: f32,
        kb: f32,
    },
}

impl MatrixKind {
    fn from_code(c: u8) -> Option<MatrixKind> {
        match c {
            0 => Some(MatrixKind::Identity),
            1 => Some(MatrixKind::Ycc { kr: 0.2126, kb: 0.0722 }),
            5 | 6 => Some(MatrixKind::Ycc { kr: 0.299, kb: 0.114 }),
            9 | 10 => Some(MatrixKind::Ycc { kr: 0.2627, kb: 0.0593 }),
            _ => None,
        }
    }
}

/// How decoded samples become sRGB RGBA8.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Conversion {
    matrix: MatrixKind,
    full_range: bool,
    /// Linear-light primaries conversion to BT.709 (for P3 / BT.2020 pictures).
    gamut: Option<[[f32; 3]; 3]>,
}

/// An alpha plane: the luma of an auxiliary image.
struct Alpha<'a> {
    planes: &'a Planes,
    full_range: bool,
    premultiplied: bool,
}

/// Normalised value of every u16 code: luma (0..1) or, with `chroma`, centred colour difference.
fn sample_table(bits: u8, full_range: bool, chroma: bool) -> Vec<f32> {
    let bits = u32::from(bits.clamp(8, 16));
    let scale = (1u32 << (bits - 8)) as f32;
    let max = ((1u64 << bits) - 1) as f32;
    (0..=u16::MAX)
        .map(|v| {
            let v = f32::from(v);
            match (full_range, chroma) {
                (true, false) => v / max,
                (true, true) => (v - (1u32 << (bits - 1)) as f32) / max,
                (false, false) => (v - 16.0 * scale) / (219.0 * scale),
                (false, true) => (v - 128.0 * scale) / (224.0 * scale),
            }
        })
        .collect()
}

/// sRGB-encoded value (0..1, 16384 steps) → linear.
fn eotf_table() -> &'static [f32] {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(|| (0..16384).map(|i| filmcraft_color::srgb_to_linear(i as f32 / 16383.0)).collect())
}

/// Linear value (0..1, 65536 steps) → sRGB-encoded 8-bit code.
fn oetf_table() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(|| (0..65536).map(|i| (filmcraft_color::linear_to_srgb(i as f32 / 65535.0) * 255.0 + 0.5).clamp(0.0, 255.0) as u8).collect())
}

/// Colour-difference samples of luma row `row`, `lut`-normalised: each chroma sample is repeated
/// over the luma samples it covers (what libheif does, and FilmCraft's own video frames).
fn chroma_row(plane: &[u16], chroma: Chroma, w: usize, h: usize, row: usize, lut: &[f32], out: &mut [f32]) {
    let (cw, ch) = chroma.plane_size(w, h);
    let (sx, sy) = chroma.shifts();
    let start = (row >> sy).min(ch.saturating_sub(1)) * cw;
    let Some(src) = plane.get(start..start + cw).filter(|s| !s.is_empty()) else {
        out.fill(0.0);
        return;
    };
    for (x, o) in out.iter_mut().take(w).enumerate() {
        *o = src.get(x >> sx).and_then(|&v| lut.get(usize::from(v))).copied().unwrap_or(0.0);
    }
}

/// Convert decoded planes (and an alpha plane) to straight-alpha sRGB RGBA8.
fn to_rgba(p: &Planes, conv: &Conversion, alpha: Option<Alpha<'_>>) -> Result<Rgba> {
    if !p.consistent() || p.width == 0 || p.height == 0 {
        return Err(CodecError::Decode("HEIF: picture planes do not fit the picture size".into()));
    }
    let (w, h) = (p.width as usize, p.height as usize);
    let identity = conv.matrix == MatrixKind::Identity;
    let y_lut = sample_table(p.bits, conv.full_range, false);
    // the identity matrix codes R and B like G (no offset)
    let c_lut = if identity { y_lut.clone() } else { sample_table(p.bits, conv.full_range, true) };
    let (kr, kb) = match conv.matrix {
        MatrixKind::Ycc { kr, kb } => (kr, kb),
        MatrixKind::Identity => (0.0, 0.0),
    };
    let kg = 1.0 - kr - kb;
    let (crr, cbb) = (2.0 * (1.0 - kr), 2.0 * (1.0 - kb));
    let (cgr, cgb) = if kg > 0.0 { (crr * kr / kg, cbb * kb / kg) } else { (0.0, 0.0) };
    let a_lut = alpha.as_ref().map(|a| sample_table(a.planes.bits, a.full_range, false));
    let (eotf, oetf) = (eotf_table(), oetf_table());
    let encode = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let mut px = vec![0u8; w.checked_mul(h).and_then(|n| n.checked_mul(4)).ok_or_else(|| unsupported("image too large".into()))?];
    px.par_chunks_mut(w * 4).enumerate().for_each(|(row, out)| {
        let mut cb = vec![0f32; w];
        let mut cr = vec![0f32; w];
        if p.chroma != Chroma::Mono {
            chroma_row(&p.u, p.chroma, w, h, row, &c_lut, &mut cb);
            chroma_row(&p.v, p.chroma, w, h, row, &c_lut, &mut cr);
        }
        let Some(ys) = p.y.get(row * w..row * w + w) else { return };
        // the alpha plane, nearest sample when its size differs
        let arow: Option<(Vec<f32>, bool)> = alpha.as_ref().zip(a_lut.as_ref()).map(|(a, lut)| {
            let (aw, ah) = (a.planes.width as usize, a.planes.height as usize);
            let ar = (row * ah / h.max(1)).min(ah.saturating_sub(1));
            let vals = (0..w)
                .map(|x| {
                    let ax = (x * aw / w.max(1)).min(aw.saturating_sub(1));
                    a.planes.y.get(ar * aw + ax).map_or(1.0, |&v| lut.get(usize::from(v)).copied().unwrap_or(1.0).clamp(0.0, 1.0))
                })
                .collect();
            (vals, a.premultiplied)
        });
        for (x, (o, &yv)) in out.as_chunks_mut::<4>().0.iter_mut().zip(ys).enumerate() {
            let yn = y_lut.get(usize::from(yv)).copied().unwrap_or(0.0);
            let (u, v) = (cb.get(x).copied().unwrap_or(0.0), cr.get(x).copied().unwrap_or(0.0));
            let (mut r, mut g, mut b) = if identity {
                (v, yn, u)
            } else if p.chroma == Chroma::Mono {
                (yn, yn, yn)
            } else {
                (yn + crr * v, yn - cgr * v - cgb * u, yn + cbb * u)
            };
            let mut a = 1.0;
            if let Some((vals, premultiplied)) = &arow {
                a = vals.get(x).copied().unwrap_or(1.0);
                if *premultiplied {
                    if a > 0.0 {
                        (r, g, b) = (r / a, g / a, b / a);
                    } else {
                        (r, g, b) = (0.0, 0.0, 0.0);
                    }
                }
            }
            let rgb = match &conv.gamut {
                Some(m) => {
                    let lin = |v: f32| eotf.get((v.clamp(0.0, 1.0) * 16383.0 + 0.5) as usize).copied().unwrap_or(0.0);
                    let (lr, lg, lb) = (lin(r), lin(g), lin(b));
                    let enc = |v: f32| oetf.get((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize).copied().unwrap_or(0);
                    [
                        enc(m[0][0] * lr + m[0][1] * lg + m[0][2] * lb),
                        enc(m[1][0] * lr + m[1][1] * lg + m[1][2] * lb),
                        enc(m[2][0] * lr + m[2][1] * lg + m[2][2] * lb),
                    ]
                }
                None => [encode(r), encode(g), encode(b)],
            };
            *o = [rgb[0], rgb[1], rgb[2], encode(a)];
        }
    });
    Ok(Rgba { w: p.width, h: p.height, px })
}

/// Clean aperture, then rotation, then mirror.
fn apply_transforms(img: Rgba, t: Transforms) -> Rgba {
    let mut img = img;
    if let Some((x, y, w, h)) = t.clap.and_then(|c| clap_rect(c, img.w, img.h)) {
        img = crop(&img, x, y, w, h);
    }
    if t.rotation != 0 {
        img = rotate(&img, t.rotation);
    }
    if let Some(m) = t.mirror {
        img = mirror(&img, m);
    }
    img
}

fn pixel(img: &Rgba, x: usize, y: usize) -> [u8; 4] {
    let i = (y * img.w as usize + x) * 4;
    img.px.get(i..i + 4).and_then(|p| p.try_into().ok()).unwrap_or([0; 4])
}

/// A new image of `w` × `h` whose pixel (x, y) is `src(x, y)` of `img`.
fn remap(img: &Rgba, w: u32, h: u32, src: impl Fn(usize, usize) -> (usize, usize) + Sync) -> Rgba {
    let wu = w as usize;
    let mut px = vec![0u8; wu * h as usize * 4];
    px.par_chunks_mut((wu * 4).max(1)).enumerate().for_each(|(y, row)| {
        for (x, o) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (sx, sy) = src(x, y);
            *o = pixel(img, sx, sy);
        }
    });
    Rgba { w, h, px }
}

fn crop(img: &Rgba, x0: u32, y0: u32, w: u32, h: u32) -> Rgba {
    remap(img, w, h, |x, y| (x + x0 as usize, y + y0 as usize))
}

/// Rotate anti-clockwise by `quarter_turns` × 90°.
fn rotate(img: &Rgba, quarter_turns: u8) -> Rgba {
    let (w, h) = (img.w as usize, img.h as usize);
    match quarter_turns & 3 {
        // the top-right corner comes to the top left
        1 => remap(img, img.h, img.w, |x, y| (w - 1 - y, x)),
        2 => remap(img, img.w, img.h, |x, y| (w - 1 - x, h - 1 - y)),
        // the top-left corner goes to the top right (90° clockwise)
        3 => remap(img, img.h, img.w, |x, y| (y, h - 1 - x)),
        _ => remap(img, img.w, img.h, |x, y| (x, y)),
    }
}

fn mirror(img: &Rgba, m: Mirror) -> Rgba {
    let (w, h) = (img.w as usize, img.h as usize);
    match m {
        Mirror::LeftRight => remap(img, img.w, img.h, |x, y| (w - 1 - x, y)),
        Mirror::TopBottom => remap(img, img.w, img.h, |x, y| (x, h - 1 - y)),
    }
}

#[cfg(test)]
#[path = "heif_tests.rs"]
mod tests;
