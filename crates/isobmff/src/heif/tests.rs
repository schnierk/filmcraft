//! HEIF `meta` parsing on files built by the test writer (`filmcraft_testkit::heif`): every box
//! version and field size, then truncated, mutated and oversized input.

use super::*;
use filmcraft_testkit::heif::{self as w, Heif, IlocLayout, Item, Storage};

/// A fake `hvcC` with one tiny parameter set of each kind.
fn hvcc() -> Vec<u8> {
    w::hvcc(4, 1, 8, &[&[0x40, 0x01, 0xAA]], &[&[0x42, 0x01, 0xBB]], &[&[0x44, 0x01, 0xCC]])
}

/// A 2×2 grid of hvc1 tiles (data in `mdat`, two extents each) with transforms, colour, an alpha
/// grid and an Exif item.
fn grid_file() -> Heif {
    let mut h = Heif::new(b"heic");
    let p_hvcc = h.property(hvcc());
    let p_tile = h.property(w::ispe(64, 48));
    let p_size = h.property(w::ispe(100, 90));
    let p_rot = h.property(w::irot(1));
    let p_mir = h.property(w::imir(1));
    let p_clap = h.property(w::clap((80, 1), (70, 1), (-2, 1), (3, 2)));
    let p_nclx = h.property(w::colr_nclx(1, 13, 6, true));
    let p_icc = h.property(w::colr_icc(b"prof", b"ICC!"));
    let p_pixi = h.property(w::pixi(&[8, 8, 8]));
    let p_aux = h.property(w::auxc("urn:mpeg:hevc:2015:auxid:1", &[7, 9]));
    h.item(Item::new(10, b"grid", w::grid(2, 2, 100, 90, false)).in_idat().with(p_size, false).with(p_clap, true).with(p_rot, true).with(p_mir, true));
    for t in 1..=4u32 {
        h.item(Item::new(t, b"hvc1", vec![t as u8; 5 + t as usize]).hidden().extents(2).with(p_hvcc, true).with(p_tile, false));
    }
    h.item(Item::new(20, b"hvc1", vec![0xA1; 9]).hidden().with(p_hvcc, true).with(p_size, false).with(p_aux, false));
    let mut x = Item::new(30, b"Exif", w::exif(b"MM\0*rest"));
    x.hidden = true;
    h.item(x);
    let mut xmp = Item::new(31, b"mime", b"<x:xmpmeta/>".to_vec());
    xmp.content_type = Some("application/rdf+xml".into());
    h.item(xmp.hidden());
    h.items[0].properties.extend([(p_nclx, false), (p_icc, true), (p_pixi, false)]);
    h.primary = Some(10);
    h.reference(b"dimg", 10, &[1, 2, 3, 4]).reference(b"auxl", 20, &[10]).reference(b"cdsc", 30, &[10]).reference(b"cdsc", 31, &[10]);
    h
}

#[test]
fn parses_items_references_and_properties() {
    let bytes = grid_file().build();
    let f = parse(&bytes[..]).unwrap();
    assert_eq!(f.major_brand, *b"heic");
    assert_eq!(f.compatible_brands, vec![FourCc(*b"mif1"), FourCc(*b"heic")]);
    assert_eq!(f.handler, *b"pict");
    assert!(f.has_meta && !f.has_movie);
    assert_eq!(f.primary_item, Some(10));
    assert_eq!(f.items.len(), 8);
    let g = f.item(10).unwrap();
    assert_eq!((g.kind, g.hidden), (FourCc(*b"grid"), false));
    assert_eq!(g.location.as_ref().unwrap().construction_method, 1);
    let grid = ImageGrid::parse(&f.read_item(&bytes[..], 10, 1 << 20).unwrap()).unwrap();
    assert_eq!(grid, ImageGrid { rows: 2, columns: 2, output_width: 100, output_height: 90 });
    assert_eq!(grid.cells(), 4);
    assert_eq!(f.references_from(10, b"dimg"), vec![1, 2, 3, 4]);
    assert_eq!(f.references_to(10, b"auxl"), vec![20]);
    assert_eq!(f.references_to(10, b"cdsc"), vec![30, 31]);
    for t in 1..=4u32 {
        let it = f.item(t).unwrap();
        assert!(it.hidden);
        assert_eq!(it.location.as_ref().unwrap().extents.len(), 2);
        assert_eq!(f.read_item(&bytes[..], t, 1 << 20).unwrap(), vec![t as u8; 5 + t as usize]);
        assert!(matches!(f.find_property(t, |p| match p {
            Property::Hevc(c) => Some(c.clone()),
            _ => None,
        }), Some(c) if c.sps() == vec![&[0x42, 0x01, 0xBB][..]] && c.length_size == 4));
        assert_eq!(f.find_property(t, |p| if let Property::ImageSize { width, height } = p { Some((*width, *height)) } else { None }), Some((64, 48)));
    }
    let props: Vec<(Option<&Property>, bool)> = f.item_properties(10).collect();
    let kinds: Vec<(String, bool)> = props.iter().map(|(p, e)| (p.map(|p| p.kind().to_string()).unwrap_or_default(), *e)).collect();
    let want = [("ispe", false), ("clap", true), ("irot", true), ("imir", true), ("colr", false), ("colr", true), ("pixi", false)];
    assert_eq!(kinds, want.iter().map(|(k, e)| (k.to_string(), *e)).collect::<Vec<_>>());
    assert_eq!(props[2].0, Some(&Property::Rotation(1)));
    assert_eq!(props[3].0, Some(&Property::Mirror(Mirror::LeftRight)));
    assert_eq!(props[1].0, Some(&Property::CleanAperture(CleanAperture { width: (80, 1), height: (70, 1), horiz_offset: (-2, 1), vert_offset: (3, 2) })));
    assert_eq!(props[4].0, Some(&Property::Color(ColorInfo::Nclx { primaries: 1, transfer: 13, matrix: 6, full_range: true })));
    assert_eq!(props[5].0, Some(&Property::Color(ColorInfo::Icc { kind: FourCc(*b"prof"), profile: b"ICC!".to_vec() })));
    assert_eq!(props[6].0, Some(&Property::PixelInfo(vec![8, 8, 8])));
    assert_eq!(
        f.find_property(20, |p| if let Property::Auxiliary { urn, subtype } = p { Some((urn.clone(), subtype.clone())) } else { None }),
        Some(("urn:mpeg:hevc:2015:auxid:1".to_string(), vec![7, 9]))
    );
    let exif = f.read_item(&bytes[..], 30, 1 << 20).unwrap();
    assert_eq!(exif_tiff(&exif), Some(&b"MM\0*rest"[..]));
    let xmp = f.item(31).unwrap();
    assert_eq!((xmp.kind, xmp.content_type.as_str()), (FourCc(*b"mime"), "application/rdf+xml"));
    assert_eq!(f.read_item(&bytes[..], 31, 1 << 20).unwrap(), b"<x:xmpmeta/>");
    // the item size limit holds
    assert!(matches!(f.read_item(&bytes[..], 4, 3), Err(Error::TooLarge(_))));
    assert_eq!(f.item_len(4, bytes.len() as u64).unwrap(), 9);
}

#[test]
fn every_iloc_version_and_field_size() {
    let layouts = [
        // (version, offset, length, base, index)
        (0, 4, 4, 0, 0),
        (0, 8, 8, 8, 0),
        (0, 4, 4, 4, 0),
        (1, 4, 4, 0, 0),
        (1, 8, 4, 4, 4),
        (1, 4, 8, 8, 8),
        (1, 0, 4, 4, 0),
        (2, 4, 4, 0, 0),
        (2, 8, 8, 8, 8),
        (2, 0, 8, 8, 4),
    ];
    for (version, offset_size, length_size, base_offset_size, index_size) in layouts {
        let mut h = Heif::new(b"mif1");
        h.iloc = IlocLayout { version, offset_size, length_size, base_offset_size, index_size };
        let one_extent = offset_size == 0;
        for id in 1..=3u32 {
            let mut it = Item::new(id, b"hvc1", (0..40 + id).map(|i| (i * 7 + id) as u8).collect());
            if !one_extent {
                it = it.extents(id as usize);
            }
            if version > 0 && id == 2 {
                it = it.in_idat();
            }
            h.item(it);
        }
        h.primary = Some(1);
        let bytes = h.build();
        let f = parse(&bytes[..]).unwrap_or_else(|e| panic!("{version}/{offset_size}/{length_size}/{base_offset_size}/{index_size}: {e}"));
        for id in 1..=3u32 {
            let want: Vec<u8> = (0..40 + id).map(|i| (i * 7 + id) as u8).collect();
            assert_eq!(
                f.read_item(&bytes[..], id, 1 << 20).unwrap(),
                want,
                "layout {version}/{offset_size}/{length_size}/{base_offset_size}/{index_size} item {id}"
            );
        }
    }
}

#[test]
fn zero_length_extents_run_to_the_end() {
    // length_size 0: one extent per item, reaching the end of the file (the last mdat item)
    let mut h = Heif::new(b"heic");
    h.iloc = IlocLayout { version: 1, offset_size: 4, length_size: 0, base_offset_size: 0, index_size: 0 };
    h.item(Item::new(1, b"hvc1", vec![5; 33]));
    h.primary = Some(1);
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert_eq!(f.item(1).unwrap().location.as_ref().unwrap().extents[0].length, 0);
    assert_eq!(f.read_item(&bytes[..], 1, 1 << 20).unwrap(), vec![5; 33]);
    // and inside idat, to the end of idat
    let mut h = Heif::new(b"heic");
    h.iloc = IlocLayout { version: 1, offset_size: 4, length_size: 0, base_offset_size: 0, index_size: 0 };
    h.item(Item::new(1, b"grid", vec![9; 12]).in_idat());
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert_eq!(f.read_item(&bytes[..], 1, 1 << 20).unwrap(), vec![9; 12]);
}

#[test]
fn large_ids_use_wide_boxes() {
    let mut h = Heif::new(b"avif");
    let mut indices = Vec::new();
    // more than 127 properties: 15-bit ipma indices
    for i in 0..140u32 {
        indices.push(h.property(w::ispe(i + 1, 2 * i + 1)));
    }
    h.ipma_wide = true;
    h.iloc = IlocLayout { version: 2, ..IlocLayout::default() };
    let id = 70_000u32;
    h.item(Item::new(id, b"av01", vec![1, 2, 3]).with(indices[139], true).with(indices[3], false));
    h.item(Item::new(id + 1, b"av01", vec![4]).hidden().with(indices[0], false));
    h.primary = Some(id);
    h.reference(b"thmb", id + 1, &[id]);
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert_eq!(f.primary_item, Some(id));
    assert_eq!(f.properties.len(), 140);
    let links = &f.item(id).unwrap().properties;
    assert_eq!(links, &vec![PropertyLink { index: 139, essential: true }, PropertyLink { index: 3, essential: false }]);
    assert_eq!(f.find_property(id, |p| if let Property::ImageSize { width, .. } = p { Some(*width) } else { None }), Some(140));
    assert_eq!(f.references_to(id, b"thmb"), vec![id + 1]);
    assert_eq!(f.read_item(&bytes[..], id, 99).unwrap(), vec![1, 2, 3]);
    // forced 32-bit iref / ipma ids with small ids parse the same
    let mut h = grid_file();
    h.iref_version = 1;
    h.ipma_version = 1;
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert_eq!(f.references_from(10, b"dimg"), vec![1, 2, 3, 4]);
    assert_eq!(f.item_properties(10).count(), 7);
}

#[test]
fn property_index_zero_and_dangling_links() {
    let mut h = Heif::new(b"heic");
    let p = h.property(w::ispe(8, 8));
    h.item(Item::new(1, b"hvc1", vec![0; 4]).with(0, true).with(p, false).with(42, true));
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    // index 0 means "no property"; 42 points past ipco and resolves to nothing
    let props: Vec<_> = f.item_properties(1).collect();
    assert_eq!(props, vec![(Some(&Property::ImageSize { width: 8, height: 8 }), false), (None, true)]);
}

#[test]
fn malformed_and_unknown_properties_are_kept_as_other() {
    let mut h = Heif::new(b"heic");
    let a = h.property(w::boxed(b"hvcC", &[1, 2]));
    let b = h.property(w::boxed(b"ispe", &[1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1]));
    let c = h.property(w::boxed(b"lsel", &[0, 1]));
    let d = h.property(w::boxed(b"pixi", &[0, 0, 0, 0, 9, 8]));
    h.item(Item::new(1, b"hvc1", vec![0; 4]).with(a, true).with(b, false).with(c, true).with(d, false));
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    let props: Vec<Property> = f.item_properties(1).map(|(p, _)| p.unwrap().clone()).collect();
    assert_eq!(
        props,
        vec![
            Property::Other { kind: FourCc(*b"hvcC"), malformed: true },
            Property::Other { kind: FourCc(*b"ispe"), malformed: true },
            Property::Other { kind: FourCc(*b"lsel"), malformed: false },
            Property::Other { kind: FourCc(*b"pixi"), malformed: true },
        ]
    );
}

#[test]
fn grid_payloads() {
    assert_eq!(ImageGrid::parse(&w::grid(3, 4, 1000, 700, false)).unwrap(), ImageGrid { rows: 3, columns: 4, output_width: 1000, output_height: 700 });
    assert_eq!(
        ImageGrid::parse(&w::grid(256, 256, 100_000, 7, false)).unwrap(),
        ImageGrid { rows: 256, columns: 256, output_width: 100_000, output_height: 7 }
    );
    assert_eq!(ImageGrid::parse(&w::grid(1, 1, 5, 6, true)).unwrap().output_width, 5);
    assert!(matches!(ImageGrid::parse(&[1, 0, 0, 0, 0, 1, 0, 1]), Err(Error::Unsupported(_))));
    assert!(matches!(ImageGrid::parse(&[0, 0, 0, 0, 0, 0, 0, 1]), Err(Error::Invalid(_))));
    for n in 0..8 {
        assert!(ImageGrid::parse(&w::grid(2, 2, 10, 10, false)[..n]).is_err());
    }
    assert!(ImageGrid::parse(&w::grid(2, 2, 10, 10, true)[..10]).is_err());
}

#[test]
fn exif_tiff_header_variants() {
    assert_eq!(exif_tiff(&w::exif(b"II*\0abc")), Some(&b"II*\0abc"[..]));
    // offset 0, TIFF right after the offset field
    assert_eq!(exif_tiff(b"\0\0\0\0MM\0*x"), Some(&b"MM\0*x"[..]));
    // offset 0 although an Exif\0\0 prefix is there
    assert_eq!(exif_tiff(b"\0\0\0\0Exif\0\0MM\0*x"), Some(&b"MM\0*x"[..]));
    // offset pointing anywhere else
    assert_eq!(exif_tiff(b"\xFF\xFF\xFF\xFFExif\0\0MM\0*x"), Some(&b"MM\0*x"[..]));
    assert_eq!(exif_tiff(b"\0\0\0\x02nope"), None);
    assert_eq!(exif_tiff(b"\0\0"), None);
}

#[test]
fn sniffs_heif_and_avif_brands() {
    let ftyp = |major: &[u8; 4], compat: &[&[u8; 4]]| {
        let mut p = major.to_vec();
        p.extend_from_slice(&[0; 4]);
        for c in compat {
            p.extend_from_slice(*c);
        }
        w::boxed(b"ftyp", &p)
    };
    assert!(sniff(&ftyp(b"heic", &[b"mif1", b"heic"])));
    assert!(sniff(&ftyp(b"avif", &[b"mif1"])));
    assert!(sniff(&ftyp(b"mif1", &[])));
    assert!(sniff(&ftyp(b"MiHB", &[b"heix"])), "a compatible brand is enough");
    assert!(sniff(&ftyp(b"msf1", &[b"hevc"])));
    assert!(!sniff(&ftyp(b"isom", &[b"iso2", b"avc1", b"mp41"])));
    assert!(!sniff(&ftyp(b"qt  ", &[])));
    assert!(!sniff(b"\0\0\0\x18ftyp"));
    assert!(!sniff(&[]));
    assert_eq!(file_type(&ftyp(b"heic", &[b"mif1"])), Some((FourCc(*b"heic"), vec![FourCc(*b"mif1")])));
    // a head shorter than the ftyp box still yields the brands it holds
    assert_eq!(file_type(&ftyp(b"avif", &[b"mif1", b"miaf"])[..20]), Some((FourCc(*b"avif"), vec![FourCc(*b"mif1")])));
}

#[test]
fn files_without_meta() {
    let mut b = w::boxed(b"ftyp", b"msf1\0\0\0\0hevc");
    b.extend_from_slice(&w::boxed(b"moov", &[0; 8]));
    b.extend_from_slice(&w::boxed(b"mdat", &[1; 20]));
    let f = parse(&b[..]).unwrap();
    assert!(!f.has_meta && f.has_movie);
    assert_eq!(f.major_brand, *b"msf1");
    assert!(f.items.is_empty() && f.primary_item.is_none());
    // a size-0 box runs to the end of the file
    let mut b = w::boxed(b"ftyp", b"heic\0\0\0\0");
    b.extend_from_slice(&[0, 0, 0, 0, b'm', b'd', b'a', b't', 1, 2, 3]);
    assert!(!parse(&b[..]).unwrap().has_meta);
}

#[test]
fn quicktime_style_meta_without_full_box_header() {
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"hvc1", vec![3; 7]));
    h.primary = Some(1);
    let bytes = h.build();
    // drop the meta full-box header and fix the meta size
    let at = bytes.windows(4).position(|x| x == b"meta").unwrap() - 4;
    let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let mut qt = bytes[..at].to_vec();
    qt.extend_from_slice(&(size - 4).to_be_bytes());
    qt.extend_from_slice(b"meta");
    qt.extend_from_slice(&bytes[at + 12..]);
    // the iloc offsets moved by 4: the file-level item is now 4 bytes earlier
    let f = parse(&qt[..]).unwrap();
    assert_eq!(f.handler, *b"pict");
    assert_eq!(f.primary_item, Some(1));
    let loc = f.item(1).unwrap().location.as_ref().unwrap();
    let start = loc.extents[0].offset as usize - 4;
    assert_eq!(&qt[start..start + 7], &[3; 7]);
}

#[test]
fn unsupported_and_invalid_structures() {
    // construction method 2 and external data references are refused when reading
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"hvc1", vec![1; 4]));
    let mut bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert_eq!(f.read_item(&bytes[..], 1, 99).unwrap(), vec![1; 4]);
    let iloc = bytes.windows(4).position(|x| x == b"iloc").unwrap() + 4;
    // iloc v1: full header (4), sizes (2), count (2), id (2), then the construction method
    bytes[iloc + 11] = 2;
    let f = parse(&bytes[..]).unwrap();
    assert!(matches!(f.read_item(&bytes[..], 1, 99), Err(Error::Unsupported(_))));
    bytes[iloc + 11] = 0;
    bytes[iloc + 13] = 1; // data_reference_index 1
    let f = parse(&bytes[..]).unwrap();
    assert!(matches!(f.read_item(&bytes[..], 1, 99), Err(Error::Unsupported(_))));
    // an item that is not in iloc has no data
    let mut h = Heif::new(b"heic");
    let mut it = Item::new(1, b"grid", vec![]);
    it.storage = Storage::Nowhere;
    h.item(it);
    let bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert!(f.item(1).unwrap().location.is_none());
    assert!(matches!(f.read_item(&bytes[..], 1, 99), Err(Error::Invalid(_))));
    assert!(matches!(f.read_item(&bytes[..], 7, 99), Err(Error::Invalid(_))));
    // the same item id twice
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"hvc1", vec![1])).item(Item::new(1, b"hvc1", vec![2]));
    assert!(matches!(parse(&h.build()[..]), Err(Error::Invalid(_))));
    // iloc version 3
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"hvc1", vec![1]));
    let mut bytes = h.build();
    let iloc = bytes.windows(4).position(|x| x == b"iloc").unwrap() + 4;
    bytes[iloc] = 3;
    assert!(matches!(parse(&bytes[..]), Err(Error::Unsupported(_))));
    // field sizes beyond 8 bytes
    bytes[iloc] = 1;
    bytes[iloc + 4] = 0x94;
    assert!(matches!(parse(&bytes[..]), Err(Error::Invalid(_))));
}

#[test]
fn extents_outside_the_file_or_idat_are_errors() {
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"hvc1", vec![1; 10]));
    h.item(Item::new(2, b"grid", vec![2; 10]).in_idat());
    let mut bytes = h.build();
    let f = parse(&bytes[..]).unwrap();
    assert!(f.read_item(&bytes[..], 1, 99).is_ok());
    // cut the file inside item 1's data
    bytes.truncate(bytes.len() - 4);
    assert!(f.read_item(&bytes[..], 1, 99).is_err());
    let mut g = f.clone();
    g.idat.truncate(4);
    assert!(matches!(g.read_item(&bytes[..], 2, 99), Err(Error::Truncated(_))));
    // offsets that overflow
    let edit = |change: &dyn Fn(&mut ItemLocation)| {
        let mut g = f.clone();
        change(g.items.iter_mut().find(|i| i.id == 1).unwrap().location.as_mut().unwrap());
        g
    };
    assert!(edit(&|l| l.base_offset = u64::MAX).read_item(&bytes[..], 1, 99).is_err());
    assert!(edit(&|l| l.extents[0].length = u64::MAX).read_item(&bytes[..], 1, u64::MAX).is_err());
}

/// iloc entries whose extents take no bytes (all field sizes 0) cannot make the parser allocate
/// without bound.
#[test]
fn extent_count_bomb_is_capped() {
    let mut iloc = w::full(1, 0);
    iloc.extend_from_slice(&[0x00, 0x00]);
    let items = 20u16;
    iloc.extend_from_slice(&items.to_be_bytes());
    for id in 0..items {
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&[0, 0, 0, 0]);
        iloc.extend_from_slice(&u16::MAX.to_be_bytes());
    }
    let mut meta = w::full(0, 0);
    meta.extend_from_slice(&w::boxed(b"iloc", &iloc));
    let mut file = w::boxed(b"ftyp", b"heic\0\0\0\0mif1");
    file.extend_from_slice(&w::boxed(b"meta", &meta));
    assert!(matches!(parse(&file[..]), Err(Error::TooLarge(_))));
}

#[test]
fn counts_larger_than_their_boxes_are_refused() {
    let wrap = |kind: &[u8; 4], payload: Vec<u8>| {
        let mut meta = w::full(0, 0);
        meta.extend_from_slice(&w::boxed(kind, &payload));
        let mut file = w::boxed(b"ftyp", b"heic\0\0\0\0");
        file.extend_from_slice(&w::boxed(b"meta", &meta));
        file
    };
    // iloc claiming 65535 items in a few bytes
    let mut iloc = w::full(1, 0);
    iloc.extend_from_slice(&[0x44, 0x00, 0xFF, 0xFF, 0, 1]);
    assert!(matches!(parse(&wrap(b"iloc", iloc)[..]), Err(Error::Truncated(_))));
    // ipma claiming 2^32-1 entries
    let mut ipma = w::full(0, 0);
    ipma.extend_from_slice(&u32::MAX.to_be_bytes());
    let iprp = w::boxed(b"ipma", &ipma);
    assert!(matches!(parse(&wrap(b"iprp", iprp)[..]), Err(Error::Truncated(_))));
    // an iref entry with more targets than bytes
    let mut iref = w::full(0, 0);
    iref.extend_from_slice(&w::boxed(b"dimg", &[0, 1, 0xFF, 0xFF, 0, 2]));
    assert!(matches!(parse(&wrap(b"iref", iref)[..]), Err(Error::Truncated(_))));
    // a meta box claiming more than the cap
    let mut big = w::boxed(b"ftyp", b"heic\0\0\0\0");
    big.extend_from_slice(&(1u32).to_be_bytes());
    big.extend_from_slice(b"meta");
    big.extend_from_slice(&(MAX_META_BYTES + 100).to_be_bytes());
    assert!(parse(&big[..]).is_err());
}

#[test]
fn too_many_items_are_refused() {
    let mut iinf = w::full(1, 0);
    iinf.extend_from_slice(&((MAX_ITEMS + 1) as u32).to_be_bytes());
    let mut infe = w::full(2, 0);
    infe.extend_from_slice(&[0, 1, 0, 0]);
    infe.extend_from_slice(b"hvc1\0");
    let one = w::boxed(b"infe", &infe);
    for _ in 0..=MAX_ITEMS {
        iinf.extend_from_slice(&one);
    }
    let mut meta = w::full(0, 0);
    meta.extend_from_slice(&w::boxed(b"iinf", &iinf));
    let mut file = w::boxed(b"ftyp", b"heic\0\0\0\0");
    file.extend_from_slice(&w::boxed(b"meta", &meta));
    assert!(matches!(parse(&file[..]), Err(Error::TooLarge(_))));
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Parse, then read every item and grid: whatever the bytes, nothing may panic.
fn exercise(bytes: &[u8]) {
    if let Ok(f) = parse(bytes) {
        for it in &f.items {
            if let Ok(d) = f.read_item(bytes, it.id, 1 << 24) {
                let _ = ImageGrid::parse(&d);
                let _ = exif_tiff(&d);
            }
            let _ = f.item_properties(it.id).count();
        }
        if let Some(p) = f.primary_item {
            let _ = f.references_from(p, b"dimg");
            let _ = f.references_to(p, b"auxl");
        }
    }
    let _ = sniff(bytes);
}

#[test]
fn truncated_mutated_and_garbage_input_never_panics() {
    let mut h = grid_file();
    h.iloc = IlocLayout { version: 2, offset_size: 8, length_size: 4, base_offset_size: 4, index_size: 4 };
    let files = [grid_file().build(), h.build()];
    let mut rng = Rng(0x5EED_4E1F);
    for f in &files {
        for n in 0..f.len() {
            assert!(std::panic::catch_unwind(|| exercise(&f[..n])).is_ok(), "truncated at {n}");
        }
        for round in 0..3000 {
            let mut m = f.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(m.len());
                match rng.below(3) {
                    0 => m[at] ^= 1 << rng.below(8),
                    1 => m[at] = rng.next() as u8,
                    _ => {
                        // a corrupt size or count: 0xFF.. or 0 over four bytes
                        let v = if rng.below(2) == 0 { 0xFF } else { 0 };
                        for b in m.iter_mut().skip(at).take(4) {
                            *b = v;
                        }
                    }
                }
            }
            assert!(std::panic::catch_unwind(|| exercise(&m)).is_ok(), "mutation round {round}");
        }
    }
    for len in [0usize, 1, 7, 8, 9, 64, 1000] {
        let garbage: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        assert!(std::panic::catch_unwind(|| exercise(&garbage)).is_ok());
    }
}
