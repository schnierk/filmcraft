//! ICC profiles (ICC.1:2022, ISO 15076-1): just enough to recognise an RGB matrix/TRC display
//! profile — which primaries its colorant tags describe and whether its tone curves are the sRGB
//! curve — so a still that embeds one (Display P3 in iPhone photos) can be converted to the working
//! space with [`crate::spaces::gamut_matrix`].
//!
//! Read: the header (`acsp` signature, `RGB ` data colour space, `XYZ ` connection space) and the
//! `rXYZ` / `gXYZ` / `bXYZ` (XYZType) and `rTRC` / `gTRC` / `bTRC` (curveType: identity, gamma or
//! table; parametricCurveType: function types 0–4) tags. Profile colorants are relative to the D50
//! connection space, adapted from the device white with the linear Bradford transform (ICC.1
//! Annex E); [`RgbProfile::gamut`] compares them with our gamuts adapted the same way. Lookup-table
//! profiles (`A2B0`…), grey profiles and Lab connection spaces are not interpreted.

use crate::Primaries;
use crate::spaces::{D65, Gamut, Mat3, inverse, mul, mul_vec};

/// The D50 connection-space illuminant (ICC.1 §7.2.16).
pub const PCS_D50: [f64; 3] = [0.9642, 1.0, 0.8249];

/// Linear Bradford cone response matrix (ICC.1 Annex E).
const BRADFORD: Mat3 = [[0.8951, 0.2664, -0.1614], [-0.7502, 1.7135, 0.0367], [0.0389, -0.0685, 1.0296]];

/// Largest difference of one colorant component from a gamut's for [`RgbProfile::gamut`]
/// (s15Fixed16 steps are 1.5·10⁻⁵; the BT.709 and P3 colorants differ by 0.09).
const COLORANT_TOLERANCE: f64 = 0.002;

/// One tone reproduction curve.
#[derive(Clone, Debug, PartialEq)]
pub enum Curve {
    /// `curv` with no entries.
    Identity,
    /// `curv` with one entry: y = x^gamma.
    Gamma(f64),
    /// `curv` table: equally spaced samples of 0..1 (as 0..65535).
    Table(Vec<u16>),
    /// `para` function type 0–4 with its parameters (g, a, b, c, d, e, f; unused ones zero).
    Parametric { function: u16, params: [f64; 7] },
}

impl Curve {
    /// Device value (0..1) → linear connection-space value.
    pub fn eval(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Curve::Identity => x,
            Curve::Gamma(g) => x.powf(*g),
            Curve::Table(t) => match t.len() {
                0 => x,
                1 => f64::from(t[0]) / 65535.0,
                n => {
                    let pos = x * (n - 1) as f64;
                    let i = (pos.floor() as usize).min(n - 2);
                    let f = pos - i as f64;
                    let (a, b) = (f64::from(t[i]), f64::from(t[i + 1]));
                    (a + (b - a) * f) / 65535.0
                }
            },
            Curve::Parametric { function, params: [g, a, b, c, d, e, f] } => {
                let pow = |v: f64| if v > 0.0 { v.powf(*g) } else { 0.0 };
                match function {
                    0 => pow(x),
                    1 => {
                        if *a != 0.0 && x >= -b / a {
                            pow(a * x + b)
                        } else {
                            0.0
                        }
                    }
                    2 => {
                        if *a != 0.0 && x >= -b / a {
                            pow(a * x + b) + c
                        } else {
                            *c
                        }
                    }
                    3 => {
                        if x >= *d {
                            pow(a * x + b)
                        } else {
                            c * x
                        }
                    }
                    _ => {
                        if x >= *d {
                            pow(a * x + b) + e
                        } else {
                            c * x + f
                        }
                    }
                }
            }
        }
    }

    /// Whether the curve follows the sRGB curve (IEC 61966-2-1) within 1/500 everywhere.
    pub fn is_srgb(&self) -> bool {
        (0..=64).map(|i| f64::from(i) / 64.0).all(|x| (self.eval(x) - f64::from(crate::srgb_to_linear(x as f32))).abs() <= 0.002)
    }
}

/// The parts of an RGB matrix/TRC profile that say what its numbers mean.
#[derive(Clone, Debug, PartialEq)]
pub struct RgbProfile {
    /// The colorant tags as columns: linear device RGB → connection-space XYZ (D50).
    pub to_xyz: Mat3,
    /// Red, green and blue tone curves.
    pub curves: [Curve; 3],
}

impl RgbProfile {
    /// Parse a profile; `None` when it is not an RGB matrix/TRC profile with an XYZ connection
    /// space, or is damaged.
    pub fn parse(profile: &[u8]) -> Option<RgbProfile> {
        if profile.len() < 132 || profile.get(36..40)? != b"acsp" || profile.get(16..20)? != b"RGB " || profile.get(20..24)? != b"XYZ " {
            return None;
        }
        let count = usize::try_from(be32(profile, 128)?).ok()?;
        // 12 bytes per tag entry, all inside the profile
        if count.checked_mul(12)?.checked_add(132)? > profile.len() {
            return None;
        }
        let tag = |sig: &[u8; 4]| -> Option<&[u8]> {
            (0..count).find_map(|k| {
                let e = 132 + 12 * k;
                if profile.get(e..e + 4)? != sig {
                    return None;
                }
                let (off, len) = (usize::try_from(be32(profile, e + 4)?).ok()?, usize::try_from(be32(profile, e + 8)?).ok()?);
                profile.get(off..off.checked_add(len)?)
            })
        };
        let xyz = |sig: &[u8; 4]| -> Option<[f64; 3]> {
            let t = tag(sig)?;
            if t.get(0..4)? != b"XYZ " {
                return None;
            }
            Some([s15f16(t, 8)?, s15f16(t, 12)?, s15f16(t, 16)?])
        };
        let (r, g, b) = (xyz(b"rXYZ")?, xyz(b"gXYZ")?, xyz(b"bXYZ")?);
        let to_xyz = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        let curves = [curve(tag(b"rTRC")?)?, curve(tag(b"gTRC")?)?, curve(tag(b"bTRC")?)?];
        Some(RgbProfile { to_xyz, curves })
    }

    /// The gamut ([`Gamut::Bt709`], [`Gamut::P3D65`] or [`Gamut::Bt2020`]) whose primaries, adapted
    /// from D65 to the D50 connection space as profiles store them, match the colorants.
    pub fn gamut(&self) -> Option<Gamut> {
        let adapt = bradford(xy_to_xyz(D65), PCS_D50)?;
        [Gamut::Bt709, Gamut::P3D65, Gamut::Bt2020].into_iter().find(|g| {
            let want = mul(&adapt, &g.to_xyz());
            want.iter().flatten().zip(self.to_xyz.iter().flatten()).all(|(a, b)| (a - b).abs() <= COLORANT_TOLERANCE)
        })
    }

    /// Whether all three tone curves are the sRGB curve.
    pub fn has_srgb_curves(&self) -> bool {
        self.curves.iter().all(Curve::is_srgb)
    }
}

/// The primaries of an RGB display profile with sRGB tone curves (Display P3, sRGB, BT.2020
/// with the sRGB curve), so its pixels can be treated as sRGB-encoded values in those primaries.
/// `None` for any other profile.
pub fn srgb_curve_primaries(profile: &[u8]) -> Option<Primaries> {
    let p = RgbProfile::parse(profile)?;
    if !p.has_srgb_curves() {
        return None;
    }
    Some(match p.gamut()? {
        Gamut::P3D65 => Primaries::P3D65,
        Gamut::Bt2020 => Primaries::Bt2020,
        _ => Primaries::Bt709,
    })
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn s15f16(b: &[u8], at: usize) -> Option<f64> {
    Some(f64::from(be32(b, at)? as i32) / 65536.0)
}

fn curve(t: &[u8]) -> Option<Curve> {
    match t.get(0..4)? {
        b"curv" => {
            let n = usize::try_from(be32(t, 8)?).ok()?;
            match n {
                0 => Some(Curve::Identity),
                1 => Some(Curve::Gamma(f64::from(u16::from_be_bytes(t.get(12..14)?.try_into().ok()?)) / 256.0)),
                _ => {
                    let body = t.get(12..12usize.checked_add(n.checked_mul(2)?)?)?;
                    Some(Curve::Table(body.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect()))
                }
            }
        }
        b"para" => {
            let function = u16::from_be_bytes(t.get(8..10)?.try_into().ok()?);
            let n = match function {
                0 => 1,
                1 => 3,
                2 => 4,
                3 => 5,
                4 => 7,
                _ => return None,
            };
            let mut params = [0.0; 7];
            for (k, p) in params.iter_mut().take(n).enumerate() {
                *p = s15f16(t, 12 + 4 * k)?;
            }
            Some(Curve::Parametric { function, params })
        }
        _ => None,
    }
}

fn xy_to_xyz(xy: [f64; 2]) -> [f64; 3] {
    [xy[0] / xy[1], 1.0, (1.0 - xy[0] - xy[1]) / xy[1]]
}

/// Linear Bradford adaptation from white `src` to white `dst` (XYZ).
fn bradford(src: [f64; 3], dst: [f64; 3]) -> Option<Mat3> {
    let (s, d) = (mul_vec(&BRADFORD, src), mul_vec(&BRADFORD, dst));
    if s.iter().any(|v| v.abs() < 1e-12) {
        return None;
    }
    let scale = [[d[0] / s[0], 0.0, 0.0], [0.0, d[1] / s[1], 0.0], [0.0, 0.0, d[2] / s[2]]];
    Some(mul(&inverse(&BRADFORD), &mul(&scale, &BRADFORD)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A matrix/TRC profile for `gamut` (colorants adapted to D50 like real profiles) with the given
    /// curve tag, built from scratch.
    fn profile(gamut: Gamut, trc: &[u8]) -> Vec<u8> {
        let adapt = bradford(xy_to_xyz(D65), PCS_D50).unwrap();
        let m = mul(&adapt, &gamut.to_xyz());
        let fixed = |v: f64| ((v * 65536.0).round() as i32).to_be_bytes();
        let xyz_tag = |c: usize| {
            let mut t = b"XYZ \0\0\0\0".to_vec();
            for row in &m {
                t.extend_from_slice(&fixed(row[c]));
            }
            t
        };
        let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"rXYZ", xyz_tag(0)),
            (b"gXYZ", xyz_tag(1)),
            (b"bXYZ", xyz_tag(2)),
            (b"rTRC", trc.to_vec()),
            (b"gTRC", trc.to_vec()),
            (b"bTRC", trc.to_vec()),
        ];
        let mut header = vec![0u8; 128];
        header[12..16].copy_from_slice(b"mntr");
        header[16..20].copy_from_slice(b"RGB ");
        header[20..24].copy_from_slice(b"XYZ ");
        header[36..40].copy_from_slice(b"acsp");
        let mut table = (tags.len() as u32).to_be_bytes().to_vec();
        let mut data = Vec::new();
        let base = 128 + 4 + 12 * tags.len();
        for (sig, body) in &tags {
            table.extend_from_slice(*sig);
            table.extend_from_slice(&((base + data.len()) as u32).to_be_bytes());
            table.extend_from_slice(&(body.len() as u32).to_be_bytes());
            data.extend_from_slice(body);
        }
        let mut out = header;
        out.extend_from_slice(&table);
        out.extend_from_slice(&data);
        let size = (out.len() as u32).to_be_bytes();
        out[0..4].copy_from_slice(&size);
        out
    }

    fn para(function: u16, params: &[f64]) -> Vec<u8> {
        let mut t = b"para\0\0\0\0".to_vec();
        t.extend_from_slice(&function.to_be_bytes());
        t.extend_from_slice(&[0, 0]);
        for p in params {
            t.extend_from_slice(&((p * 65536.0).round() as i32).to_be_bytes());
        }
        t
    }

    fn srgb_para() -> Vec<u8> {
        para(3, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
    }

    #[test]
    fn recognises_display_p3_srgb_and_bt2020() {
        assert_eq!(srgb_curve_primaries(&profile(Gamut::P3D65, &srgb_para())), Some(Primaries::P3D65));
        assert_eq!(srgb_curve_primaries(&profile(Gamut::Bt709, &srgb_para())), Some(Primaries::Bt709));
        assert_eq!(srgb_curve_primaries(&profile(Gamut::Bt2020, &srgb_para())), Some(Primaries::Bt2020));
        // a camera gamut is none of them
        assert_eq!(RgbProfile::parse(&profile(Gamut::SGamut3, &srgb_para())).unwrap().gamut(), None);
    }

    /// Apple's Display P3 colorants (as s15Fixed16 values in the profiles iPhones embed) are the
    /// SMPTE EG 432-1 primaries adapted to D50.
    #[test]
    fn apple_display_p3_colorants_match() {
        let apple = [[0.515_121_46, 0.291_976_93, 0.157_104_49], [0.241_195_68, 0.692_245_48, 0.066_574_1], [-0.001_052_86, 0.041_885_38, 0.784_072_88]];
        let p = RgbProfile { to_xyz: apple, curves: [Curve::Identity, Curve::Identity, Curve::Identity] };
        assert_eq!(p.gamut(), Some(Gamut::P3D65));
    }

    #[test]
    fn curves() {
        let srgb = curve(&srgb_para()).unwrap();
        assert!(srgb.is_srgb());
        assert!((srgb.eval(0.5) - 0.214_041).abs() < 1e-4);
        // gamma 2.2 (Adobe RGB style) and linear are not sRGB
        let mut g22 = b"curv\0\0\0\0\0\0\0\x01".to_vec();
        g22.extend_from_slice(&(563u16).to_be_bytes());
        let g = curve(&g22).unwrap();
        assert_eq!(g, Curve::Gamma(563.0 / 256.0));
        assert!(!g.is_srgb());
        assert!(!curve(b"curv\0\0\0\0\0\0\0\0").unwrap().is_srgb());
        assert_eq!(srgb_curve_primaries(&profile(Gamut::P3D65, &g22)), None);
        // a 1024-entry sRGB table is sRGB
        let mut table = b"curv\0\0\0\0".to_vec();
        table.extend_from_slice(&1024u32.to_be_bytes());
        for i in 0..1024 {
            table.extend_from_slice(&((crate::srgb_to_linear(i as f32 / 1023.0) * 65535.0).round() as u16).to_be_bytes());
        }
        assert!(curve(&table).unwrap().is_srgb());
        // the other parametric forms
        assert!((curve(&para(0, &[2.0])).unwrap().eval(0.5) - 0.25).abs() < 1e-4);
        assert!((curve(&para(1, &[1.0, 2.0, -0.5])).unwrap().eval(0.2) - 0.0).abs() < 1e-9);
        assert!((curve(&para(2, &[1.0, 2.0, -0.5, 0.1])).unwrap().eval(0.5) - 0.6).abs() < 1e-4);
        assert!((curve(&para(4, &[1.0, 1.0, 0.0, 0.5, 0.5, 0.1, 0.05])).unwrap().eval(0.25) - 0.175).abs() < 1e-4);
        assert!(curve(&para(5, &[1.0])).is_none());
    }

    #[test]
    fn damaged_and_other_profiles_are_refused() {
        let good = profile(Gamut::P3D65, &srgb_para());
        for n in 0..good.len() {
            assert!(std::panic::catch_unwind(|| srgb_curve_primaries(&good[..n])).is_ok());
        }
        assert_eq!(srgb_curve_primaries(&good[..good.len() - 1]), None, "a tag running past the end");
        let mut grey = good.clone();
        grey[16..20].copy_from_slice(b"GRAY");
        assert_eq!(srgb_curve_primaries(&grey), None);
        let mut lab = good.clone();
        lab[20..24].copy_from_slice(b"Lab ");
        assert_eq!(srgb_curve_primaries(&lab), None);
        let mut huge = good.clone();
        huge[128..132].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(srgb_curve_primaries(&huge), None);
        // every byte flipped somewhere: no panic
        for i in 0..good.len() {
            let mut m = good.clone();
            m[i] ^= 0xA5;
            assert!(std::panic::catch_unwind(|| srgb_curve_primaries(&m)).is_ok());
        }
    }
}
