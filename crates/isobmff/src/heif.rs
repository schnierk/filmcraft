//! HEIF / AVIF still images: the `meta` box of an image file, written from ISO/IEC 23008-12 (HEIF,
//! 2nd edition 2022), ISO/IEC 14496-12 (ISOBMFF, 7th edition 2022: `meta`, `iloc`, `iinf`, `iref`,
//! `idat`, `pitm`), ISO/IEC 23000-22 (MIAF) and the AV1 Image File Format (AVIF) v1.1.0.
//!
//! [`parse`] reads the top-level box headers, `ftyp` and the file-level `meta` box (never `mdat`)
//! from a [`ByteSource`] and returns a [`HeifFile`]:
//!
//! - the handler (`hdlr`, `pict` for images) and the primary item (`pitm` v0 / v1);
//! - item infos (`iinf` v0 / v1 with `infe` v0–v3: id, type, name, MIME type, hidden flag,
//!   protection);
//! - item locations (`iloc` v0–v2: offset, length, base-offset and extent-index sizes of 0–8 bytes,
//!   construction method 0 = file and 1 = `idat`, any number of extents);
//! - item references (`iref` v0 / v1: `dimg`, `thmb`, `auxl`, `cdsc`, `prem`… with 16- or 32-bit ids);
//! - item properties (`iprp`: `ipco` in order, `ipma` v0 / v1 with 7- or 15-bit property indices and
//!   essential flags). [`Property`] decodes `hvcC`, `av1C`, `ispe`, `irot`, `imir`, `clap`, `colr`
//!   (`nclx`, `prof` / `rICC`), `pixi` and `auxC`; any other box is kept as [`Property::Other`];
//! - the `idat` payload.
//!
//! [`HeifFile::read_item`] assembles an item's bytes from its extents, [`ImageGrid`] parses a `grid`
//! derived image item and [`exif_tiff`] finds the TIFF header inside an `Exif` item. Decoding the
//! coded images (HEVC, AV1) and applying the transforms is `filmcraft-codecs`' job (`heif`).
//!
//! Every count and size comes from the file and is treated as hostile: counts are checked against
//! the bytes of their box before anything is allocated, the `meta` box is capped at
//! [`MAX_META_BYTES`], items at [`MAX_ITEMS`] and extents at [`MAX_EXTENTS`].

use std::collections::HashMap;

use crate::bytes::{Cur, FourCc, boxes, parse_box_header};
use crate::codec::{Av1Config, CleanAperture, ColorInfo, HevcConfig};
use crate::error::{Error, Result};
use crate::source::ByteSource;

/// Largest `meta` box read (iPhone photos have about 40 KB).
pub const MAX_META_BYTES: u64 = 64 << 20;
/// Most items a file may describe (a 256 × 256 grid has 65 536 tiles).
pub const MAX_ITEMS: usize = 1 << 18;
/// Most extents all item locations may have together.
pub const MAX_EXTENTS: usize = 1 << 20;
/// Most top-level boxes looked at before giving up on finding `meta`.
const MAX_TOP_LEVEL_BOXES: usize = 1 << 16;

/// `ftyp` brands of HEIF and AVIF files: HEVC images and image collections (`heic`, `heix`, `heim`,
/// `heis`), HEVC image sequences (`hevc`, `hevx`), the generic image and sequence brands (`mif1`,
/// `mif2`, `msf1`) and AVIF (`avif`, `avis`).
pub const BRANDS: [&[u8; 4]; 11] = [b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"mif2", b"msf1", b"avif", b"avis"];

/// The `ftyp` box at the start of `head` (major brand, compatible brands), if there is one.
pub fn file_type(head: &[u8]) -> Option<(FourCc, Vec<FourCc>)> {
    let h = parse_box_header(head).ok()?;
    if h.kind != b"ftyp" {
        return None;
    }
    let end = usize::try_from(h.size.unwrap_or(head.len() as u64)).ok()?.min(head.len());
    let body = head.get(usize::try_from(h.header_len).ok()?..end)?;
    let major = FourCc(body.get(0..4)?.try_into().ok()?);
    let compatible = body.get(8..).unwrap_or(&[]).as_chunks::<4>().0.iter().map(|b| FourCc(*b)).collect();
    Some((major, compatible))
}

/// Whether `head` (the first bytes of a file) starts with an `ftyp` box naming a HEIF or AVIF
/// brand ([`BRANDS`]) as its major or a compatible brand.
pub fn sniff(head: &[u8]) -> bool {
    file_type(head).is_some_and(|(major, compatible)| std::iter::once(major).chain(compatible).any(|b| BRANDS.iter().any(|x| b == **x)))
}

/// Mirroring (`imir`), applied after rotation. ISO/IEC 23008-12:2022 §6.5.12 names the field
/// `mode` and says which parts trade places; the 2017 edition's "vertical (0) or horizontal (1)
/// axis" wording was read both ways, and writers and libheif follow the 2022 text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mirror {
    /// `mode` 0: the top and bottom parts are exchanged (a vertical flip).
    TopBottom,
    /// `mode` 1: the left and right parts are exchanged (a horizontal flip).
    LeftRight,
}

/// One item property (an `ipco` child).
#[derive(Clone, Debug, PartialEq)]
pub enum Property {
    /// `hvcC`: HEVC decoder configuration.
    Hevc(HevcConfig),
    /// `av1C`: AV1 codec configuration.
    Av1(Av1Config),
    /// `ispe`: the size of the reconstructed image, before any transformative property.
    ImageSize { width: u32, height: u32 },
    /// `irot`: anti-clockwise rotation in quarter turns (0–3).
    Rotation(u8),
    /// `imir`.
    Mirror(Mirror),
    /// `clap`: clean aperture (crop).
    CleanAperture(CleanAperture),
    /// `colr`: `nclx` code points or an ICC profile (`prof`, `rICC`).
    Color(ColorInfo),
    /// `pixi`: bits per channel.
    PixelInfo(Vec<u8>),
    /// `auxC`: the type of an auxiliary image (a URN such as
    /// `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha`) and its subtype bytes.
    Auxiliary { urn: String, subtype: Vec<u8> },
    /// Any other property box, or one of the above that did not parse (`malformed`).
    Other { kind: FourCc, malformed: bool },
}

impl Property {
    /// The box type.
    pub fn kind(&self) -> FourCc {
        FourCc(*match self {
            Property::Hevc(_) => b"hvcC",
            Property::Av1(_) => b"av1C",
            Property::ImageSize { .. } => b"ispe",
            Property::Rotation(_) => b"irot",
            Property::Mirror(_) => b"imir",
            Property::CleanAperture(_) => b"clap",
            Property::Color(_) => b"colr",
            Property::PixelInfo(_) => b"pixi",
            Property::Auxiliary { .. } => b"auxC",
            Property::Other { kind, .. } => return *kind,
        })
    }

    fn parse(kind: FourCc, p: &[u8]) -> Property {
        let parsed = match &kind.0 {
            b"hvcC" => HevcConfig::parse(p).map(Property::Hevc),
            b"av1C" => Av1Config::parse(p).map(Property::Av1),
            b"ispe" => parse_ispe(p),
            b"irot" => Cur::new(p, "irot").u8().map(|b| Property::Rotation(b & 3)),
            b"imir" => Cur::new(p, "imir").u8().map(|b| Property::Mirror(if b & 1 == 0 { Mirror::TopBottom } else { Mirror::LeftRight })),
            b"clap" => parse_clap(p),
            b"colr" => crate::codec::parse_colr(p).map(Property::Color),
            b"pixi" => parse_pixi(p),
            b"auxC" => parse_auxc(p),
            _ => return Property::Other { kind, malformed: false },
        };
        parsed.unwrap_or(Property::Other { kind, malformed: true })
    }
}

fn parse_ispe(p: &[u8]) -> Result<Property> {
    let mut c = Cur::new(p, "ispe");
    let (v, _) = c.full_header()?;
    if v != 0 {
        return Err(Error::Unsupported(format!("ispe version {v}")));
    }
    Ok(Property::ImageSize { width: c.u32()?, height: c.u32()? })
}

fn parse_clap(p: &[u8]) -> Result<Property> {
    let mut c = Cur::new(p, "clap");
    Ok(Property::CleanAperture(CleanAperture {
        width: (c.u32()?, c.u32()?),
        height: (c.u32()?, c.u32()?),
        horiz_offset: (c.i32()?, c.u32()?),
        vert_offset: (c.i32()?, c.u32()?),
    }))
}

fn parse_pixi(p: &[u8]) -> Result<Property> {
    let mut c = Cur::new(p, "pixi");
    c.full_header()?;
    let n = c.u8()? as usize;
    Ok(Property::PixelInfo(c.bytes(n)?.to_vec()))
}

fn parse_auxc(p: &[u8]) -> Result<Property> {
    let mut c = Cur::new(p, "auxC");
    c.full_header()?;
    let (urn, rest) = c_string(c.rest());
    Ok(Property::Auxiliary { urn, subtype: rest.to_vec() })
}

/// A null-terminated UTF-8 string (lossy; without a terminator it runs to the end) and the bytes
/// after it.
fn c_string(b: &[u8]) -> (String, &[u8]) {
    match b.iter().position(|&x| x == 0) {
        Some(i) => (String::from_utf8_lossy(b.get(..i).unwrap_or(&[])).into_owned(), b.get(i + 1..).unwrap_or(&[])),
        None => (String::from_utf8_lossy(b).into_owned(), &[]),
    }
}

/// An `ipma` association of an item with a property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyLink {
    /// Index into [`HeifFile::properties`] (0-based: the file's 1-based index minus one). It may
    /// point past the end in a damaged file; [`HeifFile::item_properties`] then yields `None`.
    pub index: usize,
    /// A reader must not display the item if it does not understand this property.
    pub essential: bool,
}

/// One extent of an item location (`iloc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Extent {
    /// `extent_index` (construction method 2 only; 0 otherwise).
    pub index: u64,
    pub offset: u64,
    /// 0: up to the end of the file or `idat`.
    pub length: u64,
}

/// Where an item's bytes are (`iloc`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ItemLocation {
    /// 0: file offsets, 1: offsets into `idat`, 2: offsets into other items.
    pub construction_method: u8,
    /// 0: this file.
    pub data_reference_index: u16,
    pub base_offset: u64,
    pub extents: Vec<Extent>,
}

/// One item (`infe`, with its location and property associations).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: u32,
    /// `item_type`: `hvc1`, `av01`, `grid`, `iden`, `iovl`, `tmap`, `Exif`, `mime`, `uri `…;
    /// all zero bytes for `infe` v0 / v1, which have none.
    pub kind: FourCc,
    pub name: String,
    /// The MIME type of a `mime` item (`application/rdf+xml` for XMP).
    pub content_type: String,
    /// `infe` flag 1: not meant to be displayed on its own (tiles, thumbnails, auxiliary images).
    pub hidden: bool,
    /// Non-zero for a protected (encrypted) item.
    pub protection_index: u16,
    /// `None` when `iloc` does not list the item.
    pub location: Option<ItemLocation>,
    /// `ipma` associations in file order.
    pub properties: Vec<PropertyLink>,
}

/// One `iref` entry: `from` refers to `to` (in order) with references of type `kind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemReference {
    /// `dimg` (derived image inputs), `thmb`, `auxl`, `cdsc` (content description), `prem`, `base`…
    pub kind: FourCc,
    pub from: u32,
    pub to: Vec<u32>,
}

/// The `meta` box of a HEIF / AVIF file. See the [module documentation](self).
#[derive(Clone, Debug, Default)]
pub struct HeifFile {
    pub major_brand: FourCc,
    pub minor_version: u32,
    pub compatible_brands: Vec<FourCc>,
    /// The `meta` handler: `pict` for images.
    pub handler: FourCc,
    /// `pitm`.
    pub primary_item: Option<u32>,
    /// Items in `iinf` order.
    pub items: Vec<Item>,
    /// `ipco` children in order (what [`PropertyLink::index`] points into).
    pub properties: Vec<Property>,
    pub references: Vec<ItemReference>,
    /// The `idat` payload (construction method 1).
    pub idat: Vec<u8>,
    /// The file also has a `moov` box (an image sequence, or a movie).
    pub has_movie: bool,
    /// The file has a top-level `meta` box (without one, every field above but the brands is empty).
    pub has_meta: bool,
    by_id: HashMap<u32, usize>,
}

impl HeifFile {
    /// The item with id `id`.
    pub fn item(&self, id: u32) -> Option<&Item> {
        self.by_id.get(&id).and_then(|&i| self.items.get(i))
    }

    /// The properties associated with item `id` in `ipma` order, with their essential flag
    /// (`None` for a link to a property that does not exist).
    pub fn item_properties(&self, id: u32) -> impl Iterator<Item = (Option<&Property>, bool)> + '_ {
        self.item(id).into_iter().flat_map(move |it| it.properties.iter().map(move |l| (self.properties.get(l.index), l.essential)))
    }

    /// The first property of item `id` that `f` picks.
    pub fn find_property<'a, T>(&'a self, id: u32, f: impl Fn(&'a Property) -> Option<T>) -> Option<T> {
        self.item_properties(id).find_map(|(p, _)| p.and_then(&f))
    }

    /// The items `from` refers to with references of type `kind`, in order.
    pub fn references_from(&self, from: u32, kind: &[u8; 4]) -> Vec<u32> {
        self.references.iter().filter(|r| r.from == from && r.kind == kind).flat_map(|r| r.to.iter().copied()).collect()
    }

    /// The items that refer to `to` with a reference of type `kind`, in `iref` order.
    pub fn references_to(&self, to: u32, kind: &[u8; 4]) -> Vec<u32> {
        let mut v: Vec<u32> = self.references.iter().filter(|r| r.kind == kind && r.to.contains(&to)).map(|r| r.from).collect();
        v.dedup();
        v
    }

    /// The number of bytes of item `id` in a file of `file_len` bytes.
    pub fn item_len(&self, id: u32, file_len: u64) -> Result<u64> {
        let (loc, space) = self.location(id, file_len)?;
        let mut total = 0u64;
        for e in &loc.extents {
            let (_, len) = extent_range(loc.base_offset, e, space)?;
            total = total.checked_add(len).ok_or(Error::TooLarge("item data"))?;
        }
        Ok(total)
    }

    /// The bytes of item `id`, assembled from its extents (at most `max_len` bytes, else
    /// [`Error::TooLarge`]).
    pub fn read_item<S: ByteSource + ?Sized>(&self, src: &S, id: u32, max_len: u64) -> Result<Vec<u8>> {
        let total = self.item_len(id, src.len())?;
        if total > max_len {
            return Err(Error::TooLarge("item data"));
        }
        let (loc, space) = self.location(id, src.len())?;
        let mut out = Vec::with_capacity(usize::try_from(total).map_err(|_| Error::TooLarge("item data"))?);
        for e in &loc.extents {
            let (start, len) = extent_range(loc.base_offset, e, space)?;
            let n = usize::try_from(len).map_err(|_| Error::TooLarge("item data"))?;
            if loc.construction_method == 1 {
                let s = usize::try_from(start).map_err(|_| Error::Truncated("idat"))?;
                let bytes = self.idat.get(s..s.checked_add(n).ok_or(Error::Truncated("idat"))?).ok_or(Error::Truncated("idat"))?;
                out.extend_from_slice(bytes);
            } else {
                let at = out.len();
                out.resize(at.checked_add(n).ok_or(Error::TooLarge("item data"))?, 0);
                let buf = out.get_mut(at..).ok_or(Error::Truncated("item data"))?;
                src.read_at(start, buf)?;
            }
        }
        Ok(out)
    }

    /// Item `id`'s location and the size of the space its offsets point into.
    fn location(&self, id: u32, file_len: u64) -> Result<(&ItemLocation, u64)> {
        let item = self.item(id).ok_or_else(|| Error::Invalid(format!("no item {id}")))?;
        let loc = item.location.as_ref().ok_or_else(|| Error::Invalid(format!("item {id} has no location")))?;
        if loc.data_reference_index != 0 {
            return Err(Error::Unsupported(format!("item {id} is in another file")));
        }
        match loc.construction_method {
            0 => Ok((loc, file_len)),
            1 => Ok((loc, self.idat.len() as u64)),
            m => Err(Error::Unsupported(format!("item {id}: construction method {m}"))),
        }
    }
}

/// (start, length) of an extent inside a space of `space` bytes.
fn extent_range(base: u64, e: &Extent, space: u64) -> Result<(u64, u64)> {
    let start = base.checked_add(e.offset).ok_or(Error::Truncated("item extent"))?;
    if start > space {
        return Err(Error::Truncated("item extent"));
    }
    let len = if e.length == 0 { space - start } else { e.length };
    match start.checked_add(len) {
        Some(end) if end <= space => Ok((start, len)),
        _ => Err(Error::Truncated("item extent")),
    }
}

/// A `grid` derived image item: the input images (its `dimg` references, row by row) are placed on
/// a `columns` × `rows` canvas and cropped to the output size from the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageGrid {
    pub rows: u32,
    pub columns: u32,
    pub output_width: u32,
    pub output_height: u32,
}

impl ImageGrid {
    /// Parse the item's data (ISO/IEC 23008-12 §6.6.2.3).
    pub fn parse(data: &[u8]) -> Result<ImageGrid> {
        let mut c = Cur::new(data, "grid");
        let version = c.u8()?;
        if version != 0 {
            return Err(Error::Unsupported(format!("grid version {version}")));
        }
        let flags = c.u8()?;
        let rows = u32::from(c.u8()?) + 1;
        let columns = u32::from(c.u8()?) + 1;
        let (output_width, output_height) = if flags & 1 != 0 { (c.u32()?, c.u32()?) } else { (u32::from(c.u16()?), u32::from(c.u16()?)) };
        if output_width == 0 || output_height == 0 {
            return Err(Error::Invalid(format!("grid output size {output_width}x{output_height}")));
        }
        Ok(ImageGrid { rows, columns, output_width, output_height })
    }

    /// Number of cells (`rows` × `columns`).
    pub fn cells(&self) -> usize {
        self.rows as usize * self.columns as usize
    }
}

/// The TIFF header and what follows it inside the data of an `Exif` item (ISO/IEC 23008-12 Annex A:
/// a 32-bit offset from after itself to the TIFF header, usually 6 for an `Exif\0\0` prefix). Also
/// accepts the TIFF header right after the offset field, or after an `Exif\0\0` the offset does not
/// count. `None` when no TIFF header is where it should be.
pub fn exif_tiff(data: &[u8]) -> Option<&[u8]> {
    let is_tiff = |b: &[u8]| b.starts_with(b"II*\0") || b.starts_with(b"MM\0*");
    let offset = usize::try_from(u32::from_be_bytes(data.get(0..4)?.try_into().ok()?)).ok()?;
    let payload = data.get(4..)?;
    if let Some(t) = payload.get(offset..).filter(|t| is_tiff(t)) {
        return Some(t);
    }
    let payload = payload.strip_prefix(b"Exif\0\0").unwrap_or(payload);
    is_tiff(payload).then_some(payload)
}

/// Read a big-endian unsigned field of `n` bytes (0–8; 0 reads as 0).
fn uint(c: &mut Cur, n: u8) -> Result<u64> {
    let b = c.bytes(n as usize)?;
    Ok(b.iter().fold(0u64, |v, &x| (v << 8) | u64::from(x)))
}

/// Parse a HEIF / AVIF file: top-level box headers, `ftyp` and the first top-level `meta` box.
/// A file without `meta` parses too (with [`HeifFile::has_meta`] false), so a caller can tell an
/// image sequence (only `moov`) from a damaged image.
pub fn parse<S: ByteSource + ?Sized>(src: &S) -> Result<HeifFile> {
    let len = src.len();
    let mut file = HeifFile::default();
    let mut offset = 0u64;
    let mut seen_ftyp = false;
    for _ in 0..MAX_TOP_LEVEL_BOXES {
        if offset.saturating_add(8) > len {
            return Ok(file);
        }
        let n = (len - offset).min(32) as usize;
        let mut hb = [0u8; 32];
        let head = hb.get_mut(..n).ok_or(Error::Truncated("box header"))?;
        src.read_at(offset, head)?;
        let h = parse_box_header(head)?;
        let size = h.size.unwrap_or(len - offset);
        let body_start = offset.checked_add(h.header_len).ok_or(Error::Truncated("box"))?;
        let end = offset.saturating_add(size);
        if end > len {
            return Err(Error::Truncated("top-level box"));
        }
        let body = |max: u64, what: &'static str| -> Result<Vec<u8>> {
            let n = end - body_start;
            if n > max {
                return Err(Error::TooLarge(what));
            }
            let mut v = vec![0u8; usize::try_from(n).map_err(|_| Error::TooLarge(what))?];
            src.read_at(body_start, &mut v)?;
            Ok(v)
        };
        match &h.kind.0 {
            b"ftyp" if !seen_ftyp => {
                seen_ftyp = true;
                let b = body(1 << 16, "ftyp")?;
                let mut c = Cur::new(&b, "ftyp");
                file.major_brand = c.fourcc()?;
                file.minor_version = c.u32().unwrap_or(0);
                while c.remaining() >= 4 {
                    file.compatible_brands.push(c.fourcc()?);
                }
            }
            b"moov" => file.has_movie = true,
            b"meta" => {
                parse_meta(&body(MAX_META_BYTES, "meta")?, &mut file)?;
                return Ok(file);
            }
            _ => {}
        }
        if h.size.is_none() {
            return Ok(file);
        }
        offset = end;
    }
    Err(Error::TooLarge("top-level boxes"))
}

fn parse_meta(payload: &[u8], file: &mut HeifFile) -> Result<()> {
    file.has_meta = true;
    // ISO `meta` is a full box; QuickTime's is not (its first child starts right away).
    let children = if payload.get(4..8) == Some(&b"hdlr"[..]) {
        payload
    } else {
        let mut c = Cur::new(payload, "meta");
        let (v, _) = c.full_header()?;
        if v != 0 {
            return Err(Error::Unsupported(format!("meta version {v}")));
        }
        c.rest()
    };
    let mut locations: Vec<(u32, ItemLocation)> = Vec::new();
    let mut links: Vec<(u32, Vec<PropertyLink>)> = Vec::new();
    let (mut seen_iinf, mut seen_iloc, mut seen_ipco) = (false, false, false);
    for b in boxes(children) {
        let b = b?;
        let p = b.payload;
        match &b.kind.0 {
            b"hdlr" if file.handler == FourCc::default() => {
                let mut c = Cur::new(p, "hdlr");
                c.full_header()?;
                c.skip(4)?;
                file.handler = c.fourcc()?;
            }
            b"pitm" if file.primary_item.is_none() => {
                let mut c = Cur::new(p, "pitm");
                let (v, _) = c.full_header()?;
                file.primary_item = Some(if v == 0 { u32::from(c.u16()?) } else { c.u32()? });
            }
            b"iinf" if !seen_iinf => {
                seen_iinf = true;
                file.items = parse_iinf(p)?;
            }
            b"iloc" if !seen_iloc => {
                seen_iloc = true;
                locations = parse_iloc(p)?;
            }
            b"iref" => parse_iref(p, &mut file.references)?,
            b"iprp" => {
                for c in boxes(p) {
                    let c = c?;
                    match &c.kind.0 {
                        b"ipco" if !seen_ipco => {
                            seen_ipco = true;
                            for prop in boxes(c.payload) {
                                let prop = prop?;
                                file.properties.push(Property::parse(prop.kind, prop.payload));
                            }
                        }
                        b"ipma" => parse_ipma(c.payload, &mut links)?,
                        _ => {}
                    }
                }
            }
            b"idat" if file.idat.is_empty() => file.idat = p.to_vec(),
            _ => {}
        }
    }
    file.by_id = HashMap::with_capacity(file.items.len());
    for (i, it) in file.items.iter().enumerate() {
        if file.by_id.insert(it.id, i).is_some() {
            return Err(Error::Invalid(format!("item {} described twice", it.id)));
        }
    }
    for (id, loc) in locations {
        if let Some(it) = file.by_id.get(&id).and_then(|&i| file.items.get_mut(i))
            && it.location.is_none()
        {
            it.location = Some(loc);
        }
    }
    for (id, l) in links {
        if let Some(it) = file.by_id.get(&id).and_then(|&i| file.items.get_mut(i)) {
            it.properties.extend(l);
        }
    }
    Ok(())
}

fn parse_iinf(p: &[u8]) -> Result<Vec<Item>> {
    let mut c = Cur::new(p, "iinf");
    let (v, _) = c.full_header()?;
    let _count = if v == 0 { u32::from(c.u16()?) } else { c.u32()? };
    let mut items = Vec::new();
    for b in boxes(c.rest()) {
        let b = b?;
        if b.kind != b"infe" {
            continue;
        }
        if items.len() >= MAX_ITEMS {
            return Err(Error::TooLarge("items"));
        }
        if let Some(item) = parse_infe(b.payload)? {
            items.push(item);
        }
    }
    Ok(items)
}

/// One `infe`; `None` for a version this parser does not know (the item is then ignored).
fn parse_infe(p: &[u8]) -> Result<Option<Item>> {
    let mut c = Cur::new(p, "infe");
    let (v, flags) = c.full_header()?;
    let mut item = Item {
        id: 0,
        kind: FourCc::default(),
        name: String::new(),
        content_type: String::new(),
        hidden: flags & 1 != 0,
        protection_index: 0,
        location: None,
        properties: Vec::new(),
    };
    match v {
        0 | 1 => {
            item.id = u32::from(c.u16()?);
            item.protection_index = c.u16()?;
            let (name, rest) = c_string(c.rest());
            item.name = name;
            item.content_type = c_string(rest).0;
        }
        2 | 3 => {
            item.id = if v == 2 { u32::from(c.u16()?) } else { c.u32()? };
            item.protection_index = c.u16()?;
            item.kind = c.fourcc()?;
            let (name, rest) = c_string(c.rest());
            item.name = name;
            if item.kind == b"mime" {
                item.content_type = c_string(rest).0;
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(item))
}

fn parse_iloc(p: &[u8]) -> Result<Vec<(u32, ItemLocation)>> {
    let mut c = Cur::new(p, "iloc");
    let (v, _) = c.full_header()?;
    if v > 2 {
        return Err(Error::Unsupported(format!("iloc version {v}")));
    }
    let a = c.u8()?;
    let b = c.u8()?;
    let (offset_size, length_size, base_offset_size) = (a >> 4, a & 15, b >> 4);
    let index_size = if v == 0 { 0 } else { b & 15 };
    if [offset_size, length_size, base_offset_size, index_size].iter().any(|&s| s > 8) {
        return Err(Error::Invalid(format!("iloc field sizes {offset_size}/{length_size}/{base_offset_size}/{index_size}")));
    }
    let count = if v < 2 { usize::from(c.u16()?) } else { c.u32()? as usize };
    // id + data_reference_index + extent_count at least
    c.check_count(count, if v < 2 { 6 } else { 8 })?;
    let extent_bytes = usize::from(offset_size) + usize::from(length_size) + usize::from(index_size);
    let mut out = Vec::with_capacity(count.min(MAX_ITEMS));
    let mut extents_total = 0usize;
    for _ in 0..count {
        let id = if v < 2 { u32::from(c.u16()?) } else { c.u32()? };
        let construction_method = if v == 0 { 0 } else { (c.u16()? & 15) as u8 };
        let data_reference_index = c.u16()?;
        let base_offset = uint(&mut c, base_offset_size)?;
        let n = usize::from(c.u16()?);
        c.check_count(n, extent_bytes)?;
        extents_total = extents_total.saturating_add(n);
        if extents_total > MAX_EXTENTS {
            return Err(Error::TooLarge("item extents"));
        }
        let mut extents = Vec::with_capacity(n);
        for _ in 0..n {
            let index = uint(&mut c, index_size)?;
            let offset = uint(&mut c, offset_size)?;
            let length = uint(&mut c, length_size)?;
            extents.push(Extent { index, offset, length });
        }
        out.push((id, ItemLocation { construction_method, data_reference_index, base_offset, extents }));
    }
    Ok(out)
}

fn parse_iref(p: &[u8], out: &mut Vec<ItemReference>) -> Result<()> {
    let mut c = Cur::new(p, "iref");
    let (v, _) = c.full_header()?;
    let id_size = if v == 0 { 2 } else { 4 };
    for b in boxes(c.rest()) {
        let b = b?;
        let mut r = Cur::new(b.payload, "iref entry");
        let id = |r: &mut Cur| -> Result<u32> { if v == 0 { r.u16().map(u32::from) } else { r.u32() } };
        let from = id(&mut r)?;
        let n = usize::from(r.u16()?);
        r.check_count(n, id_size)?;
        let mut to = Vec::with_capacity(n);
        for _ in 0..n {
            to.push(id(&mut r)?);
        }
        out.push(ItemReference { kind: b.kind, from, to });
    }
    Ok(())
}

fn parse_ipma(p: &[u8], out: &mut Vec<(u32, Vec<PropertyLink>)>) -> Result<()> {
    let mut c = Cur::new(p, "ipma");
    let (v, flags) = c.full_header()?;
    let wide = flags & 1 != 0;
    let entries = c.u32()? as usize;
    // id + association_count at least
    c.check_count(entries, if v < 1 { 3 } else { 5 })?;
    for _ in 0..entries {
        let id = if v < 1 { u32::from(c.u16()?) } else { c.u32()? };
        let n = usize::from(c.u8()?);
        c.check_count(n, if wide { 2 } else { 1 })?;
        let mut links = Vec::with_capacity(n);
        for _ in 0..n {
            let (essential, index) = if wide {
                let x = c.u16()?;
                (x & 0x8000 != 0, usize::from(x & 0x7FFF))
            } else {
                let x = c.u8()?;
                (x & 0x80 != 0, usize::from(x & 0x7F))
            };
            // index 0: no property
            if let Some(index) = index.checked_sub(1) {
                links.push(PropertyLink { index, essential });
            }
        }
        out.push((id, links));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
