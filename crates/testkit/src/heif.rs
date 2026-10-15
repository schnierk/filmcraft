//! A small HEIF / AVIF writer for tests (ISO/IEC 23008-12 file layout: `ftyp`, `meta`, `mdat`).
//!
//! It writes what the parser and decoder tests need, with every version and field size they must
//! accept: `infe` v2 / v3, `iloc` v0–v2 with any offset / length / base-offset / index size, item
//! data in `mdat` or `idat` split into several extents, `iref` and `ipma` v0 / v1 (7- or 15-bit
//! property indices, essential flags). Property boxes are passed in whole, so tests can also hand
//! it damaged ones. Helpers build the common properties and the `grid` and `Exif` item payloads.
//!
//! Test-only code: a layout the chosen field sizes cannot express is a panic.

/// Where an item's bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// In the `mdat` box (construction method 0, file offsets).
    Mdat,
    /// In the `idat` box (construction method 1; needs `iloc` v1 or v2).
    Idat,
    /// No `iloc` entry at all.
    Nowhere,
}

/// One item.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: u32,
    pub kind: [u8; 4],
    pub name: String,
    /// MIME type of a `mime` item.
    pub content_type: Option<String>,
    pub hidden: bool,
    pub data: Vec<u8>,
    pub storage: Storage,
    /// The data is split into this many extents (at least 1).
    pub extents: usize,
    /// (1-based `ipco` index, essential).
    pub properties: Vec<(u16, bool)>,
}

impl Item {
    pub fn new(id: u32, kind: &[u8; 4], data: Vec<u8>) -> Self {
        Item { id, kind: *kind, name: String::new(), content_type: None, hidden: false, data, storage: Storage::Mdat, extents: 1, properties: Vec::new() }
    }
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
    pub fn in_idat(mut self) -> Self {
        self.storage = Storage::Idat;
        self
    }
    pub fn extents(mut self, n: usize) -> Self {
        self.extents = n.max(1);
        self
    }
    /// Associate property `index` (1-based, as returned by [`Heif::property`]).
    pub fn with(mut self, index: u16, essential: bool) -> Self {
        self.properties.push((index, essential));
        self
    }
}

/// Field sizes and version of `iloc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IlocLayout {
    pub version: u8,
    pub offset_size: u8,
    pub length_size: u8,
    pub base_offset_size: u8,
    /// Written for versions 1 and 2 only.
    pub index_size: u8,
}

impl Default for IlocLayout {
    fn default() -> Self {
        IlocLayout { version: 1, offset_size: 4, length_size: 4, base_offset_size: 0, index_size: 0 }
    }
}

/// A HEIF / AVIF file being put together.
#[derive(Clone, Debug)]
pub struct Heif {
    pub major_brand: [u8; 4],
    pub compatible_brands: Vec<[u8; 4]>,
    pub handler: [u8; 4],
    pub primary: Option<u32>,
    pub items: Vec<Item>,
    /// Complete property boxes, in `ipco` order.
    pub properties: Vec<Vec<u8>>,
    /// (type, from, to).
    pub references: Vec<([u8; 4], u32, Vec<u32>)>,
    pub iloc: IlocLayout,
    /// `iref` version: 0 = 16-bit ids, 1 = 32-bit ids (forced to 1 when an id needs it).
    pub iref_version: u8,
    /// `ipma` version: 0 = 16-bit item ids, 1 = 32-bit (forced to 1 when an id needs it).
    pub ipma_version: u8,
    /// `ipma` flag 1: 15-bit property indices.
    pub ipma_wide: bool,
    /// Extra boxes appended inside `meta`.
    pub extra_meta: Vec<Vec<u8>>,
}

impl Heif {
    /// An empty `heic` file (`mif1`, `heic` compatible) with handler `pict`.
    pub fn new(major_brand: &[u8; 4]) -> Self {
        Heif {
            major_brand: *major_brand,
            compatible_brands: vec![*b"mif1", *major_brand],
            handler: *b"pict",
            primary: None,
            items: Vec::new(),
            properties: Vec::new(),
            references: Vec::new(),
            iloc: IlocLayout::default(),
            iref_version: 0,
            ipma_version: 0,
            ipma_wide: false,
            extra_meta: Vec::new(),
        }
    }

    /// Add a property box; returns its 1-based `ipco` index.
    pub fn property(&mut self, b: Vec<u8>) -> u16 {
        self.properties.push(b);
        u16::try_from(self.properties.len()).expect("property count")
    }

    pub fn item(&mut self, item: Item) -> &mut Self {
        self.items.push(item);
        self
    }

    pub fn reference(&mut self, kind: &[u8; 4], from: u32, to: &[u32]) -> &mut Self {
        self.references.push((*kind, from, to.to_vec()));
        self
    }

    /// The whole file.
    pub fn build(&self) -> Vec<u8> {
        let ftyp = {
            let mut p = self.major_brand.to_vec();
            p.extend_from_slice(&0u32.to_be_bytes());
            for b in &self.compatible_brands {
                p.extend_from_slice(b);
            }
            boxed(b"ftyp", &p)
        };
        // the meta box's size does not depend on the offsets in it: build once to measure
        let mdat_payload_start = ftyp.len() as u64 + self.meta(0).len() as u64 + 8;
        let meta = self.meta(mdat_payload_start);
        let mut out = ftyp;
        out.extend_from_slice(&meta);
        let mdat: Vec<u8> = self.items.iter().filter(|i| i.storage == Storage::Mdat).flat_map(|i| i.data.iter().copied()).collect();
        out.extend_from_slice(&boxed(b"mdat", &mdat));
        out
    }

    fn meta(&self, mdat_start: u64) -> Vec<u8> {
        let mut m = full(0, 0);
        let mut hdlr = full(0, 0);
        hdlr.extend_from_slice(&[0; 4]);
        hdlr.extend_from_slice(&self.handler);
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.push(0);
        m.extend_from_slice(&boxed(b"hdlr", &hdlr));
        if let Some(p) = self.primary {
            let mut b = full(u8::from(p > 0xFFFF), 0);
            push_id(&mut b, p, p > 0xFFFF);
            m.extend_from_slice(&boxed(b"pitm", &b));
        }
        m.extend_from_slice(&self.iinf());
        if !self.references.is_empty() {
            m.extend_from_slice(&self.iref());
        }
        m.extend_from_slice(&self.iprp());
        m.extend_from_slice(&self.iloc(mdat_start));
        let idat: Vec<u8> = self.items.iter().filter(|i| i.storage == Storage::Idat).flat_map(|i| i.data.iter().copied()).collect();
        if !idat.is_empty() {
            m.extend_from_slice(&boxed(b"idat", &idat));
        }
        for b in &self.extra_meta {
            m.extend_from_slice(b);
        }
        boxed(b"meta", &m)
    }

    fn iinf(&self) -> Vec<u8> {
        let mut b = full(0, 0);
        b.extend_from_slice(&u16::try_from(self.items.len()).expect("item count").to_be_bytes());
        for it in &self.items {
            let wide = it.id > 0xFFFF;
            let mut e = full(if wide { 3 } else { 2 }, u32::from(it.hidden));
            push_id(&mut e, it.id, wide);
            e.extend_from_slice(&0u16.to_be_bytes());
            e.extend_from_slice(&it.kind);
            e.extend_from_slice(it.name.as_bytes());
            e.push(0);
            if let Some(t) = &it.content_type {
                e.extend_from_slice(t.as_bytes());
                e.push(0);
            }
            b.extend_from_slice(&boxed(b"infe", &e));
        }
        boxed(b"iinf", &b)
    }

    fn iref(&self) -> Vec<u8> {
        let wide = self.iref_version == 1 || self.references.iter().any(|(_, f, t)| *f > 0xFFFF || t.iter().any(|&x| x > 0xFFFF));
        let mut b = full(u8::from(wide), 0);
        for (kind, from, to) in &self.references {
            let mut e = Vec::new();
            push_id(&mut e, *from, wide);
            e.extend_from_slice(&u16::try_from(to.len()).expect("reference count").to_be_bytes());
            for &t in to {
                push_id(&mut e, t, wide);
            }
            b.extend_from_slice(&boxed(kind, &e));
        }
        boxed(b"iref", &b)
    }

    fn iprp(&self) -> Vec<u8> {
        let ipco: Vec<u8> = self.properties.iter().flatten().copied().collect();
        let wide_ids = self.ipma_version == 1 || self.items.iter().any(|i| i.id > 0xFFFF);
        let mut ipma = full(u8::from(wide_ids), u32::from(self.ipma_wide));
        let with: Vec<&Item> = self.items.iter().filter(|i| !i.properties.is_empty()).collect();
        ipma.extend_from_slice(&(with.len() as u32).to_be_bytes());
        for it in with {
            push_id(&mut ipma, it.id, wide_ids);
            ipma.push(u8::try_from(it.properties.len()).expect("association count"));
            for &(index, essential) in &it.properties {
                if self.ipma_wide {
                    assert!(index < 0x8000);
                    ipma.extend_from_slice(&((u16::from(essential) << 15) | index).to_be_bytes());
                } else {
                    assert!(index < 0x80, "property index {index} needs ipma_wide");
                    ipma.push((u8::from(essential) << 7) | index as u8);
                }
            }
        }
        let mut b = boxed(b"ipco", &ipco);
        b.extend_from_slice(&boxed(b"ipma", &ipma));
        boxed(b"iprp", &b)
    }

    fn iloc(&self, mdat_start: u64) -> Vec<u8> {
        let l = self.iloc;
        let mut b = full(l.version, 0);
        b.push((l.offset_size << 4) | l.length_size);
        b.push((l.base_offset_size << 4) | if l.version == 0 { 0 } else { l.index_size });
        let located: Vec<&Item> = self.items.iter().filter(|i| i.storage != Storage::Nowhere).collect();
        if l.version < 2 {
            b.extend_from_slice(&u16::try_from(located.len()).expect("iloc count").to_be_bytes());
        } else {
            b.extend_from_slice(&(located.len() as u32).to_be_bytes());
        }
        let (mut in_mdat, mut in_idat) = (mdat_start, 0u64);
        for it in located {
            let start = match it.storage {
                Storage::Idat => {
                    assert!(l.version > 0, "idat needs iloc v1 or v2");
                    &mut in_idat
                }
                _ => &mut in_mdat,
            };
            push_id(&mut b, it.id, l.version == 2);
            if l.version > 0 {
                b.extend_from_slice(&u16::from(it.storage == Storage::Idat).to_be_bytes());
            }
            b.extend_from_slice(&0u16.to_be_bytes());
            let base = if l.base_offset_size > 0 { *start } else { 0 };
            push_uint(&mut b, base, l.base_offset_size);
            let n = it.extents.max(1);
            b.extend_from_slice(&u16::try_from(n).expect("extent count").to_be_bytes());
            let len = it.data.len() as u64;
            let mut at = 0u64;
            for k in 0..n as u64 {
                let next = len * (k + 1) / n as u64;
                if l.version > 0 && l.index_size > 0 {
                    push_uint(&mut b, 0, l.index_size);
                }
                let offset = *start + at - base;
                assert!(l.offset_size > 0 || offset == 0, "offset_size 0 cannot place this extent");
                push_uint(&mut b, offset, l.offset_size);
                // length_size 0: no length field, the extent runs to the end of the file or idat
                assert!(l.length_size > 0 || n == 1, "length_size 0 needs a single extent running to the end");
                if l.length_size > 0 {
                    push_uint(&mut b, next - at, l.length_size);
                }
                at = next;
            }
            *start += len;
        }
        boxed(b"iloc", &b)
    }
}

/// A box with a 32-bit size.
pub fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = (u32::try_from(payload.len() + 8).expect("box size")).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(payload);
    b
}

/// A full-box header (version, flags).
pub fn full(version: u8, flags: u32) -> Vec<u8> {
    ((u32::from(version) << 24) | (flags & 0xFF_FFFF)).to_be_bytes().to_vec()
}

fn push_id(b: &mut Vec<u8>, id: u32, wide: bool) {
    if wide {
        b.extend_from_slice(&id.to_be_bytes());
    } else {
        b.extend_from_slice(&u16::try_from(id).expect("16-bit id").to_be_bytes());
    }
}

fn push_uint(b: &mut Vec<u8>, v: u64, size: u8) {
    if size < 8 {
        assert!(v < 1u64 << (8 * u32::from(size)), "{v} does not fit {size} bytes");
    }
    b.extend_from_slice(&v.to_be_bytes()[8 - size as usize..]);
}

/// `ispe`.
pub fn ispe(width: u32, height: u32) -> Vec<u8> {
    let mut p = full(0, 0);
    p.extend_from_slice(&width.to_be_bytes());
    p.extend_from_slice(&height.to_be_bytes());
    boxed(b"ispe", &p)
}

/// `irot` with `quarter_turns` anti-clockwise.
pub fn irot(quarter_turns: u8) -> Vec<u8> {
    boxed(b"irot", &[quarter_turns & 3])
}

/// `imir`: mode 0 exchanges top and bottom, 1 left and right (ISO/IEC 23008-12:2022 §6.5.12).
pub fn imir(mode: u8) -> Vec<u8> {
    boxed(b"imir", &[mode & 1])
}

/// `clap` from (numerator, denominator) pairs.
pub fn clap(width: (u32, u32), height: (u32, u32), horiz_offset: (i32, u32), vert_offset: (i32, u32)) -> Vec<u8> {
    let mut p = Vec::new();
    for v in [width.0, width.1, height.0, height.1] {
        p.extend_from_slice(&v.to_be_bytes());
    }
    p.extend_from_slice(&horiz_offset.0.to_be_bytes());
    p.extend_from_slice(&horiz_offset.1.to_be_bytes());
    p.extend_from_slice(&vert_offset.0.to_be_bytes());
    p.extend_from_slice(&vert_offset.1.to_be_bytes());
    boxed(b"clap", &p)
}

/// `colr` `nclx`.
pub fn colr_nclx(primaries: u16, transfer: u16, matrix: u16, full_range: bool) -> Vec<u8> {
    let mut p = b"nclx".to_vec();
    for v in [primaries, transfer, matrix] {
        p.extend_from_slice(&v.to_be_bytes());
    }
    p.push(u8::from(full_range) << 7);
    boxed(b"colr", &p)
}

/// `colr` with an ICC profile (`prof` or `rICC`).
pub fn colr_icc(kind: &[u8; 4], profile: &[u8]) -> Vec<u8> {
    let mut p = kind.to_vec();
    p.extend_from_slice(profile);
    boxed(b"colr", &p)
}

/// `pixi`.
pub fn pixi(bits: &[u8]) -> Vec<u8> {
    let mut p = full(0, 0);
    p.push(u8::try_from(bits.len()).expect("channels"));
    p.extend_from_slice(bits);
    boxed(b"pixi", &p)
}

/// `auxC`.
pub fn auxc(urn: &str, subtype: &[u8]) -> Vec<u8> {
    let mut p = full(0, 0);
    p.extend_from_slice(urn.as_bytes());
    p.push(0);
    p.extend_from_slice(subtype);
    boxed(b"auxC", &p)
}

/// `hvcC` (HEVCDecoderConfigurationRecord) holding the given parameter sets (NAL units without
/// start codes). Profile and level fields are zero.
pub fn hvcc(length_size: u8, chroma_format_idc: u8, bit_depth: u8, vps: &[&[u8]], sps: &[&[u8]], pps: &[&[u8]]) -> Vec<u8> {
    let mut p = vec![1u8, 0x01];
    p.extend_from_slice(&[0; 4 + 6 + 1]);
    p.extend_from_slice(&[
        0xF0,
        0x00,
        0xFC,
        0xFC | (chroma_format_idc & 3),
        0xF8 | (bit_depth.saturating_sub(8) & 7),
        0xF8 | (bit_depth.saturating_sub(8) & 7),
    ]);
    p.extend_from_slice(&[0, 0]);
    p.push(0x0C | (length_size.clamp(1, 4) - 1));
    let arrays: Vec<(u8, &[&[u8]])> = [(32u8, vps), (33, sps), (34, pps)].into_iter().filter(|(_, n)| !n.is_empty()).collect();
    p.push(arrays.len() as u8);
    for (t, nalus) in arrays {
        p.push(0x80 | t);
        p.extend_from_slice(&u16::try_from(nalus.len()).expect("NAL count").to_be_bytes());
        for n in nalus {
            p.extend_from_slice(&u16::try_from(n.len()).expect("NAL size").to_be_bytes());
            p.extend_from_slice(n);
        }
    }
    boxed(b"hvcC", &p)
}

/// `av1C` with the given configuration OBUs (usually the sequence header).
pub fn av1c(profile: u8, high_bitdepth: bool, monochrome: bool, subsampling: (bool, bool), config_obus: &[u8]) -> Vec<u8> {
    let mut p = vec![0x81, profile << 5];
    p.push((u8::from(high_bitdepth) << 6) | (u8::from(monochrome) << 4) | (u8::from(subsampling.0) << 3) | (u8::from(subsampling.1) << 2));
    p.push(0);
    p.extend_from_slice(config_obus);
    boxed(b"av1C", &p)
}

/// The data of a `grid` item (32-bit output size fields when either side needs them or `wide`).
pub fn grid(rows: u16, columns: u16, width: u32, height: u32, wide: bool) -> Vec<u8> {
    let wide = wide || width > 0xFFFF || height > 0xFFFF;
    let mut p = vec![0, u8::from(wide), u8::try_from(rows - 1).expect("rows"), u8::try_from(columns - 1).expect("columns")];
    if wide {
        p.extend_from_slice(&width.to_be_bytes());
        p.extend_from_slice(&height.to_be_bytes());
    } else {
        p.extend_from_slice(&u16::try_from(width).expect("width").to_be_bytes());
        p.extend_from_slice(&u16::try_from(height).expect("height").to_be_bytes());
    }
    p
}

/// The data of an `Exif` item: the offset field, then `Exif\0\0` and the TIFF bytes.
pub fn exif(tiff: &[u8]) -> Vec<u8> {
    let mut p = 6u32.to_be_bytes().to_vec();
    p.extend_from_slice(b"Exif\0\0");
    p.extend_from_slice(tiff);
    p
}

/// Split an Annex B byte stream (start codes) into NAL units.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
            // a four-byte start code leaves a zero before the next one
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            &data[s..e]
        })
        .collect()
}

/// NAL units with `length_size`-byte big-endian length prefixes (HEIF / MP4 item data).
pub fn length_prefixed(nals: &[&[u8]], length_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&(n.len() as u64).to_be_bytes()[8 - length_size..]);
        out.extend_from_slice(n);
    }
    out
}

/// An HEVC Annex B stream as an image item: the parameter sets go into an `hvcC` (4-byte NAL
/// lengths) and the other NAL units into the item data.
pub fn hevc_item(annexb: &[u8], chroma_format_idc: u8, bit_depth: u8) -> (Vec<u8>, Vec<u8>) {
    let nals = annexb_nals(annexb);
    let kind = |n: &[u8]| n.first().map_or(0, |h| (h >> 1) & 0x3F);
    let of = |t: u8| nals.iter().copied().filter(|n| kind(n) == t).collect::<Vec<_>>();
    let config = hvcc(4, chroma_format_idc, bit_depth, &of(32), &of(33), &of(34));
    let rest: Vec<&[u8]> = nals.iter().copied().filter(|n| !(32..=34).contains(&kind(n))).collect();
    (config, length_prefixed(&rest, 4))
}
