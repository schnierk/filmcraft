//! HEIF decoding without an encoder: transforms, colour conversion and tile placement on synthetic
//! planes, and hostile container structures (built with `filmcraft_testkit::heif`) that must fail
//! cleanly before anything is decoded. The ffmpeg-made HEVC / AV1 cases are in
//! `tests/heif_oracle.rs`.

use super::*;
use filmcraft_testkit::heif::{self as w, Heif, Item};

fn img(wd: u32, ht: u32) -> Rgba {
    // pixel (x, y) = [x, y, x + 10 y, 255]
    let px = (0..ht).flat_map(|y| (0..wd).flat_map(move |x| [x as u8, y as u8, (x + 10 * y) as u8, 255])).collect();
    Rgba { w: wd, h: ht, px }
}

fn at(i: &Rgba, x: u32, y: u32) -> [u8; 4] {
    pixel(i, x as usize, y as usize)
}

#[test]
fn rotation_turns_anti_clockwise() {
    let src = img(3, 2);
    let r = rotate(&src, 1);
    assert_eq!((r.w, r.h), (2, 3));
    // anti-clockwise: the top-right corner comes to the top left, the top-left to the bottom left
    assert_eq!(at(&r, 0, 0), at(&src, 2, 0));
    assert_eq!(at(&r, 0, 2), at(&src, 0, 0));
    assert_eq!(at(&r, 1, 2), at(&src, 0, 1));
    let r3 = rotate(&src, 3);
    assert_eq!((r3.w, r3.h), (2, 3));
    // clockwise: the top-left corner goes to the top right
    assert_eq!(at(&r3, 1, 0), at(&src, 0, 0));
    assert_eq!(at(&r3, 0, 0), at(&src, 0, 1));
    let r2 = rotate(&src, 2);
    assert_eq!(at(&r2, 0, 0), at(&src, 2, 1));
    assert_eq!(rotate(&rotate(&src, 1), 3).px, src.px);
    assert_eq!(rotate(&r2, 2).px, src.px);
    assert_eq!(rotate(&src, 0).px, src.px);
}

#[test]
fn mirror_axes() {
    let src = img(3, 2);
    let lr = mirror(&src, Mirror::LeftRight);
    assert_eq!(at(&lr, 0, 0), at(&src, 2, 0));
    assert_eq!(at(&lr, 2, 1), at(&src, 0, 1));
    let tb = mirror(&src, Mirror::TopBottom);
    assert_eq!(at(&tb, 0, 0), at(&src, 0, 1));
    assert_eq!(mirror(&lr, Mirror::LeftRight).px, src.px);
}

#[test]
fn clean_aperture_rectangles() {
    let c = |w: u32, h: u32, dx: (i32, u32), dy: (i32, u32)| CleanAperture { width: (w, 1), height: (h, 1), horiz_offset: dx, vert_offset: dy };
    assert_eq!(clap_rect(c(80, 60, (0, 1), (0, 1)), 100, 90), Some((10, 15, 80, 60)));
    assert_eq!(clap_rect(c(80, 60, (-4, 1), (6, 2)), 100, 90), Some((6, 18, 80, 60)));
    // rational sizes round
    assert_eq!(clap_rect(CleanAperture { width: (161, 2), height: (60, 1), horiz_offset: (0, 1), vert_offset: (0, 1) }, 100, 90), Some((10, 15, 81, 60)));
    // the whole picture, nonsense and out of bounds: no crop
    assert_eq!(clap_rect(c(100, 90, (0, 1), (0, 1)), 100, 90), None);
    assert_eq!(clap_rect(c(80, 60, (0, 0), (0, 1)), 100, 90), None);
    assert_eq!(clap_rect(CleanAperture { width: (80, 0), ..c(80, 60, (0, 1), (0, 1)) }, 100, 90), None);
    assert_eq!(clap_rect(c(0, 60, (0, 1), (0, 1)), 100, 90), None);
    assert_eq!(clap_rect(c(120, 60, (0, 1), (0, 1)), 100, 90), None);
    assert_eq!(clap_rect(c(80, 60, (11, 1), (0, 1)), 100, 90), None);
    assert_eq!(clap_rect(c(80, 60, (i32::MIN, 1), (0, 1)), 100, 90), None);
    let t = Transforms { clap: Some(c(80, 60, (0, 1), (0, 1))), rotation: 1, mirror: Some(Mirror::TopBottom) };
    assert_eq!(t.display_size(100, 90), (60, 80));
    let out = apply_transforms(img(100, 90), t);
    assert_eq!((out.w, out.h), (60, 80));
    // crop (10, 15) 80 × 60, then turned anti-clockwise, then flipped top to bottom: the first
    // output row is the crop's left column read downwards
    assert_eq!(at(&out, 0, 0), at(&img(100, 90), 10, 15));
    assert_eq!(at(&out, 59, 0), at(&img(100, 90), 10, 74));
    assert_eq!(at(&out, 0, 79), at(&img(100, 90), 89, 15));
}

fn planes(width: u32, height: u32, bits: u8, chroma: Chroma, y: Vec<u16>, u: Vec<u16>, v: Vec<u16>) -> Planes {
    Planes { width, height, bits, chroma, y, u, v, signalled: None }
}

fn conv(code: u8, full_range: bool) -> Conversion {
    Conversion { matrix: MatrixKind::from_code(code).unwrap(), full_range, gamut: None }
}

/// Y'CbCr of an R'G'B' colour (0..1) with BT.601 / BT.709 coefficients.
fn ycc(rgb: [f64; 3], kr: f64, kb: f64, bits: u32, full: bool) -> [u16; 3] {
    let y = kr * rgb[0] + (1.0 - kr - kb) * rgb[1] + kb * rgb[2];
    let (cb, cr) = ((rgb[2] - y) / (2.0 * (1.0 - kb)), (rgb[0] - y) / (2.0 * (1.0 - kr)));
    let s = f64::from(1u32 << (bits - 8));
    let max = f64::from((1u32 << bits) - 1);
    let mid = f64::from(1u32 << (bits - 1));
    if full {
        [(y * max).round() as u16, (cb * max + mid).round() as u16, (cr * max + mid).round() as u16]
    } else {
        [((219.0 * y + 16.0) * s).round() as u16, ((224.0 * cb + 128.0) * s).round() as u16, ((224.0 * cr + 128.0) * s).round() as u16]
    }
}

#[test]
fn matrices_ranges_and_bit_depths_round_trip() {
    let colours = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.5, 0.5, 0.5], [0.9, 0.6, 0.2], [0.1, 0.3, 0.8], [1.0, 1.0, 1.0], [0.0, 0.0, 0.0]];
    for (code, kr, kb) in [(6u8, 0.299, 0.114), (1, 0.2126, 0.0722), (9, 0.2627, 0.0593)] {
        for bits in [8u8, 10, 12] {
            for full in [true, false] {
                for c in colours {
                    let [y, u, v] = ycc(c, kr, kb, bits.into(), full);
                    let p = planes(1, 1, bits, Chroma::C444, vec![y], vec![u], vec![v]);
                    let out = to_rgba(&p, &conv(code, full), None).unwrap();
                    // 8-bit Y'CbCr quantisation alone moves a channel by up to 1.5 codes
                    let tolerance = if bits == 8 { 2 } else { 1 };
                    for k in 0..3 {
                        let want = (c[k] * 255.0).round() as i32;
                        assert!((i32::from(out.px[k]) - want).abs() <= tolerance, "matrix {code} {bits}-bit full {full} colour {c:?}: {:?}", &out.px[..3]);
                    }
                    assert_eq!(out.px[3], 255);
                }
            }
        }
    }
    // limited range: black and white at the code points the standard fixes, clipping beyond
    let p = planes(3, 1, 10, Chroma::C444, vec![64, 940, 1023], vec![512; 3], vec![512; 3]);
    let out = to_rgba(&p, &conv(1, false), None).unwrap();
    assert_eq!([out.px[0], out.px[4], out.px[8]], [0, 255, 255]);
}

#[test]
fn identity_matrix_and_monochrome() {
    // identity (RGB coding): the planes hold G, B, R
    let p = planes(1, 1, 8, Chroma::C444, vec![200], vec![50], vec![10]);
    assert_eq!(&to_rgba(&p, &conv(0, true), None).unwrap().px, &[10, 200, 50, 255]);
    let p = planes(2, 1, 8, Chroma::Mono, vec![0, 128], vec![], vec![]);
    assert_eq!(&to_rgba(&p, &conv(6, true), None).unwrap().px, &[0, 0, 0, 255, 128, 128, 128, 255]);
}

#[test]
fn chroma_samples_cover_their_luma_samples() {
    // a 5 × 3 picture, every chroma sample different; full-range Cb only (Cr neutral) so blue
    // shows which chroma sample each pixel took
    for (chroma, cw, ch) in [(Chroma::C420, 3, 2), (Chroma::C422, 3, 3), (Chroma::C444, 5, 3)] {
        let u: Vec<u16> = (0..cw * ch).map(|i| 128 + 10 * i as u16).collect();
        let p = planes(5, 3, 8, chroma, vec![0; 15], u.clone(), vec![128; cw * ch]);
        let out = to_rgba(&p, &conv(6, true), None).unwrap();
        let (sx, sy) = chroma.shifts();
        for yy in 0..3usize {
            for xx in 0..5usize {
                let k = (yy >> sy) * cw + (xx >> sx);
                let want = ((f32::from(u[k]) - 128.0) / 255.0 * 1.772 * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                assert_eq!(out.px[(yy * 5 + xx) * 4 + 2], want, "{chroma:?} pixel {xx},{yy}");
            }
        }
    }
}

#[test]
fn alpha_planes() {
    let colour = planes(2, 2, 8, Chroma::C444, vec![200; 4], vec![128; 4], vec![128; 4]);
    let alpha = planes(2, 2, 8, Chroma::Mono, vec![0, 64, 128, 255], vec![], vec![]);
    let out = to_rgba(&colour, &conv(6, true), Some(Alpha { planes: &alpha, full_range: true, premultiplied: false })).unwrap();
    assert_eq!(out.px.chunks(4).map(|p| p[3]).collect::<Vec<_>>(), vec![0, 64, 128, 255]);
    assert!(out.px.chunks(4).all(|p| p[0] == 200));
    // limited-range alpha
    let alpha = planes(2, 2, 8, Chroma::Mono, vec![16, 235, 0, 255], vec![], vec![]);
    let out = to_rgba(&colour, &conv(6, true), Some(Alpha { planes: &alpha, full_range: false, premultiplied: false })).unwrap();
    assert_eq!(out.px.chunks(4).map(|p| p[3]).collect::<Vec<_>>(), vec![0, 255, 0, 255]);
    // premultiplied colour is divided by alpha (and 0 where alpha is 0)
    let colour = planes(2, 1, 8, Chroma::C444, vec![100, 100], vec![128; 2], vec![128; 2]);
    let alpha = planes(2, 1, 8, Chroma::Mono, vec![0, 128], vec![], vec![]);
    let out = to_rgba(&colour, &conv(6, true), Some(Alpha { planes: &alpha, full_range: true, premultiplied: true })).unwrap();
    assert_eq!(&out.px[..4], &[0, 0, 0, 0]);
    assert_eq!(out.px[4], 199);
    assert_eq!(out.px[7], 128);
    // an alpha plane of another size is sampled to fit
    let colour = planes(4, 2, 8, Chroma::C444, vec![50; 8], vec![128; 8], vec![128; 8]);
    let alpha = planes(2, 1, 8, Chroma::Mono, vec![10, 20], vec![], vec![]);
    let out = to_rgba(&colour, &conv(6, true), Some(Alpha { planes: &alpha, full_range: true, premultiplied: false })).unwrap();
    assert_eq!(out.px.chunks(4).map(|p| p[3]).collect::<Vec<_>>(), vec![10, 10, 20, 20, 10, 10, 20, 20]);
}

/// The exact sRGB → linear → 3×3 → sRGB conversion of one 8-bit colour.
fn exact_gamut(rgb: [u8; 3], m: &[[f64; 3]; 3]) -> [u8; 3] {
    let lin = rgb.map(|v| f64::from(filmcraft_color::srgb_to_linear(f32::from(v) / 255.0)));
    std::array::from_fn(|i| {
        let l = (m[i][0] * lin[0] + m[i][1] * lin[1] + m[i][2] * lin[2]).clamp(0.0, 1.0);
        (f64::from(filmcraft_color::linear_to_srgb(l as f32)) * 255.0).round() as u8
    })
}

#[test]
fn wide_gamut_pictures_convert_to_bt709_in_linear_light() {
    let m = gamut_matrix(Gamut::P3D65, Gamut::Bt709);
    let mut c = conv(0, true);
    c.gamut = Some(to_f32(&m));
    let mut worst = 0;
    for r in (0..=255).step_by(15) {
        for g in (0..=255).step_by(15) {
            for b in (0..=255).step_by(15) {
                let p = planes(1, 1, 8, Chroma::C444, vec![g], vec![b], vec![r]);
                let out = to_rgba(&p, &c, None).unwrap();
                let want = exact_gamut([r as u8, g as u8, b as u8], &m);
                for k in 0..3 {
                    worst = worst.max((i32::from(out.px[k]) - i32::from(want[k])).abs());
                }
            }
        }
    }
    assert!(worst <= 1, "worst difference {worst}");
    // grey stays grey, saturated P3 green lies outside BT.709 and clips
    let p = planes(1, 1, 8, Chroma::C444, vec![118], vec![118], vec![118]);
    let out = to_rgba(&p, &c, None).unwrap();
    assert!(out.px[..3].iter().all(|&v| v.abs_diff(118) <= 1));
    let p = planes(1, 1, 8, Chroma::C444, vec![255], vec![0], vec![0]);
    let out = to_rgba(&p, &c, None).unwrap();
    assert_eq!((out.px[0], out.px[1], out.px[2]), (0, 255, 0));
}

#[test]
fn tiles_are_placed_row_by_row_and_cut_to_the_output() {
    // 2 × 2 tiles of 4 × 4 (4:2:0) on a 7 × 6 output
    let tile = |k: u16| planes(4, 4, 8, Chroma::C420, vec![10 * k; 16], vec![100 + k; 4], vec![200 + k; 4]);
    let tiles: Vec<Planes> = (1..=4).map(tile).collect();
    let g = ImageGrid { rows: 2, columns: 2, output_width: 7, output_height: 6 };
    let out = assemble(&tiles, g, (4, 4)).unwrap();
    assert!(out.consistent());
    assert_eq!((out.width, out.height, out.u.len()), (7, 6, 4 * 3));
    let luma = |x: usize, y: usize| out.y[y * 7 + x];
    assert_eq!([luma(0, 0), luma(3, 3), luma(4, 0), luma(6, 3), luma(0, 4), luma(6, 5)], [10, 10, 20, 20, 30, 40]);
    let cb = |x: usize, y: usize| out.u[y * 4 + x];
    assert_eq!([cb(0, 0), cb(1, 1), cb(2, 0), cb(3, 1), cb(0, 2), cb(3, 2)], [101, 101, 102, 102, 103, 104]);
    assert_eq!(out.v[2 * 4 + 3], 204);
}

// ---- hostile structures: refused before anything is decoded ----

/// One HEVC item with a fake hvcC (no decoder ever runs in these tests).
fn hevc_file(ispe: (u32, u32)) -> Heif {
    let mut h = Heif::new(b"heic");
    let c = h.property(w::hvcc(4, 1, 8, &[], &[], &[]));
    let s = h.property(w::ispe(ispe.0, ispe.1));
    h.item(Item::new(1, b"hvc1", vec![0, 0, 0, 2, 0x26, 0x01]).with(c, true).with(s, false));
    h.primary = Some(1);
    h
}

fn grid_file(rows: u16, cols: u16, out: (u32, u32), tile: (u32, u32), tiles: usize) -> Heif {
    let mut h = Heif::new(b"heic");
    let c = h.property(w::hvcc(4, 1, 8, &[], &[], &[]));
    let s = h.property(w::ispe(tile.0, tile.1));
    let o = h.property(w::ispe(out.0, out.1));
    h.item(Item::new(100, b"grid", w::grid(rows, cols, out.0, out.1, false)).in_idat().with(o, false));
    for t in 0..tiles as u32 {
        h.item(Item::new(t + 1, b"hvc1", vec![0, 0, 0, 2, 0x26, 0x01]).hidden().with(c, true).with(s, false));
    }
    let ids: Vec<u32> = (1..=tiles as u32).collect();
    h.reference(b"dimg", 100, &ids);
    h.primary = Some(100);
    h
}

fn err(bytes: &[u8]) -> String {
    match decode(bytes) {
        Ok(_) => panic!("decoded"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn info_reads_the_container_only() {
    let mut h = grid_file(2, 3, (300, 200), (128, 128), 6);
    let rot = h.property(w::irot(3));
    let mir = h.property(w::imir(0));
    let clap = h.property(w::clap((250, 1), (180, 1), (0, 1), (0, 1)));
    let icc = h.property(w::colr_icc(b"prof", b"not really a profile"));
    h.items[0].properties.extend([(clap, true), (rot, true), (mir, false), (icc, false)]);
    let mut exif = Item::new(200, b"Exif", w::exif(b"II*\0\x08\0\0\0"));
    exif.hidden = true;
    h.item(exif);
    h.reference(b"cdsc", 200, &[100]);
    let i = info(&h.build()).unwrap();
    assert_eq!((i.width, i.height, i.coded_width, i.coded_height), (180, 250, 300, 200));
    assert_eq!((i.rotation, i.mirror, i.cropped, i.grid), (3, Some(Mirror::TopBottom), true, Some((2, 3))));
    assert_eq!((i.codec, i.brand, i.bit_depth, i.has_alpha), (HeifCodec::Hevc, FourCc(*b"heic"), 8, false));
    assert_eq!(i.icc_profile.as_deref(), Some(&b"not really a profile"[..]));
    assert_eq!(i.exif.as_deref(), Some(&b"II*\0\x08\0\0\0"[..]));
    assert_eq!((i.codec.name(), i.codec.format()), ("HEVC", "HEIC"));
    // the same from a byte source (a library scan reads headers, not whole files)
    let bytes = h.build();
    let from = info_from(&bytes).unwrap();
    assert_eq!((from.width, from.height, from.exif.as_deref()), (180, 250, Some(&b"II*\0\x08\0\0\0"[..])));
}

#[test]
fn grid_structure_is_checked_before_decoding() {
    // more and fewer tiles than cells
    assert!(err(&grid_file(2, 2, (200, 200), (128, 128), 5).build()).contains("tiles for"));
    assert!(err(&grid_file(2, 2, (200, 200), (128, 128), 3).build()).contains("tiles for"));
    // tiles that do not cover the output, and a column wholly outside it
    assert!(err(&grid_file(2, 2, (300, 200), (128, 128), 4).build()).contains("do not fit"));
    assert!(err(&grid_file(1, 3, (200, 100), (128, 128), 3).build()).contains("do not fit"));
    // a canvas beyond the pixel cap is refused, not allocated
    assert!(err(&grid_file(1, 1, (60_000, 60_000), (60_000, 60_000), 1).build()).contains("larger than"));
    assert!(err(&grid_file(2, 2, (60_000, 30_000), (30_000, 15_000), 4).build()).contains("larger than"));
    // a tile of another size
    let mut h = grid_file(1, 2, (200, 100), (128, 128), 2);
    let odd = h.property(w::ispe(64, 64));
    h.items[2].properties[1] = (odd, false);
    assert!(err(&h.build()).contains("is not 128x128"));
    // a tile with its own rotation
    let mut h = grid_file(1, 2, (200, 100), (128, 128), 2);
    let r = h.property(w::irot(1));
    h.items[1].properties.push((r, false));
    assert!(err(&h.build()).contains("own transforms"));
    // a tile that is not a coded image (the grid itself)
    let mut h = grid_file(1, 2, (200, 100), (128, 128), 2);
    h.references[0].2 = vec![1, 100];
    let e = err(&h.build());
    assert!(e.contains("tile of itself") || e.contains("not a coded image"), "{e}");
    // tiles of two codecs
    let mut h = grid_file(1, 2, (200, 100), (128, 128), 2);
    let a = h.property(w::av1c(0, false, false, (true, true), &[]));
    h.items[2].kind = *b"av01";
    h.items[2].properties[0] = (a, true);
    assert!(err(&h.build()).contains("mixes HEVC and AV1"));
    // an image that would be larger than the cap
    assert!(err(&hevc_file((70_000, 70_000)).build()).contains("larger than"));
    assert!(err(&hevc_file((0, 10)).build()).contains("image size"));
}

#[test]
fn derived_image_cycles_and_depth_are_bounded() {
    // iden 1 → iden 2 → iden 1
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"iden", vec![]).in_idat());
    h.item(Item::new(2, b"iden", vec![]).in_idat());
    h.reference(b"dimg", 1, &[2]).reference(b"dimg", 2, &[1]);
    h.primary = Some(1);
    let bytes = h.build();
    assert!(err(&bytes).contains("derives from itself"));
    assert!(info(&bytes).is_err());
    // an iden chain longer than MAX_DEPTH
    let mut h = Heif::new(b"heic");
    let n = MAX_DEPTH as u32 + 3;
    for id in 1..=n {
        h.item(Item::new(id, b"iden", vec![]).in_idat());
        h.reference(b"dimg", id, &[id + 1]);
    }
    let c = h.property(w::hvcc(4, 1, 8, &[], &[], &[]));
    h.item(Item::new(n + 1, b"hvc1", vec![1]).with(c, true));
    h.primary = Some(1);
    let bytes = h.build();
    assert!(err(&bytes).contains("nested more than"));
    assert!(info(&bytes).is_err());
    // a grid that is its own tile
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"grid", w::grid(1, 1, 8, 8, false)).in_idat());
    h.reference(b"dimg", 1, &[1]);
    h.primary = Some(1);
    assert!(decode(&h.build()).is_err());
    // an iden with two inputs
    let mut h = hevc_file((8, 8));
    h.item(Item::new(9, b"iden", vec![]).in_idat());
    h.reference(b"dimg", 9, &[1, 1]);
    h.primary = Some(9);
    assert!(err(&h.build()).contains("inputs"));
}

#[test]
fn properties_and_items_we_cannot_handle_are_refused() {
    // an essential property we do not know (a layer selector)
    let mut h = hevc_file((8, 8));
    let p = h.property(w::boxed(b"lsel", &[0, 1]));
    h.items[0].properties.push((p, true));
    assert!(err(&h.build()).contains("essential property 'lsel'"));
    // the same, not essential: ignored (the decoder then fails on the fake data instead)
    h.items[0].properties.last_mut().unwrap().1 = false;
    assert!(!err(&h.build()).contains("lsel"));
    // an essential link to nothing
    let mut h = hevc_file((8, 8));
    h.items[0].properties.push((40, true));
    assert!(err(&h.build()).contains("does not exist"));
    // no decoder configuration
    let mut h = hevc_file((8, 8));
    h.items[0].properties.remove(0);
    assert!(err(&h.build()).contains("no decoder configuration"));
    // a protected item
    let mut bytes = hevc_file((8, 8)).build();
    let infe = bytes.windows(4).position(|x| x == b"infe").unwrap() + 4;
    bytes[infe + 7] = 1; // item_protection_index (after the full-box header and the 16-bit id)
    assert!(err(&bytes).contains("protected"));
    // an overlay, an uncompressed image, JPEG-in-HEIF: not supported
    for kind in [b"iovl", b"unci", b"jpeg"] {
        let mut h = Heif::new(b"mif1");
        h.item(Item::new(1, kind, vec![0; 8]).in_idat());
        h.primary = Some(1);
        assert!(matches!(decode(&h.build()), Err(CodecError::Unsupported(_))), "{kind:?}");
    }
    // not an image: another handler, no primary item, a primary item that is not there
    let mut h = hevc_file((8, 8));
    h.handler = *b"vide";
    assert!(matches!(decode(&h.build()), Err(CodecError::Unsupported(_))));
    let mut h = hevc_file((8, 8));
    h.primary = None;
    assert!(err(&h.build()).contains("no primary item"));
    let mut h = hevc_file((8, 8));
    h.primary = Some(77);
    assert!(err(&h.build()).contains("not described"));
}

#[test]
fn damaged_coded_data_is_an_error() {
    // a fake hvcC with no parameter sets and garbage slice data
    assert!(decode(&hevc_file((8, 8)).build()).is_err());
    // an hvcC whose SPS is garbage
    let mut h = Heif::new(b"heic");
    let c = h.property(w::hvcc(4, 1, 8, &[], &[&[0x42, 0x01, 0xFF, 0xFF, 0xFF]], &[]));
    h.item(Item::new(1, b"hvc1", vec![0, 0, 0, 3, 0x26, 0x01, 0xAF]).with(c, true));
    h.primary = Some(1);
    assert!(decode(&h.build()).is_err());
    assert!(info(&h.build()).is_err(), "no ispe and no usable SPS");
    // AV1 garbage
    let mut h = Heif::new(b"avif");
    let c = h.property(w::av1c(0, false, false, (true, true), &[]));
    let s = h.property(w::ispe(16, 16));
    h.item(Item::new(1, b"av01", vec![0x12, 0x00, 0x0A, 0x05, 1, 2, 3, 4, 5]).with(c, true).with(s, false));
    h.primary = Some(1);
    let bytes = h.build();
    assert!(decode(&bytes).is_err());
    assert_eq!(info(&bytes).unwrap().codec, HeifCodec::Av1);
}

/// Extents that all cover the same bytes cannot make a tiny file read more than its pixels need.
#[test]
fn overlapping_extents_cannot_inflate_an_item() {
    let bytes = hevc_file((16, 16)).build();
    let mut file = heif::parse(&bytes[..]).unwrap();
    let loc = file.items.iter_mut().find(|i| i.id == 1).unwrap().location.as_mut().unwrap();
    loc.extents = vec![loc.extents[0]; 50_000];
    let ctx = Ctx { file: &file, src: &bytes[..] };
    let e = ctx.decode_planes(1, &[]).err().unwrap().to_string();
    assert!(e.contains("size limit"), "{e}");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn mutated_structures_never_panic() {
    let mut files = vec![grid_file(2, 3, (300, 200), (128, 128), 6).build(), hevc_file((16, 16)).build()];
    let mut h = Heif::new(b"heic");
    h.item(Item::new(1, b"iden", vec![]).in_idat());
    h.item(Item::new(2, b"tmap", vec![]).in_idat());
    h.reference(b"dimg", 1, &[2]).reference(b"dimg", 2, &[1]);
    h.primary = Some(1);
    files.push(h.build());
    let mut rng = Rng(0xF00D_CAFE);
    for f in &files {
        for n in 0..f.len() {
            assert!(std::panic::catch_unwind(|| (info(&f[..n]).is_ok(), decode(&f[..n]).is_ok())).is_ok(), "truncated at {n}");
        }
        for round in 0..1500 {
            let mut m = f.clone();
            for _ in 0..1 + rng.next() % 4 {
                let at = (rng.next() % m.len() as u64) as usize;
                m[at] = if rng.next().is_multiple_of(2) { m[at] ^ (1 << (rng.next() % 8)) } else { rng.next() as u8 };
            }
            assert!(std::panic::catch_unwind(|| (info(&m).is_ok(), decode(&m).is_ok())).is_ok(), "round {round}");
        }
    }
}
