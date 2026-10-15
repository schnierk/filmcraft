//! HEIF / AVIF decoding of pictures we know: ffmpeg (libx265, libaom-av1) codes a pattern made
//! here losslessly, `filmcraft_testkit::heif` wraps the streams as items, and the decoded image
//! must match the pattern (PSNR, which only Y'CbCr 4:2:0 rounding keeps from infinite) in every
//! layout: tile grids, rotation / mirror / clean aperture, 10-bit, colour signalled by `nclx` or
//! by the bitstream, alpha planes, AVIF. ffmpeg is a fixture generator only (AGENTS.md §2);
//! fixtures go to `target/fixtures/codecs/heif/`. Without ffmpeg or an encoder the tests skip.
//! With `FILMCRAFT_HEIF_DUMP=<dir>` the transform cases are also written there as `.heic` files, to
//! open with other readers.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use filmcraft_codecs::heif::{self, HeifCodec};
use filmcraft_media::MediaSource;
use filmcraft_testkit::heif::{self as w, Heif, Item};

/// ffmpeg with `encoder`, or `None` after reporting a skip.
fn ffmpeg_with(encoder: &str) -> Option<PathBuf> {
    let ff = filmcraft_testkit::ffmpeg_or_skip("heif oracle")?;
    let out = Command::new(&ff).args(["-hide_banner", "-encoders"]).stdin(Stdio::null()).output().ok()?;
    if !String::from_utf8_lossy(&out.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(encoder)) {
        filmcraft_testkit::skip("heif oracle", &format!("ffmpeg has no {encoder} encoder"));
        return None;
    }
    Some(ff)
}

/// The test picture (RGB): smooth colour gradients and two neutral grey marks (a lighter block at
/// the top left, a darker bar at the right). The marks change luma only, so 4:2:0 loses nothing
/// at their edges, and a wrong rotation, mirror, crop or tile position moves them.
fn pattern(w: usize, h: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64 / w as f64, y as f64 / h as f64);
            let mark = if x < w / 4 && y < h / 5 {
                45.0
            } else if x > w * 7 / 8 && y > h / 3 {
                -40.0
            } else {
                0.0
            };
            let rgb = [50.0 + 150.0 * fx, 50.0 + 150.0 * fy, 190.0 - 110.0 * fx * fy];
            v.extend(rgb.map(|c| (c + mark).round().clamp(0.0, 255.0) as u8));
        }
    }
    v
}

/// A grey alpha pattern: a ramp with a transparent hole.
fn alpha_pattern(w: usize, h: usize) -> Vec<u8> {
    (0..h).flat_map(|y| (0..w).map(move |x| if (w / 3..w / 2).contains(&x) && (h / 3..h / 2).contains(&y) { 0 } else { (40 + 200 * x / w) as u8 })).collect()
}

/// The `tw` × `th` block of a `w` × `h` image (`channels` per pixel) at (x0, y0), its edge
/// repeated where it runs past the image.
fn block(src: &[u8], w: usize, h: usize, channels: usize, x0: usize, y0: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(tw * th * channels);
    for y in 0..th {
        for x in 0..tw {
            let (sx, sy) = ((x0 + x).min(w - 1), (y0 + y).min(h - 1));
            v.extend_from_slice(&src[(sy * w + sx) * channels..(sy * w + sx + 1) * channels]);
        }
    }
    v
}

/// Code a raw picture with ffmpeg (cached under `name`).
fn encode(ff: &Path, name: &str, raw: &[u8], pix_fmt: &str, (w, h): (usize, usize), args: &[&str]) -> Vec<u8> {
    let out = filmcraft_testkit::fixtures_dir("codecs/heif").join(name);
    let made = filmcraft_testkit::fixtures::generate(&out, |tmp| {
        let input = tmp.with_extension("raw");
        if std::fs::write(&input, raw).is_err() {
            return false;
        }
        let size = format!("{w}x{h}");
        let st = Command::new(ff)
            .args(["-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-s", &size, "-i"])
            .arg(&input)
            .args(args)
            .arg(tmp)
            .stdin(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&input);
        matches!(st, Ok(s) if s.success())
    });
    std::fs::read(made.unwrap_or_else(|| panic!("ffmpeg could not make {name}"))).unwrap()
}

/// libx265 lossless, one picture, with the colour the VUI should signal.
fn x265(pix_fmt: &'static str, matrix: &'static str, range: &'static str, vui: &'static str) -> Vec<String> {
    let scale = format!("scale=out_color_matrix={matrix}:out_range={range},format={pix_fmt}");
    let params = format!("lossless=1:log-level=error:{vui}");
    ["-vf", &scale, "-c:v", "libx265", "-x265-params", &params, "-frames:v", "1", "-f", "hevc"].iter().map(|s| s.to_string()).collect()
}

const VUI_601_LIMITED: &str = "colorprim=bt709:transfer=iec61966-2-1:colormatrix=bt470bg:range=limited";
const VUI_709_FULL: &str = "colorprim=bt709:transfer=bt709:colormatrix=bt709:range=full";

fn hevc(ff: &Path, name: &str, rgb: &[u8], size: (usize, usize), args: &[String]) -> Vec<u8> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    encode(ff, name, rgb, "rgb24", size, &a)
}

fn psnr(rgba: &[u8], rgb: &[u8]) -> f64 {
    assert_eq!(rgba.len() / 4, rgb.len() / 3, "picture sizes differ");
    let mut se = 0f64;
    for (p, q) in rgba.chunks(4).zip(rgb.chunks(3)) {
        for k in 0..3 {
            let d = f64::from(p[k]) - f64::from(q[k]);
            se += d * d;
        }
    }
    let mse = se / (rgb.len() as f64);
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

/// An RGB image transformed like `heif` does: crop, then anti-clockwise quarter turns, then mirror
/// (`imir` mode 0 exchanges top and bottom, 1 left and right). Written independently of the decoder.
fn transformed(src: &[u8], w: usize, h: usize, crop: Option<(usize, usize, usize, usize)>, turns: u8, axis: Option<u8>) -> (usize, usize, Vec<u8>) {
    let (cx, cy, cw, ch) = crop.unwrap_or((0, 0, w, h));
    let get = |x: usize, y: usize| -> [u8; 3] {
        let i = ((cy + y) * w + cx + x) * 3;
        [src[i], src[i + 1], src[i + 2]]
    };
    let (ow, oh) = if turns % 2 == 1 { (ch, cw) } else { (cw, ch) };
    let mut out = Vec::with_capacity(ow * oh * 3);
    for y in 0..oh {
        for x in 0..ow {
            let (mut x1, mut y1) = (x, y);
            match axis {
                Some(0) => y1 = oh - 1 - y,
                Some(_) => x1 = ow - 1 - x,
                None => {}
            }
            // undo the rotation: where in the cropped picture this pixel came from
            let (sx, sy) = match turns % 4 {
                1 => (cw - 1 - y1, x1),
                2 => (cw - 1 - x1, ch - 1 - y1),
                3 => (y1, ch - 1 - x1),
                _ => (x1, y1),
            };
            out.extend_from_slice(&get(sx, sy));
        }
    }
    (ow, oh, out)
}

const W: usize = 200;
const H: usize = 136;
const TILE: usize = 64;

/// The pattern coded as a grid of 64 × 64 HEVC tiles (4 columns, 3 rows; the last ones cut) with
/// the grid in `idat` and every second tile in two extents.
fn hevc_grid(ff: &Path, h: &mut Heif) -> u32 {
    let src = pattern(W, H);
    let (cols, rows) = (W.div_ceil(TILE), H.div_ceil(TILE));
    let tile_size = h.property(w::ispe(TILE as u32, TILE as u32));
    let out_size = h.property(w::ispe(W as u32, H as u32));
    let pixi = h.property(w::pixi(&[8, 8, 8]));
    let mut ids = Vec::new();
    let mut config = None;
    for r in 0..rows {
        for c in 0..cols {
            let rgb = block(&src, W, H, 3, c * TILE, r * TILE, TILE, TILE);
            let stream = hevc(ff, &format!("grid_v1_{r}_{c}.hevc"), &rgb, (TILE, TILE), &x265("yuv420p", "bt601", "tv", VUI_601_LIMITED));
            let (hvcc, data) = w::hevc_item(&stream, 1, 8);
            let cfg = *config.get_or_insert_with(|| h.property(hvcc));
            let id = 1 + (r * cols + c) as u32;
            h.item(Item::new(id, b"hvc1", data).hidden().extents(1 + id as usize % 2).with(cfg, true).with(tile_size, false));
            ids.push(id);
        }
    }
    h.item(Item::new(100, b"grid", w::grid(rows as u16, cols as u16, W as u32, H as u32, false)).in_idat().with(out_size, false).with(pixi, false));
    h.reference(b"dimg", 100, &ids);
    h.primary = Some(100);
    100
}

#[test]
fn hevc_grid_matches_the_pattern() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let mut h = Heif::new(b"heic");
    hevc_grid(&ff, &mut h);
    let bytes = h.build();
    let img = heif::decode(&bytes).unwrap();
    assert_eq!((img.width, img.height), (W as u32, H as u32));
    let p = psnr(&img.rgba, &pattern(W, H));
    eprintln!("HEVC grid 4 × 3: PSNR {p:.2} dB");
    assert!(p >= 40.0, "PSNR {p:.2}");
    assert!(!img.has_alpha);
    let i = heif::info(&bytes).unwrap();
    assert_eq!((i.width, i.height, i.grid, i.codec, i.bit_depth), (W as u32, H as u32, Some((3, 4)), HeifCodec::Hevc, 8));
}

#[test]
fn transforms_apply_in_the_standard_order() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let (w0, h0) = (96usize, 64usize);
    let src = pattern(w0, h0);
    let stream = hevc(&ff, "single_v1_96x64.hevc", &src, (w0, h0), &x265("yuv420p", "bt601", "tv", VUI_601_LIMITED));
    // (irot, imir axis, clap)
    let cases: [(u8, Option<u8>, bool); 9] = [
        (0, None, false),
        (1, None, false),
        (2, None, false),
        (3, None, false),
        (0, Some(0), false),
        (0, Some(1), false),
        (0, None, true),
        (3, Some(0), true),
        (1, Some(1), true),
    ];
    for (turns, axis, clap) in cases {
        let mut h = Heif::new(b"heic");
        let (hvcc, data) = w::hevc_item(&stream, 1, 8);
        let c = h.property(hvcc);
        let s = h.property(w::ispe(w0 as u32, h0 as u32));
        let mut item = Item::new(1, b"hvc1", data).with(c, true).with(s, false);
        // clean aperture: 80 × 50, moved 3 left and 2 down from the centre → (5, 9)
        if clap {
            item = item.with(h.property(w::clap((80, 1), (50, 1), (-3, 1), (4, 2))), true);
        }
        if turns != 0 {
            item = item.with(h.property(w::irot(turns)), true);
        }
        if let Some(a) = axis {
            item = item.with(h.property(w::imir(a)), true);
        }
        h.item(item);
        h.primary = Some(1);
        let bytes = h.build();
        if let Some(dir) = std::env::var_os("FILMCRAFT_HEIF_DUMP") {
            let name = format!("transform_irot{turns}_imir{}_clap{}.heic", axis.map_or("-".to_string(), |a| a.to_string()), u8::from(clap));
            std::fs::write(Path::new(&dir).join(name), &bytes).unwrap();
        }
        let img = heif::decode(&bytes).unwrap();
        let (ow, oh, want) = transformed(&src, w0, h0, clap.then_some((5, 9, 80, 50)), turns, axis);
        assert_eq!((img.width as usize, img.height as usize), (ow, oh), "irot {turns} imir {axis:?} clap {clap}");
        let i = heif::info(&bytes).unwrap();
        assert_eq!((i.width as usize, i.height as usize, i.rotation, i.cropped), (ow, oh, turns, clap));
        let p = psnr(&img.rgba, &want);
        assert!(p >= 40.0, "irot {turns} imir {axis:?} clap {clap}: PSNR {p:.2}");
    }
}

/// An iPhone photo's layout: a grid turned a quarter clockwise (`irot` 3, portrait).
#[test]
fn rotated_grid_like_an_iphone_portrait() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let mut h = Heif::new(b"heic");
    let grid = hevc_grid(&ff, &mut h);
    let r = h.property(w::irot(3));
    let item = h.items.iter_mut().find(|i| i.id == grid).unwrap();
    item.properties.push((r, true));
    let img = heif::decode(&h.build()).unwrap();
    let (ow, oh, want) = transformed(&pattern(W, H), W, H, None, 3, None);
    assert_eq!((img.width as usize, img.height as usize), (ow, oh));
    assert_eq!((ow, oh), (H, W), "portrait");
    let p = psnr(&img.rgba, &want);
    assert!(p >= 40.0, "PSNR {p:.2}");
}

#[test]
fn ten_bit_full_range_from_the_vui_or_nclx() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let (w0, h0) = (96usize, 64usize);
    let src = pattern(w0, h0);
    let stream = hevc(&ff, "single_v1_10bit_full_709.hevc", &src, (w0, h0), &x265("yuv420p10le", "bt709", "pc", VUI_709_FULL));
    for nclx in [false, true] {
        let mut h = Heif::new(b"heic");
        let (hvcc, data) = w::hevc_item(&stream, 1, 10);
        let c = h.property(hvcc);
        let s = h.property(w::ispe(w0 as u32, h0 as u32));
        let mut item = Item::new(1, b"hvc1", data).with(c, true).with(s, false);
        if nclx {
            item = item.with(h.property(w::colr_nclx(1, 13, 1, true)), false);
        }
        h.item(item);
        h.primary = Some(1);
        let bytes = h.build();
        let img = heif::decode(&bytes).unwrap();
        let p = psnr(&img.rgba, &src);
        eprintln!("10-bit full range BT.709 (nclx {nclx}): PSNR {p:.2} dB");
        assert!(p >= 42.0, "nclx {nclx}: PSNR {p:.2}");
        assert_eq!(heif::info(&bytes).unwrap().bit_depth, 10);
    }
}

/// `nclx` wins over the bitstream: a stream whose VUI claims limited range but holds full-range
/// samples decodes right only with the file's `nclx`.
#[test]
fn nclx_overrides_the_bitstream_colour() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let (w0, h0) = (96usize, 64usize);
    let src = pattern(w0, h0);
    let stream = hevc(&ff, "single_v1_full_but_vui_limited.hevc", &src, (w0, h0), &x265("yuv420p", "bt601", "pc", VUI_601_LIMITED));
    let decode = |nclx: bool| {
        let mut h = Heif::new(b"heic");
        let (hvcc, data) = w::hevc_item(&stream, 1, 8);
        let c = h.property(hvcc);
        let mut item = Item::new(1, b"hvc1", data).with(c, true);
        if nclx {
            item = item.with(h.property(w::colr_nclx(1, 13, 6, true)), false);
        }
        h.item(item);
        h.primary = Some(1);
        psnr(&heif::decode(&h.build()).unwrap().rgba, &src)
    };
    let (with, without) = (decode(true), decode(false));
    eprintln!("full-range samples, VUI says limited: with nclx {with:.2} dB, without {without:.2} dB");
    assert!(with >= 40.0 && without < with - 10.0, "with nclx {with:.2}, without {without:.2}");
}

/// The alpha auxiliary image (both URNs), and a premultiplied one.
#[test]
fn alpha_planes_apply() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let (w0, h0) = (96usize, 64usize);
    let src = pattern(w0, h0);
    let alpha = alpha_pattern(w0, h0);
    let colour = hevc(&ff, "single_v1_96x64.hevc", &src, (w0, h0), &x265("yuv420p", "bt601", "tv", VUI_601_LIMITED));
    // the alpha plane as the luma of a 4:2:0 picture (neutral chroma), full range
    let grey_rgb: Vec<u8> = alpha.iter().flat_map(|&a| [a, a, a]).collect();
    let alpha_stream = hevc(&ff, "alpha_v1_96x64.hevc", &grey_rgb, (w0, h0), &x265("yuv420p", "bt601", "pc", "range=full"));
    for urn in heif::ALPHA_URNS {
        let mut h = Heif::new(b"heic");
        let (hvcc, data) = w::hevc_item(&colour, 1, 8);
        let c = h.property(hvcc);
        let s = h.property(w::ispe(w0 as u32, h0 as u32));
        h.item(Item::new(1, b"hvc1", data).with(c, true).with(s, false));
        let (ahvcc, adata) = w::hevc_item(&alpha_stream, 1, 8);
        let ac = h.property(ahvcc);
        let aux = h.property(w::auxc(urn, &[]));
        h.item(Item::new(2, b"hvc1", adata).hidden().with(ac, true).with(s, false).with(aux, true));
        h.reference(b"auxl", 2, &[1]);
        h.primary = Some(1);
        let bytes = h.build();
        let img = heif::decode(&bytes).unwrap();
        assert!(img.has_alpha && heif::info(&bytes).unwrap().has_alpha);
        let got: Vec<u8> = img.rgba.chunks(4).map(|p| p[3]).collect();
        let worst = got.iter().zip(&alpha).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(worst <= 1, "{urn}: alpha off by {worst}");
        assert!(psnr(&img.rgba, &src) >= 40.0);
        // a depth map is not alpha
        let mut h2 = h.clone();
        h2.properties[aux as usize - 1] = w::auxc("urn:mpeg:hevc:2015:auxid:2", &[]);
        assert!(!heif::decode(&h2.build()).unwrap().has_alpha);
    }
}

/// A monochrome (4:0:0) HEVC alpha plane, which our HEVC decoder does not take yet, leaves the
/// image opaque instead of failing it.
#[test]
fn undecodable_alpha_leaves_the_image_opaque() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let (w0, h0) = (96usize, 64usize);
    let src = pattern(w0, h0);
    let colour = hevc(&ff, "single_v1_96x64.hevc", &src, (w0, h0), &x265("yuv420p", "bt601", "tv", VUI_601_LIMITED));
    let alpha = encode(
        &ff,
        "alpha_v1_gray.hevc",
        &alpha_pattern(w0, h0),
        "gray",
        (w0, h0),
        &["-c:v", "libx265", "-x265-params", "lossless=1:log-level=error", "-frames:v", "1", "-f", "hevc"],
    );
    let mut h = Heif::new(b"heic");
    let (hvcc, data) = w::hevc_item(&colour, 1, 8);
    let c = h.property(hvcc);
    h.item(Item::new(1, b"hvc1", data).with(c, true));
    let (ahvcc, adata) = w::hevc_item(&alpha, 0, 8);
    let ac = h.property(ahvcc);
    let aux = h.property(w::auxc(heif::ALPHA_URNS[0], &[]));
    h.item(Item::new(2, b"hvc1", adata).hidden().with(ac, true).with(aux, true));
    h.reference(b"auxl", 2, &[1]);
    h.primary = Some(1);
    let img = heif::decode(&h.build()).unwrap();
    assert!(!img.has_alpha);
    assert!(psnr(&img.rgba, &src) >= 40.0);
}

/// AV1 OBUs as an `av01` item: the sequence header goes into `av1C` as well.
fn av1_item(stream: &[u8], monochrome: bool, ten_bit: bool) -> (Vec<u8>, Vec<u8>) {
    let (mut seq, mut data) = (Vec::new(), Vec::new());
    let mut pos = 0;
    while pos < stream.len() {
        let h = stream[pos];
        let (kind, ext, has_size) = ((h >> 3) & 15, h & 4 != 0, h & 2 != 0);
        let mut p = pos + 1 + usize::from(ext);
        assert!(has_size, "ffmpeg writes OBU sizes");
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let b = stream[p];
            p += 1;
            size |= usize::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        let obu = &stream[pos..p + size];
        if kind == 1 && seq.is_empty() {
            seq = obu.to_vec();
        }
        if kind != 2 {
            data.extend_from_slice(obu);
        }
        pos = p + size;
    }
    (w::av1c(0, ten_bit, monochrome, (!monochrome, !monochrome), &seq), data)
}

fn aom(pix_fmt: &str, range: &str) -> Vec<String> {
    let scale = format!("scale=out_color_matrix=bt601:out_range={range},format={pix_fmt}");
    let cr = if range == "pc" { "pc" } else { "tv" };
    [
        "-vf",
        &scale,
        "-c:v",
        "libaom-av1",
        "-aom-params",
        "lossless=1",
        "-cpu-used",
        "8",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "iec61966-2-1",
        "-colorspace",
        "bt470bg",
        "-color_range",
        cr,
        "-frames:v",
        "1",
        "-f",
        "obu",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn avif_with_alpha_matches_the_pattern() {
    let Some(ff) = ffmpeg_with("libaom-av1") else { return };
    let (w0, h0) = (96usize, 64usize);
    let src = pattern(w0, h0);
    let alpha = alpha_pattern(w0, h0);
    let a: Vec<String> = aom("yuv420p", "tv");
    let colour = encode(&ff, "avif_v1_96x64.obu", &src, "rgb24", (w0, h0), &a.iter().map(String::as_str).collect::<Vec<_>>());
    let g: Vec<String> = aom("gray", "pc");
    let alpha_stream = encode(&ff, "avif_v1_alpha.obu", &alpha, "gray", (w0, h0), &g.iter().map(String::as_str).collect::<Vec<_>>());
    let mut h = Heif::new(b"avif");
    let (c, data) = av1_item(&colour, false, false);
    let c = h.property(c);
    let s = h.property(w::ispe(w0 as u32, h0 as u32));
    h.item(Item::new(1, b"av01", data).with(c, true).with(s, false));
    let (ac, adata) = av1_item(&alpha_stream, true, false);
    let ac = h.property(ac);
    let aux = h.property(w::auxc(heif::ALPHA_URNS[0], &[]));
    h.item(Item::new(2, b"av01", adata).hidden().with(ac, true).with(s, false).with(aux, true));
    h.reference(b"auxl", 2, &[1]);
    h.primary = Some(1);
    let bytes = h.build();
    let img = heif::decode(&bytes).unwrap();
    let p = psnr(&img.rgba, &src);
    eprintln!("AVIF 8-bit 4:2:0: PSNR {p:.2} dB");
    assert!(p >= 40.0, "PSNR {p:.2}");
    let got: Vec<u8> = img.rgba.chunks(4).map(|p| p[3]).collect();
    assert!(got.iter().zip(&alpha).all(|(a, b)| a.abs_diff(*b) <= 1), "monochrome AV1 alpha");
    let i = heif::info(&bytes).unwrap();
    assert_eq!((i.codec, i.brand, i.has_alpha, i.codec.format()), (HeifCodec::Av1, filmcraft_isobmff::FourCc(*b"avif"), true, "AVIF"));
    // and as a still source
    let s = filmcraft_codecs::open_bytes("picture.avif", Arc::from(bytes)).unwrap();
    assert_eq!(s.info().video.as_ref().unwrap().codec, "AVIF");
    assert!(s.info().video.as_ref().unwrap().has_alpha);
}

#[test]
fn avif_grid() {
    let Some(ff) = ffmpeg_with("libaom-av1") else { return };
    let src = pattern(W, H);
    let a: Vec<String> = aom("yuv420p", "pc");
    let mut h = Heif::new(b"avif");
    let tile_size = h.property(w::ispe(128, 128));
    let mut ids = Vec::new();
    for r in 0..2 {
        for c in 0..2 {
            let rgb = block(&src, W, H, 3, c * 128, r * 128, 128, 128);
            let stream = encode(&ff, &format!("avif_grid_v1_{r}_{c}.obu"), &rgb, "rgb24", (128, 128), &a.iter().map(String::as_str).collect::<Vec<_>>());
            let (cfg, data) = av1_item(&stream, false, false);
            let cfg = h.property(cfg);
            let id = 1 + (r * 2 + c) as u32;
            h.item(Item::new(id, b"av01", data).hidden().with(cfg, true).with(tile_size, false));
            ids.push(id);
        }
    }
    h.item(Item::new(10, b"grid", w::grid(2, 2, W as u32, H as u32, true)).in_idat());
    h.reference(b"dimg", 10, &ids);
    h.primary = Some(10);
    let img = heif::decode(&h.build()).unwrap();
    let p = psnr(&img.rgba, &src);
    eprintln!("AVIF grid 2 × 2: PSNR {p:.2} dB");
    assert!(p >= 40.0, "PSNR {p:.2}");
}

/// Every way a file is opened ends at the HEIF decoder, never the MP4 demuxer.
#[test]
fn opens_as_a_still_through_every_route() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let mut h = Heif::new(b"heic");
    hevc_grid(&ff, &mut h);
    let mut exif = Item::new(300, b"Exif", w::exif(b"MM\0*\0\0\0\x08\0\0"));
    exif.hidden = true;
    h.item(exif);
    h.reference(b"cdsc", 300, &[100]);
    let bytes: Arc<[u8]> = Arc::from(h.build());
    assert!(filmcraft_codecs::heif::sniff(&bytes));
    assert!(!filmcraft_codecs::mp4::sniff(&bytes), "the MP4 opener must not take HEIF");
    let check = |s: &filmcraft_media::SharedSource| {
        let i = s.info();
        assert_eq!(i.kind, filmcraft_media::MediaKind::Still);
        let v = i.video.as_ref().unwrap();
        assert_eq!((v.width, v.height, v.codec.as_str()), (W as u32, H as u32, "HEIC"));
        let f = s.video_frame(filmcraft_media::FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap();
        assert_eq!((f.width, f.height), (W as u32, H as u32));
    };
    // the byte openers (CLI probe, web import), whatever the file is called
    for name in ["IMG_0001.HEIC", "photo.heif", "renamed.mp4"] {
        check(&filmcraft_codecs::open_bytes(name, bytes.clone()).unwrap());
    }
    // the reader openers (the engine's media pool, the Media Browser)
    let reader: filmcraft_media::SharedReader = Arc::new(filmcraft_media::reader::MemReader(bytes.clone()));
    check(&filmcraft_media::reader::open_reader("IMG_0001.HEIC", reader.clone(), &filmcraft_codecs::reader_openers(), &filmcraft_codecs::openers()).unwrap());
    check(
        &filmcraft_media::reader::open_reader_within("IMG_0001.HEIC", reader, &filmcraft_codecs::reader_openers(), &filmcraft_codecs::openers(), 1 << 20)
            .unwrap(),
    );
    // header-only metadata
    let i = heif::info(&bytes).unwrap();
    assert_eq!(i.exif.as_deref(), Some(&b"MM\0*\0\0\0\x08\0\0"[..]));
    // image sequences of HEIC frames decode through the still decoders handed to them
    let frames = vec![Some("a_0001.heic".to_string()), Some("a_0002.heic".to_string())];
    let b2 = bytes.clone();
    let loader: filmcraft_media::sequence::FrameLoader = Arc::new(move |_: &str| Ok(b2.to_vec()));
    let seq = filmcraft_media::sequence::ImageSequenceSource::with_decoders(
        "a_0001.heic",
        frames.clone(),
        filmcraft_time::FrameRate::FPS_25,
        loader.clone(),
        &filmcraft_codecs::still_decoders(),
    )
    .unwrap();
    assert_eq!(seq.info().video.as_ref().map(|v| (v.width, v.height)), Some((W as u32, H as u32)));
    assert!(filmcraft_media::sequence::ImageSequenceSource::new("a_0001.heic", frames, filmcraft_time::FrameRate::FPS_25, loader).is_err());
    assert!(filmcraft_media::is_importable(Path::new("IMG_0001.HEIC")));
}

/// A tile whose SPS declares a picture far larger than its `ispe` is refused before decoding.
#[test]
fn parameter_sets_larger_than_the_item_are_refused() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let src = pattern(W, H);
    let rgb = block(&src, W, H, 3, 0, 0, 512, 512);
    let stream = hevc(&ff, "big_v1_512.hevc", &rgb, (512, 512), &x265("yuv420p", "bt601", "tv", VUI_601_LIMITED));
    let mut h = Heif::new(b"heic");
    let (hvcc, data) = w::hevc_item(&stream, 1, 8);
    let c = h.property(hvcc);
    let s = h.property(w::ispe(16, 16));
    h.item(Item::new(1, b"hvc1", data).with(c, true).with(s, false));
    h.primary = Some(1);
    let e = heif::decode(&h.build()).err().unwrap().to_string();
    assert!(e.contains("larger than the item"), "{e}");
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

/// Bit flips, corrupt bytes and truncation anywhere in a real grid file: errors or pictures, never a
/// panic.
#[test]
fn damaged_files_never_panic() {
    let Some(ff) = ffmpeg_with("libx265") else { return };
    let mut h = Heif::new(b"heic");
    hevc_grid(&ff, &mut h);
    let good = h.build();
    let mut rng = Rng(0xBAD_F11E);
    for k in 0..40 {
        let n = good.len() * k / 40;
        assert!(std::panic::catch_unwind(|| heif::decode(&good[..n]).is_ok()).is_ok(), "truncated at {n}");
    }
    let mut decoded = 0;
    for round in 0..250 {
        let mut m = good.clone();
        for _ in 0..1 + rng.next() % 6 {
            let at = (rng.next() % m.len() as u64) as usize;
            m[at] = if rng.next().is_multiple_of(2) { m[at] ^ (1 << (rng.next() % 8)) } else { rng.next() as u8 };
        }
        match std::panic::catch_unwind(|| heif::decode(&m).map(|i| i.rgba.len())) {
            Ok(r) => decoded += usize::from(r.is_ok()),
            Err(_) => panic!("round {round} panicked"),
        }
    }
    eprintln!("{decoded} of 250 damaged files still decoded");
}
