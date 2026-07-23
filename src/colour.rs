use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Colour space identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColourSpace {
    /// Rec. 709 / sRGB
    Rec709,
    /// DCI-P3
    P3,
    /// CIE XYZ (digital cinema)
    Xyz,
    /// Rec. 2020
    Rec2020,
    /// ACES (AP0)
    Aces,
    /// ACEScg (AP1)
    AcesCg,
    /// Alexa LogC
    LogC,
}

/// Colour conversion options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColourConvertOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub source_space: ColourSpace,
    pub target_space: ColourSpace,
    /// Optional 3D LUT path for custom transform
    pub lut_path: Option<PathBuf>,
}

/// Convert colour space of an image or sequence using ffmpeg.
///
/// ffmpeg's `colorspace` filter only models the video primaries/transfer sets:
/// Rec.709, DCI-P3 and Rec.2020. XYZ (DCDM), ACES, ACEScg and LogC are not
/// expressible there, so mapping them to bt709 gave silently wrong colour. Those
/// spaces now require a 3D LUT (`lut_path`); without one the conversion is
/// rejected. For the Rec.709 to DCI X'Y'Z' transform use `rgb_to_xyz_inplace` /
/// the `dcdm` module, which implement it correctly.
pub fn convert_colour(opts: &ColourConvertOptions) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-y").arg("-i").arg(&opts.input);

    // If a custom LUT is provided, use it for any pair of spaces.
    if let Some(ref lut) = opts.lut_path {
        cmd.arg("-vf").arg(format!("lut3d={}", lut.display()));
    } else {
        // No LUT: only spaces the colorspace filter models are honest here.
        let (colorspace, primaries, trc) = ffmpeg_color_params(opts.target_space)
            .ok_or_else(|| unsupported_err(opts.target_space))?;
        let (in_colorspace, in_primaries, in_trc) = ffmpeg_color_params(opts.source_space)
            .ok_or_else(|| unsupported_err(opts.source_space))?;

        let filter = format!(
            "colorspace=all={colorspace}:iall={in_colorspace}:iprimaries={in_primaries}:itrc={in_trc}:primaries={primaries}:trc={trc}"
        );
        cmd.arg("-vf").arg(filter);
    }

    cmd.arg(&opts.output);

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "ffmpeg colour conversion failed: {stderr}"
        )));
    }
    Ok(())
}

fn unsupported_err(cs: ColourSpace) -> std::io::Error {
    std::io::Error::other(format!(
        "{cs:?} has no ffmpeg colorspace model; supply a 3D LUT (lut_path), or use \
         the dcdm module for X'Y'Z'"
    ))
}

/// ffmpeg colorspace params (matrix, primaries, transfer) for the spaces the
/// filter can model, or None for spaces that need a LUT instead.
fn ffmpeg_color_params(cs: ColourSpace) -> Option<(&'static str, &'static str, &'static str)> {
    match cs {
        ColourSpace::Rec709 => Some(("bt709", "bt709", "bt709")),
        ColourSpace::P3 => Some(("bt709", "smpte431", "bt709")),
        ColourSpace::Rec2020 => Some(("bt2020ncl", "bt2020", "bt2020-10")),
        // XYZ/ACES/ACEScg/LogC are not colorspace-filter expressible.
        ColourSpace::Xyz | ColourSpace::Aces | ColourSpace::AcesCg | ColourSpace::LogC => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffmpeg_color_params() {
        let (cs, p, t) = ffmpeg_color_params(ColourSpace::Rec709).unwrap();
        assert_eq!(cs, "bt709");
        assert_eq!(p, "bt709");
        assert_eq!(t, "bt709");
    }

    #[test]
    fn wide_gamut_and_log_spaces_have_no_ffmpeg_model() {
        for cs in [
            ColourSpace::Xyz,
            ColourSpace::Aces,
            ColourSpace::AcesCg,
            ColourSpace::LogC,
        ] {
            assert!(ffmpeg_color_params(cs).is_none(), "{cs:?} must need a LUT");
        }
    }

    #[test]
    fn convert_rejects_unsupported_space_without_lut() {
        let opts = ColourConvertOptions {
            input: "in.tif".into(),
            output: "out.tif".into(),
            source_space: ColourSpace::Rec709,
            target_space: ColourSpace::Aces,
            lut_path: None,
        };
        let err = convert_colour(&opts).unwrap_err();
        assert!(err.to_string().contains("LUT"), "{err}");
    }
}

// ─── In-memory RGB → X'Y'Z' transform for DCI ─────────────────────────────

// DCI companding (SMPTE 428-1 / RP 431-2): 48 cd/m² reference white over the
// 52.37 encoding peak. libdcp/DoM (rgb_xyz.cc) and grok fold this into the
// matrix so diffuse white lands below full code, not at 4095.
const DCI_COEFFICIENT: f64 = 48.0 / 52.37;

// Display-referred Rec.709 linearization: pure gamma 2.2. This matches libdcp's
// rec709_to_xyz and grok's applyXYZTransform (the DoM-parity reference), not the
// camera OETF inverse. The OETF inverse is for scene-referred capture; DCP
// content is display-referred, so gamma 2.2 is the mastering convention here.
fn rec709_to_linear(v: f64) -> f64 {
    if v <= 0.0 { 0.0 } else { v.powf(2.2) }
}

// DCI 2.6 gamma: linear → X'Y'Z' (gamma-encoded)
fn linear_to_dci_gamma(v: f64) -> f64 {
    if v <= 0.0 { 0.0 } else { v.powf(1.0 / 2.6) }
}

// Rec.709/D65 RGB → CIE XYZ, pre-multiplied by the DCI companding coefficient.
const C: f32 = DCI_COEFFICIENT as f32;
const M00: f32 = 0.4124564 * C;
const M01: f32 = 0.3575761 * C;
const M02: f32 = 0.1804375 * C;
const M10: f32 = 0.2126729 * C;
const M11: f32 = 0.7151522 * C;
const M12: f32 = 0.0721750 * C;
const M20: f32 = 0.0193339 * C;
const M21: f32 = 0.119192 * C;
const M22: f32 = 0.9503041 * C;

/// Transform a 16-bit big-endian RGB frame buffer to X'Y'Z' (DCI) in-place.
///
/// Assumes `buf` contains pixels as [R_hi, R_lo, G_hi, G_lo, B_hi, B_lo, ...]
/// (rgb48be format from ffmpeg). Each sample is 16-bit unsigned big-endian.
///
/// The transform pipeline (libdcp/DoM and grok parity):
/// 1. Rec.709 gamma 2.2 (display-referred → linear)
/// 2. 3×3 matrix (linear Rec.709/D65 RGB → linear CIE XYZ), pre-multiplied by
///    the DCI companding coefficient (48/52.37)
/// 3. DCI 2.6 gamma (linear → X'Y'Z')
///
/// Output overwrites `buf` in the same rgb48be layout.
pub fn rgb_to_xyz_inplace(buf: &mut [u8]) {
    // Pre-compute LUTs to avoid per-pixel powf calls
    // Linearization LUT: u16 Rec.709 → f32 linear (256 KB)
    let lin_lut: Vec<f32> = (0..=65535u32)
        .map(|v| rec709_to_linear(v as f64 / 65535.0) as f32)
        .collect();

    // DCI gamma LUT: u16 linear → u16 gamma-encoded (128 KB)
    let gamma_lut: Vec<u16> = (0..=65535u32)
        .map(|v| {
            let g = linear_to_dci_gamma(v as f64 / 65535.0);
            (g.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16
        })
        .collect();

    let pixel_count = buf.len() / 6;

    for i in 0..pixel_count {
        let off = i * 6;

        // Read 16-bit big-endian samples
        let r_raw = ((buf[off] as u16) << 8) | (buf[off + 1] as u16);
        let g_raw = ((buf[off + 2] as u16) << 8) | (buf[off + 3] as u16);
        let b_raw = ((buf[off + 4] as u16) << 8) | (buf[off + 5] as u16);

        // Step 1: Linearize via LUT (no powf)
        let r_lin = lin_lut[r_raw as usize];
        let g_lin = lin_lut[g_raw as usize];
        let b_lin = lin_lut[b_raw as usize];

        // Step 2: 3×3 matrix multiply (linear RGB → linear XYZ), with DCI companding folded in
        let x_lin = M00 * r_lin + M01 * g_lin + M02 * b_lin;
        let y_lin = M10 * r_lin + M11 * g_lin + M12 * b_lin;
        let z_lin = M20 * r_lin + M21 * g_lin + M22 * b_lin;

        // Step 3: Quantize and apply DCI 2.6 gamma via LUT (no powf)
        let x16 = gamma_lut[(x_lin.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize];
        let y16 = gamma_lut[(y_lin.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize];
        let z16 = gamma_lut[(z_lin.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize];

        // Write back as big-endian
        buf[off] = (x16 >> 8) as u8;
        buf[off + 1] = x16 as u8;
        buf[off + 2] = (y16 >> 8) as u8;
        buf[off + 3] = y16 as u8;
        buf[off + 4] = (z16 >> 8) as u8;
        buf[off + 5] = z16 as u8;
    }
}

// ─── Display transform: DCI X'Y'Z' code values → sRGB ─────────────────────
//
// Inverse of the DCDM encode in `dcdm.rs`, for showing a real DCP picture
// (12-bit CIE X'Y'Z', DCI white, 2.6 gamma per SMPTE 428-1) correctly on an
// sRGB monitor. Pipeline, per SMPTE 428-1 decode + a Bradford illuminant
// adaptation the encode side never applied:
//   code/4095 → ^2.6 (peak-relative linear XYZ)
//   × 52.37/48 (Y = 1 at the DCI reference white)
//   Bradford-adapt DCI white → D65
//   XYZ(D65) → linear sRGB
//   sRGB OETF → 8-bit.

/// Rendering intent for the optional ICC display path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderingIntent {
    #[default]
    RelativeColorimetric,
    AbsoluteColorimetric,
    Perceptual,
    Saturation,
}

const DCDM_DECODE_GAMMA: f32 = 2.6;
const MAX_CODE_12BIT: f32 = 4095.0;
/// SMPTE 428-1 peak luminance the encoding normalises against (cd/m²).
const DCI_PEAK_LUMINANCE: f32 = 52.37;
/// DCI reference white luminance (cd/m²).
const DCI_REFERENCE_WHITE: f32 = 48.0;

// Bradford cone-response matrix and its inverse.
const BRADFORD: [[f32; 3]; 3] = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];
const BRADFORD_INV: [[f32; 3]; 3] = [
    [0.9869929, -0.1470543, 0.1599627],
    [0.4323053, 0.5183603, 0.0492912],
    [-0.0085287, 0.0400428, 0.9684867],
];
// CIE XYZ (D65) → linear sRGB.
const XYZ_D65_TO_SRGB: [[f32; 3]; 3] = [
    [3.240454, -1.537139, -0.498531],
    [-0.969266, 1.876011, 0.041556],
    [0.055643, -0.204026, 1.057225],
];
// White points as XYZ with Y = 1.
// DCI white x=0.314 y=0.351, D65 x=0.3127 y=0.3290.
const DCI_WHITE_XYZ: [f32; 3] = [0.894_587, 1.0, 0.954_416];
const D65_WHITE_XYZ: [f32; 3] = [0.950_456, 1.0, 1.088_754];

fn mat_vec(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn mat_mul(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut r = [[0.0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                r[i][j] += a[i][k] * b[k][j];
            }
        }
    }
    r
}

/// Bradford chromatic adaptation from `src` white to `dst` white (both XYZ, Y=1).
fn bradford(src: [f32; 3], dst: [f32; 3]) -> [[f32; 3]; 3] {
    let rs = mat_vec(&BRADFORD, src);
    let rd = mat_vec(&BRADFORD, dst);
    let diag = [
        [rd[0] / rs[0], 0.0, 0.0],
        [0.0, rd[1] / rs[1], 0.0],
        [0.0, 0.0, rd[2] / rs[2]],
    ];
    mat_mul(&BRADFORD_INV, &mat_mul(&diag, &BRADFORD))
}

fn srgb_oetf(u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    if u <= 0.0031308 {
        12.92 * u
    } else {
        1.055 * u.powf(1.0 / 2.4) - 0.055
    }
}

/// Precomputed DCI X'Y'Z' → sRGB display transform.
///
/// Built once, then applied per pixel via LUTs (no per-pixel `powf`).
pub struct XyzToSrgb {
    /// code (0..=4095) → peak-relative linear component (^2.6)
    expand: Vec<f32>,
    /// linear XYZ (rel, Y=1 at ref white) → linear sRGB, folds the Y-scale
    mat: [[f32; 3]; 3],
    /// quantised linear sRGB → 8-bit sRGB (OETF)
    oetf: Vec<u8>,
}

impl Default for XyzToSrgb {
    fn default() -> Self {
        Self::new()
    }
}

impl XyzToSrgb {
    pub fn new() -> Self {
        let scale = DCI_PEAK_LUMINANCE / DCI_REFERENCE_WHITE;
        let cat = bradford(DCI_WHITE_XYZ, D65_WHITE_XYZ);
        let mut mat = mat_mul(&XYZ_D65_TO_SRGB, &cat);
        for row in mat.iter_mut() {
            for c in row.iter_mut() {
                *c *= scale;
            }
        }
        let expand = (0..=4095u32)
            .map(|c| (c as f32 / MAX_CODE_12BIT).powf(DCDM_DECODE_GAMMA))
            .collect();
        let oetf = (0..=4095u32)
            .map(|i| (srgb_oetf(i as f32 / 4095.0) * 255.0 + 0.5) as u8)
            .collect();
        Self { expand, mat, oetf }
    }

    /// One pixel from 12-bit X'Y'Z' code values to 8-bit sRGB.
    pub fn pixel(&self, x: u16, y: u16, z: u16) -> [u8; 3] {
        let xyz = [
            self.expand[x.min(4095) as usize],
            self.expand[y.min(4095) as usize],
            self.expand[z.min(4095) as usize],
        ];
        let rgb = mat_vec(&self.mat, xyz);
        [
            self.oetf[(rgb[0].clamp(0.0, 1.0) * 4095.0) as usize],
            self.oetf[(rgb[1].clamp(0.0, 1.0) * 4095.0) as usize],
            self.oetf[(rgb[2].clamp(0.0, 1.0) * 4095.0) as usize],
        ]
    }

    /// Transform an `xyz12le` rawvideo frame into packed 8-bit sRGB.
    ///
    /// ffmpeg's `xyz12le` puts each 12-bit code in the high bits of a 16-bit
    /// little-endian sample (value = code << 4), so we shift back to the code.
    pub fn frame_xyz12le_to_srgb8(&self, raw: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.reserve(raw.len() / 2);
        for px in raw.chunks_exact(6) {
            let x = u16::from_le_bytes([px[0], px[1]]) >> 4;
            let y = u16::from_le_bytes([px[2], px[3]]) >> 4;
            let z = u16::from_le_bytes([px[4], px[5]]) >> 4;
            out.extend_from_slice(&self.pixel(x, y, z));
        }
    }
}

#[cfg(feature = "icc")]
mod icc {
    use super::{DCDM_DECODE_GAMMA, RenderingIntent, bradford, mat_vec};
    use super::{DCI_PEAK_LUMINANCE, DCI_REFERENCE_WHITE, DCI_WHITE_XYZ, MAX_CODE_12BIT};
    use lcms2::{Intent, PixelFormat, Profile, Transform};

    // D50 PCS white (lcms2's XYZ profile connection space).
    const D50_WHITE_XYZ: [f32; 3] = [0.964_212, 1.0, 0.825_188];

    /// DCI X'Y'Z' → device RGB through a monitor ICC profile.
    ///
    /// Decodes to peak-relative linear XYZ, adapts DCI white → the D50 PCS,
    /// then runs the ICC engine (littleCMS) into the profile's 8-bit RGB.
    pub struct XyzToIcc {
        expand: Vec<f32>,
        to_pcs: [[f32; 3]; 3],
        transform: Transform<[f32; 3], [u8; 3]>,
    }

    fn map_intent(i: RenderingIntent) -> Intent {
        match i {
            RenderingIntent::RelativeColorimetric => Intent::RelativeColorimetric,
            RenderingIntent::AbsoluteColorimetric => Intent::AbsoluteColorimetric,
            RenderingIntent::Perceptual => Intent::Perceptual,
            RenderingIntent::Saturation => Intent::Saturation,
        }
    }

    impl XyzToIcc {
        pub fn new(icc_path: &std::path::Path, intent: RenderingIntent) -> Result<Self, String> {
            let device = Profile::new_file(icc_path)
                .map_err(|e| format!("failed to load ICC profile: {e}"))?;
            let pcs = Profile::new_xyz();
            let transform = Transform::new(
                &pcs,
                PixelFormat::XYZ_FLT,
                &device,
                PixelFormat::RGB_8,
                map_intent(intent),
            )
            .map_err(|e| format!("failed to build ICC transform: {e}"))?;

            let scale = DCI_PEAK_LUMINANCE / DCI_REFERENCE_WHITE;
            let mut to_pcs = bradford(DCI_WHITE_XYZ, D50_WHITE_XYZ);
            for row in to_pcs.iter_mut() {
                for c in row.iter_mut() {
                    *c *= scale;
                }
            }
            let expand = (0..=4095u32)
                .map(|c| (c as f32 / MAX_CODE_12BIT).powf(DCDM_DECODE_GAMMA))
                .collect();
            Ok(Self {
                expand,
                to_pcs,
                transform,
            })
        }

        /// Transform an `xyz12le` rawvideo frame into packed 8-bit device RGB.
        pub fn frame_xyz12le_to_rgb8(&self, raw: &[u8], out: &mut Vec<u8>) {
            let n = raw.len() / 6;
            let mut pcs: Vec<[f32; 3]> = Vec::with_capacity(n);
            for px in raw.chunks_exact(6) {
                let x = (u16::from_le_bytes([px[0], px[1]]) >> 4).min(4095) as usize;
                let y = (u16::from_le_bytes([px[2], px[3]]) >> 4).min(4095) as usize;
                let z = (u16::from_le_bytes([px[4], px[5]]) >> 4).min(4095) as usize;
                pcs.push(mat_vec(
                    &self.to_pcs,
                    [self.expand[x], self.expand[y], self.expand[z]],
                ));
            }
            let mut rgb = vec![[0u8; 3]; n];
            self.transform.transform_pixels(&pcs, &mut rgb);
            out.clear();
            out.reserve(n * 3);
            for p in rgb {
                out.extend_from_slice(&p);
            }
        }
    }
}

#[cfg(feature = "icc")]
pub use icc::XyzToIcc;

#[cfg(test)]
mod tests_display {
    use super::*;

    // DCI reference white as 12-bit X'Y'Z' code values.
    fn dci_white_codes() -> [u16; 3] {
        let mut c = [0u16; 3];
        for i in 0..3 {
            let peak = DCI_WHITE_XYZ[i] * DCI_REFERENCE_WHITE / DCI_PEAK_LUMINANCE;
            c[i] = (peak.powf(1.0 / DCDM_DECODE_GAMMA) * MAX_CODE_12BIT).round() as u16;
        }
        c
    }

    #[test]
    fn black_maps_to_black() {
        let t = XyzToSrgb::new();
        assert_eq!(t.pixel(0, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn dci_reference_white_maps_to_srgb_white() {
        let t = XyzToSrgb::new();
        let [x, y, z] = dci_white_codes();
        let rgb = t.pixel(x, y, z);
        // neutral and near full-scale
        for c in rgb {
            assert!(c >= 253, "channel {c} not near 255 for DCI white {rgb:?}");
        }
    }

    #[test]
    fn mid_grey_is_neutral_and_between_black_and_white() {
        let t = XyzToSrgb::new();
        // DCI-white chromaticity at 18% of reference-white luminance
        let mut codes = [0u16; 3];
        for i in 0..3 {
            let peak = DCI_WHITE_XYZ[i] * 0.18 * DCI_REFERENCE_WHITE / DCI_PEAK_LUMINANCE;
            codes[i] = (peak.powf(1.0 / DCDM_DECODE_GAMMA) * MAX_CODE_12BIT).round() as u16;
        }
        let rgb = t.pixel(codes[0], codes[1], codes[2]);
        let max = *rgb.iter().max().unwrap();
        let min = *rgb.iter().min().unwrap();
        assert!(max - min <= 2, "mid grey not neutral: {rgb:?}");
        assert!(
            min > 0 && max < 255,
            "mid grey not between black and white: {rgb:?}"
        );
    }

    #[test]
    fn transform_is_monotonic_on_the_neutral_axis() {
        let t = XyzToSrgb::new();
        let mut prev = 0u8;
        for step in 0..=8u16 {
            let code = step * 500;
            let g = t.pixel(code, code, code)[1];
            assert!(
                g >= prev,
                "green not monotonic at code {code}: {g} < {prev}"
            );
            prev = g;
        }
    }

    #[test]
    fn frame_transform_matches_per_pixel() {
        let t = XyzToSrgb::new();
        // two pixels as xyz12le (code << 4)
        let codes: [(u16, u16, u16); 2] = [(2000, 2048, 2100), (0, 0, 0)];
        let mut raw = Vec::new();
        for (x, y, z) in codes {
            raw.extend_from_slice(&(x << 4).to_le_bytes());
            raw.extend_from_slice(&(y << 4).to_le_bytes());
            raw.extend_from_slice(&(z << 4).to_le_bytes());
        }
        let mut out = Vec::new();
        t.frame_xyz12le_to_srgb8(&raw, &mut out);
        assert_eq!(&out[0..3], &t.pixel(2000, 2048, 2100));
        assert_eq!(&out[3..6], &[0, 0, 0]);
    }

    // Generate an sRGB ICC in-test and push DCI white through the ICC engine.
    // sRGB is our built-in target, so the result must land near-neutral white.
    #[cfg(feature = "icc")]
    #[test]
    fn icc_path_maps_dci_white_to_plausible_neutral() {
        let icc = lcms2::Profile::new_srgb().icc().unwrap();
        let path = std::env::temp_dir().join(format!("postkit-srgb-{}.icc", uuid::Uuid::new_v4()));
        std::fs::write(&path, &icc).unwrap();

        let t = XyzToIcc::new(&path, RenderingIntent::RelativeColorimetric).unwrap();
        let [x, y, z] = dci_white_codes();
        let mut raw = Vec::new();
        raw.extend_from_slice(&(x << 4).to_le_bytes());
        raw.extend_from_slice(&(y << 4).to_le_bytes());
        raw.extend_from_slice(&(z << 4).to_le_bytes());
        let mut out = Vec::new();
        t.frame_xyz12le_to_rgb8(&raw, &mut out);

        let max = *out.iter().max().unwrap();
        let min = *out.iter().min().unwrap();
        assert!(
            max >= 240,
            "DCI white not near full-scale through ICC: {out:?}"
        );
        assert!(max - min <= 8, "DCI white not neutral through ICC: {out:?}");
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod tests_xyz {
    use super::*;

    // Independent reference: display-referred Rec.709 RGB (16-bit) → 16-bit
    // X'Y'Z', done in f64 with no LUTs. gamma 2.2 → Rec.709/D65 matrix ×
    // (48/52.37) → 2.6 out, matching libdcp rec709_to_xyz + rgb_xyz.cc and
    // grok's applyXYZTransform. Derived from the spec, not from the impl.
    fn expected_xyz16(rgb: [u16; 3]) -> [u16; 3] {
        let coeff = 48.0f64 / 52.37;
        let m = [
            [0.4124564, 0.3575761, 0.1804375],
            [0.2126729, 0.7151522, 0.0721750],
            [0.0193339, 0.1191920, 0.9503041],
        ];
        let lin: Vec<f64> = rgb
            .iter()
            .map(|&v| (v as f64 / 65535.0).powf(2.2))
            .collect();
        let mut out = [0u16; 3];
        for (i, row) in m.iter().enumerate() {
            let xyz = (row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2]) * coeff;
            out[i] = (xyz.clamp(0.0, 1.0).powf(1.0 / 2.6) * 65535.0 + 0.5) as u16;
        }
        out
    }

    fn run(rgb: [u16; 3]) -> [u16; 3] {
        let mut buf = Vec::new();
        for c in rgb {
            buf.extend_from_slice(&c.to_be_bytes());
        }
        rgb_to_xyz_inplace(&mut buf);
        [
            u16::from_be_bytes([buf[0], buf[1]]),
            u16::from_be_bytes([buf[2], buf[3]]),
            u16::from_be_bytes([buf[4], buf[5]]),
        ]
    }

    // two-stage LUT quantization vs the f64 reference costs a few codes at most
    fn assert_close(got: [u16; 3], want: [u16; 3]) {
        for i in 0..3 {
            let d = (got[i] as i32 - want[i] as i32).abs();
            assert!(d <= 4, "channel {i}: got {got:?} want {want:?} (Δ{d})");
        }
    }

    #[test]
    fn black_stays_black() {
        assert_eq!(run([0, 0, 0]), [0, 0, 0]);
    }

    #[test]
    fn red_matches_reference() {
        // solid Rec.709 red; scaled to 12-bit this is grok's [2817, 2183, 870]
        let got = run([65535, 0, 0]);
        assert_close(got, expected_xyz16([65535, 0, 0]));
        let grok12 = [2817u16, 2183, 870];
        for (i, &g) in grok12.iter().enumerate() {
            let twelve = (got[i] as u32 * 4095 / 65535) as i32;
            assert!(
                (twelve - g as i32).abs() <= 2,
                "12-bit {i}: {twelve} vs grok {g}"
            );
        }
    }

    #[test]
    fn white_matches_reference() {
        // DCI companding puts white Y below full code, not at 65535
        let got = run([65535, 65535, 65535]);
        assert_close(got, expected_xyz16([65535, 65535, 65535]));
        assert!(got[1] < 64000, "white Y must be companded, got {}", got[1]);
    }

    #[test]
    fn mid_grey_matches_reference() {
        let rgb = [32768, 32768, 32768];
        assert_close(run(rgb), expected_xyz16(rgb));
    }
}
