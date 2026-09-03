use crate::grok_encoder::SampleOrder;
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

/// The colour space a name written on a command line means, or None when it
/// names none of them. Every spelling either wizard has ever taken is here, so
/// a name accepted by one is accepted by both.
pub fn parse_colour_space(name: &str) -> Option<ColourSpace> {
    match name.trim().to_lowercase().as_str() {
        "rec709" | "bt709" => Some(ColourSpace::Rec709),
        "p3" | "dcip3" | "dci-p3" => Some(ColourSpace::P3),
        "xyz" | "ciexyz" => Some(ColourSpace::Xyz),
        "rec2020" | "bt2020" | "2020" => Some(ColourSpace::Rec2020),
        "aces" | "ap0" => Some(ColourSpace::Aces),
        "acescg" | "ap1" => Some(ColourSpace::AcesCg),
        "logc" | "arrilogc" | "alexa" => Some(ColourSpace::LogC),
        _ => None,
    }
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

    /// Every spelling either front end takes lands on the same space, whichever
    /// front end it was written for.
    #[test]
    fn every_name_a_front_end_takes_parses() {
        for (name, space) in [
            ("rec709", ColourSpace::Rec709),
            ("BT709", ColourSpace::Rec709),
            ("dcip3", ColourSpace::P3),
            ("dci-p3", ColourSpace::P3),
            ("ciexyz", ColourSpace::Xyz),
            ("2020", ColourSpace::Rec2020),
            ("bt2020", ColourSpace::Rec2020),
            ("ap0", ColourSpace::Aces),
            ("ap1", ColourSpace::AcesCg),
            ("alexa", ColourSpace::LogC),
            (" arrilogc ", ColourSpace::LogC),
        ] {
            assert_eq!(parse_colour_space(name), Some(space), "{name}");
        }
        assert_eq!(parse_colour_space("srgb"), None);
        assert_eq!(parse_colour_space(""), None);
    }

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

/// ARRI ALEXA Log C (LogC3) decode parameters for EI 800, relative scene
/// exposure, from "ALEXA Log C Curve - Usage in VFX" (Brendel, rev 2017-03-09).
const LOGC3_EI800_CUT: f32 = 0.010_591;
const LOGC3_EI800_A: f32 = 5.555_556;
const LOGC3_EI800_B: f32 = 0.052_272;
const LOGC3_EI800_C: f32 = 0.247_190;
const LOGC3_EI800_D: f32 = 0.385_537;
const LOGC3_EI800_E: f32 = 5.367_655;
const LOGC3_EI800_F: f32 = 0.092_809;

/// How a source's normalised code values become linear light.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Linearisation {
    Gamma(f32),
    /// ARRI LogC3 at EI 800. It reaches well past 1.0 scene linear, and
    /// everything above that clips to white in the DCDM encode's clamp.
    LogC3,
}

impl Linearisation {
    fn to_linear(self, code: f32) -> f32 {
        match self {
            Self::Gamma(gamma) => code.powf(gamma),
            Self::LogC3 => {
                if code > LOGC3_EI800_E * LOGC3_EI800_CUT + LOGC3_EI800_F {
                    (10.0f32.powf((code - LOGC3_EI800_D) / LOGC3_EI800_C) - LOGC3_EI800_B)
                        / LOGC3_EI800_A
                } else {
                    (code - LOGC3_EI800_F) / LOGC3_EI800_E
                }
            }
        }
    }
}

/// Source colour space description for the DCDM transform: the linear RGB to
/// CIE XYZ matrix, the curve that linearises its code values, and the scale that
/// lands source white on the DCI reference white.
pub(crate) struct SourceSpace {
    pub to_xyz: [[f32; 3]; 3],
    pub linearisation: Linearisation,
    pub scale: f32,
}

/// Resolve a source colour space to its matrix, linearisation and DCI scale.
///
/// The scene-referred spaces are refused: no 3x3 matrix reaches X'Y'Z' from
/// them, and approximating one silently would be wrong colour.
pub(crate) fn source_space(space: ColourSpace) -> Result<SourceSpace, String> {
    let dci_scale = DCI_REFERENCE_WHITE / DCI_PEAK_LUMINANCE;
    let resolved = match space {
        ColourSpace::Rec709 => SourceSpace {
            // the sRGB/D65 matrix grok, libdcp and DoM all use, so their X'Y'Z'
            // and ours agree code for code
            to_xyz: [
                [0.412_456_4, 0.357_576_1, 0.180_437_5],
                [0.212_672_9, 0.715_152_2, 0.072_175],
                [0.019_333_9, 0.119_192, 0.950_304_1],
            ],
            // gamma 2.2 for display-referred Rec.709, matching libdcp rec709_to_xyz,
            // DoM and grok. Was 2.4 (Rec.1886); harmonized 2026-07-23.
            linearisation: Linearisation::Gamma(2.2),
            scale: dci_scale,
        },
        ColourSpace::P3 => SourceSpace {
            to_xyz: [
                [0.445_169_8, 0.277_134_4, 0.172_282_7],
                [0.209_491_7, 0.721_595_2, 0.068_913_1],
                [0.0, 0.047_060_6, 0.907_378_4],
            ],
            linearisation: Linearisation::Gamma(DCDM_GAMMA),
            scale: dci_scale,
        },
        ColourSpace::Rec2020 => SourceSpace {
            to_xyz: [
                [0.636_958, 0.144_616_9, 0.168_881],
                [0.262_700_2, 0.677_998_1, 0.059_301_7],
                [0.0, 0.028_072_7, 1.060_985_1],
            ],
            linearisation: Linearisation::Gamma(2.4),
            scale: dci_scale,
        },
        // already X'Y'Z': decode and requantise, the luminance scaling is baked in
        ColourSpace::Xyz => SourceSpace {
            to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            linearisation: Linearisation::Gamma(DCDM_GAMMA),
            scale: 1.0,
        },
        // ALEXA Wide Gamut RGB with D65 white, from the same ARRI document as the
        // LogC3 decode. Scene linear above 1.0 clips to white, no tone mapping.
        ColourSpace::LogC => SourceSpace {
            to_xyz: [
                [0.638_008, 0.214_704, 0.097_744],
                [0.291_954, 0.823_841, -0.115_795],
                [0.002_798, -0.067_034, 1.153_294],
            ],
            linearisation: Linearisation::LogC3,
            scale: dci_scale,
        },
        ColourSpace::Aces | ColourSpace::AcesCg => {
            return Err(format!(
                "{space:?} is scene-referred: no 3x3 matrix reaches X'Y'Z' from it, \
                 so it needs a 3D LUT that lands on one of rec709, p3, rec2020 or xyz"
            ));
        }
    };
    Ok(resolved)
}

/// The DCDM encode transform for one source colour space, built once and applied
/// to every frame of a run.
///
/// Per pixel: linearise the source's code values with its own curve (a gamma, or
/// the LogC3 decode), matrix them into the output space, scale to the DCI
/// reference white, then re-encode with the 2.6 gamma SMPTE 428-1 stores. For
/// Rec.709 this is the transform grok applies itself and libdcp/DoM's
/// `rgb_xyz.cc` agrees with.
pub struct DcdmTransform {
    space: ColourSpace,
    /// linear source RGB to the output space (X'Y'Z' or P3-D65 linear RGB)
    matrix: [[f32; 3]; 3],
    /// linear scale applied to the matrix output
    scale: f32,
    /// 16-bit source code value to linear light
    linear: Vec<f32>,
}

impl std::fmt::Debug for DcdmTransform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DcdmTransform")
            .field("space", &self.space)
            .finish()
    }
}

impl DcdmTransform {
    /// Transform from `space` to DCI X'Y'Z'.
    pub fn to_xyz(space: ColourSpace) -> Result<Self, String> {
        let resolved = source_space(space)?;
        Ok(Self::new(
            space,
            resolved.to_xyz,
            resolved.linearisation,
            resolved.scale,
        ))
    }

    pub(crate) fn new(
        space: ColourSpace,
        matrix: [[f32; 3]; 3],
        linearisation: Linearisation,
        scale: f32,
    ) -> Self {
        Self {
            space,
            matrix,
            scale,
            linear: (0..=u16::MAX)
                .map(|v| linearisation.to_linear(v as f32 / 65535.0))
                .collect(),
        }
    }

    /// One pixel of 16-bit source code values to output code values, quantised
    /// to `max_code` (4095 for 12-bit DCDM, 65535 for 16-bit).
    pub fn pixel(&self, rgb: [u16; 3], max_code: u16) -> [u16; 3] {
        let r = self.linear[rgb[0] as usize];
        let g = self.linear[rgb[1] as usize];
        let b = self.linear[rgb[2] as usize];
        let inv_gamma = 1.0 / DCDM_GAMMA;
        let max = max_code as f32;
        let mut out = [0u16; 3];
        for (row, slot) in self.matrix.iter().zip(out.iter_mut()) {
            let v = (row[0] * r + row[1] * g + row[2] * b) * self.scale;
            *slot = (v.clamp(0.0, 1.0).powf(inv_gamma) * max).round() as u16;
        }
        out
    }

    /// Convert one rgb48le frame into `out`, three code values per pixel.
    pub fn frame_rgb48le(&self, rgb: &[u8], max_code: u16, out: &mut [u16]) {
        for (px, xyz) in rgb
            .as_chunks::<6>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<3>().0)
        {
            let codes = self.pixel(
                [
                    u16::from_le_bytes([px[0], px[1]]),
                    u16::from_le_bytes([px[2], px[3]]),
                    u16::from_le_bytes([px[4], px[5]]),
                ],
                max_code,
            );
            xyz.copy_from_slice(&codes);
        }
    }

    /// Convert one packed rgb48 frame in place, to 16-bit code values in the
    /// same layout and byte order. This is ffmpeg's rawvideo format and what the
    /// J2K encoder reads.
    pub fn frame_rgb48_inplace(&self, buf: &mut [u8], order: SampleOrder) {
        for px in buf.as_chunks_mut::<6>().0 {
            let read = |bytes: [u8; 2]| match order {
                SampleOrder::Big => u16::from_be_bytes(bytes),
                SampleOrder::Little => u16::from_le_bytes(bytes),
            };
            let codes = self.pixel(
                [
                    read([px[0], px[1]]),
                    read([px[2], px[3]]),
                    read([px[4], px[5]]),
                ],
                u16::MAX,
            );
            for (i, code) in codes.iter().enumerate() {
                let bytes = match order {
                    SampleOrder::Big => code.to_be_bytes(),
                    SampleOrder::Little => code.to_le_bytes(),
                };
                px[i * 2] = bytes[0];
                px[i * 2 + 1] = bytes[1];
            }
        }
    }
}

/// Transform a 16-bit big-endian Rec.709 RGB frame buffer to X'Y'Z' (DCI) in place.
///
/// Assumes `buf` contains pixels as [R_hi, R_lo, G_hi, G_lo, B_hi, B_lo, ...]
/// (rgb48be format from ffmpeg). Each sample is 16-bit unsigned big-endian.
///
/// Builds the transform per call, so a caller converting more than one frame
/// should hold a [`DcdmTransform`] instead.
pub fn rgb_to_xyz_inplace(buf: &mut [u8]) {
    DcdmTransform::to_xyz(ColourSpace::Rec709)
        .expect("Rec.709 has a matrix")
        .frame_rgb48_inplace(buf, SampleOrder::Big);
}

// ─── Display transform: DCI X'Y'Z' code values → sRGB ─────────────────────
//
// Inverse of the DCDM encode in `dcdm.rs`, for showing a real DCP picture
// (12-bit CIE X'Y'Z', 2.6 gamma per SMPTE 428-1) on an sRGB monitor. Pipeline:
//   code/4095 → ^2.6 (peak-relative linear XYZ)
//   × 52.37/48 (Y = 1 at the DCI reference white)
//   XYZ(D65) → linear sRGB
//   sRGB OETF → 8-bit.
//
// No illuminant adaptation, because a DCDM stores absolute XYZ and does not say
// what its neutral is. The encode side leaves a Rec.709 or P3-D65 master's
// neutral at D65, as libdcp and DCP-o-matic do, so adapting here would tint
// every DCP built from one. A picture graded against DCI white renders green,
// which is what it measures.

/// Rendering intent for the optional ICC display path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderingIntent {
    #[default]
    RelativeColorimetric,
    AbsoluteColorimetric,
    Perceptual,
    Saturation,
}

/// DCDM encoding gamma (SMPTE 428-1), used in both directions.
const DCDM_GAMMA: f32 = 2.6;
const MAX_CODE_12BIT: f32 = 4095.0;
/// SMPTE 428-1 peak luminance the encoding normalises against (cd/m²).
const DCI_PEAK_LUMINANCE: f32 = 52.37;
/// DCI reference white luminance (cd/m²).
const DCI_REFERENCE_WHITE: f32 = 48.0;

// Bradford cone-response matrix and its inverse. Only the ICC display path
// adapts an illuminant; the sRGB one renders the DCDM's XYZ as measured.
#[cfg(feature = "icc")]
const BRADFORD: [[f32; 3]; 3] = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];
#[cfg(feature = "icc")]
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
// D65 as XYZ with Y = 1, x=0.3127 y=0.3290.
#[cfg(any(feature = "icc", test))]
const D65_WHITE_XYZ: [f32; 3] = [0.950_456, 1.0, 1.088_754];

fn mat_vec(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

#[cfg(feature = "icc")]
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
#[cfg(feature = "icc")]
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
        let mut mat = XYZ_D65_TO_SRGB;
        for row in mat.iter_mut() {
            for c in row.iter_mut() {
                *c *= scale;
            }
        }
        let expand = (0..=4095u32)
            .map(|c| (c as f32 / MAX_CODE_12BIT).powf(DCDM_GAMMA))
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
        for px in raw.as_chunks::<6>().0 {
            let x = u16::from_le_bytes([px[0], px[1]]) >> 4;
            let y = u16::from_le_bytes([px[2], px[3]]) >> 4;
            let z = u16::from_le_bytes([px[4], px[5]]) >> 4;
            out.extend_from_slice(&self.pixel(x, y, z));
        }
    }
}

#[cfg(feature = "icc")]
mod icc {
    use super::{D65_WHITE_XYZ, DCI_PEAK_LUMINANCE, DCI_REFERENCE_WHITE, MAX_CODE_12BIT};
    use super::{DCDM_GAMMA, RenderingIntent, bradford, mat_vec};
    use lcms2::{Intent, PixelFormat, Profile, Transform};

    // D50 PCS white (lcms2's XYZ profile connection space).
    const D50_WHITE_XYZ: [f32; 3] = [0.964_212, 1.0, 0.825_188];

    /// DCI X'Y'Z' → device RGB through a monitor ICC profile.
    ///
    /// Decodes to peak-relative linear XYZ, adapts D65 → the D50 PCS,
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
            let mut to_pcs = bradford(D65_WHITE_XYZ, D50_WHITE_XYZ);
            for row in to_pcs.iter_mut() {
                for c in row.iter_mut() {
                    *c *= scale;
                }
            }
            let expand = (0..=4095u32)
                .map(|c| (c as f32 / MAX_CODE_12BIT).powf(DCDM_GAMMA))
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
            for px in raw.as_chunks::<6>().0 {
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

    // DCI white x=0.314 y=0.351 as XYZ with Y = 1, for the codes a picture graded
    // against the projector's own white carries.
    const DCI_WHITE_XYZ: [f32; 3] = [0.894_587, 1.0, 0.954_416];

    /// A white's 12-bit X'Y'Z' codes at `luminance` of the DCI reference white.
    fn white_codes(white: [f32; 3], luminance: f32) -> [u16; 3] {
        let mut codes = [0u16; 3];
        for i in 0..3 {
            let peak = white[i] * luminance * DCI_REFERENCE_WHITE / DCI_PEAK_LUMINANCE;
            codes[i] = (peak.powf(1.0 / DCDM_GAMMA) * MAX_CODE_12BIT).round() as u16;
        }
        codes
    }

    /// Encoding Rec.709 to X'Y'Z' and previewing it has to return the picture
    /// that went in. An illuminant adaptation on one side only is what broke
    /// this, and it showed up as a desaturated, faintly magenta preview of every
    /// DCP built from a Rec.709 master.
    #[test]
    fn the_preview_inverts_the_rec709_encode() {
        let encode = DcdmTransform::to_xyz(ColourSpace::Rec709).unwrap();
        let preview = XyzToSrgb::new();
        // gamma 2.2 on the way in against the sRGB OETF on the way out costs a
        // code or two, so this is not bit-exact
        const TOLERANCE: i32 = 3;
        for rgb16 in [
            [65535u16, 65535, 65535],
            [65535, 0, 0],
            [0, 65535, 0],
            [0, 0, 65535],
            [32768, 32768, 32768],
            [49152, 16384, 32768],
        ] {
            let xyz = encode.pixel(rgb16, 4095);
            let out = preview.pixel(xyz[0], xyz[1], xyz[2]);
            for channel in 0..3 {
                let want = (f32::from(rgb16[channel]) / 65535.0 * 255.0).round() as i32;
                let got = i32::from(out[channel]);
                assert!(
                    (got - want).abs() <= TOLERANCE,
                    "channel {channel} of {rgb16:?} came back {got}, not {want} (got {out:?})"
                );
            }
        }
    }

    #[test]
    fn black_maps_to_black() {
        let t = XyzToSrgb::new();
        assert_eq!(t.pixel(0, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn d65_reference_white_maps_to_srgb_white() {
        let t = XyzToSrgb::new();
        let [x, y, z] = white_codes(D65_WHITE_XYZ, 1.0);
        let rgb = t.pixel(x, y, z);
        // neutral and near full-scale
        for c in rgb {
            assert!(c >= 253, "channel {c} not near 255 for D65 white {rgb:?}");
        }
    }

    #[test]
    fn dci_white_chromaticity_renders_green() {
        // The transform adapts no illuminant, so a picture graded against DCI
        // white shows the green cast it measures rather than being neutralised.
        // Neutralising it is what tinted every DCP built from a D65 master.
        let t = XyzToSrgb::new();
        let [x, y, z] = white_codes(DCI_WHITE_XYZ, 1.0);
        let [r, g, b] = t.pixel(x, y, z);
        assert!(
            g > r && g > b,
            "DCI white should stay green, got {:?}",
            [r, g, b]
        );
    }

    #[test]
    fn mid_grey_is_neutral_and_between_black_and_white() {
        let t = XyzToSrgb::new();
        let codes = white_codes(D65_WHITE_XYZ, 0.18);
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

    // Generate an sRGB ICC in-test and push D65 white through the ICC engine.
    // sRGB is our built-in target, so the result must land near-neutral white.
    #[cfg(feature = "icc")]
    #[test]
    fn icc_path_maps_d65_white_to_plausible_neutral() {
        let icc = lcms2::Profile::new_srgb().icc().unwrap();
        let path = std::env::temp_dir().join(format!("postkit-srgb-{}.icc", uuid::Uuid::new_v4()));
        std::fs::write(&path, &icc).unwrap();

        let t = XyzToIcc::new(&path, RenderingIntent::RelativeColorimetric).unwrap();
        let [x, y, z] = white_codes(D65_WHITE_XYZ, 1.0);
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
            "D65 white not near full-scale through ICC: {out:?}"
        );
        assert!(max - min <= 8, "D65 white not neutral through ICC: {out:?}");
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

    // f32 arithmetic and the linearization table cost a few codes against the
    // f64 reference
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

    // Independent 12-bit reference for a source that is not Rec.709: linearise
    // with the space's own gamma, apply its published RGB->XYZ matrix, compand
    // to the DCI reference white, encode with 2.6.
    fn expected_xyz12(rgb: [u16; 3], matrix: [[f64; 3]; 3], gamma: f64) -> [u16; 3] {
        let coeff = 48.0f64 / 52.37;
        let lin: Vec<f64> = rgb
            .iter()
            .map(|&v| (v as f64 / 65535.0).powf(gamma))
            .collect();
        let mut out = [0u16; 3];
        for (i, row) in matrix.iter().enumerate() {
            let xyz = (row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2]) * coeff;
            out[i] = (xyz.clamp(0.0, 1.0).powf(1.0 / 2.6) * 4095.0 + 0.5) as u16;
        }
        out
    }

    // SMPTE RP 431-2 P3-DCI primaries with the DCI white point, gamma 2.6.
    const P3_DCI_TO_XYZ: [[f64; 3]; 3] = [
        [0.4451698, 0.2771344, 0.1722827],
        [0.2094917, 0.7215952, 0.0689131],
        [0.0, 0.0470606, 0.9073747],
    ];
    // ITU-R BT.2020 primaries with D65, gamma 2.4.
    const REC2020_TO_XYZ: [[f64; 3]; 3] = [
        [0.6369580, 0.1446169, 0.1688810],
        [0.2627002, 0.6779981, 0.0593017],
        [0.0, 0.0280727, 1.0609851],
    ];

    #[test]
    fn p3_source_matches_a_published_matrix() {
        let transform = DcdmTransform::to_xyz(ColourSpace::P3).unwrap();
        for rgb in [[65535u16, 0, 0], [0, 65535, 0], [65535; 3], [32768; 3]] {
            let got = transform.pixel(rgb, 4095);
            let want = expected_xyz12(rgb, P3_DCI_TO_XYZ, 2.6);
            for i in 0..3 {
                let d = (got[i] as i32 - want[i] as i32).abs();
                assert!(d <= 2, "{rgb:?} channel {i}: got {got:?} want {want:?}");
            }
        }
    }

    #[test]
    fn rec2020_source_matches_a_published_matrix() {
        let transform = DcdmTransform::to_xyz(ColourSpace::Rec2020).unwrap();
        for rgb in [[65535u16, 0, 0], [0, 0, 65535], [65535; 3], [32768; 3]] {
            let got = transform.pixel(rgb, 4095);
            let want = expected_xyz12(rgb, REC2020_TO_XYZ, 2.4);
            for i in 0..3 {
                let d = (got[i] as i32 - want[i] as i32).abs();
                assert!(d <= 2, "{rgb:?} channel {i}: got {got:?} want {want:?}");
            }
        }
    }

    #[test]
    fn a_wide_gamut_source_is_not_silently_read_as_rec709() {
        let red = [65535u16, 0, 0];
        let rec709 = DcdmTransform::to_xyz(ColourSpace::Rec709)
            .unwrap()
            .pixel(red, 4095);
        for space in [ColourSpace::P3, ColourSpace::Rec2020] {
            let got = DcdmTransform::to_xyz(space).unwrap().pixel(red, 4095);
            assert!(
                got[0] > rec709[0],
                "{space:?} red must reach further than Rec.709 red: {got:?} vs {rec709:?}"
            );
        }
        for (i, grok) in [2817i32, 2183, 870].iter().enumerate() {
            let d = (rec709[i] as i32 - grok).abs();
            assert!(d <= 2, "Rec.709 red {rec709:?} vs grok's published values");
        }
    }

    #[test]
    fn the_scene_referred_spaces_are_refused() {
        for space in [ColourSpace::Aces, ColourSpace::AcesCg] {
            let err = DcdmTransform::to_xyz(space).unwrap_err();
            assert!(err.contains("3D LUT"), "{space:?}: {err}");
        }
    }

    // ITU-R BT.709 primaries with D65, the sRGB matrix, gamma 2.2.
    const REC709_TO_XYZ: [[f64; 3]; 3] = [
        [0.4124564, 0.3575761, 0.1804375],
        [0.2126729, 0.7151522, 0.0721750],
        [0.0193339, 0.1191920, 0.9503041],
    ];
    // the LogC3 code value ARRI publishes for an 18% grey card at EI 800
    const LOGC3_GREY_CODE: f64 = 0.391;
    // a LogC3 code decoding above 1.0 scene linear
    const LOGC3_OVER_WHITE_CODE: f64 = 0.6;
    // the f constant, the LogC3 code decoding to 0 scene linear
    const LOGC3_BLACK_CODE: f64 = 0.092809;

    #[test]
    fn logc_grey_matches_rec709_grey() {
        let code = (LOGC3_GREY_CODE * 65535.0).round() as u16;
        let got = DcdmTransform::to_xyz(ColourSpace::LogC)
            .unwrap()
            .pixel([code; 3], 4095);

        // 0.18 linear on all three channels through the Rec.709 arm: both are
        // D65 neutral at the same luminance, so they land on the same X'Y'Z'
        let coeff = 48.0f64 / 52.37;
        let mut want = [0u16; 3];
        for (i, row) in REC709_TO_XYZ.iter().enumerate() {
            let xyz = (row[0] + row[1] + row[2]) * 0.18 * coeff;
            want[i] = (xyz.powf(1.0 / 2.6) * 4095.0).round() as u16;
        }

        for i in 0..3 {
            let d = (got[i] as i32 - want[i] as i32).abs();
            assert!(d <= 1, "channel {i}: got {got:?} want {want:?}");
        }
    }

    #[test]
    fn logc_above_diffuse_white_clips() {
        let transform = DcdmTransform::to_xyz(ColourSpace::LogC).unwrap();
        let clipped = transform.pixel([65535; 3], 4095);
        assert_eq!(clipped, [4095; 3]);
        let code = (LOGC3_OVER_WHITE_CODE * 65535.0).round() as u16;
        assert_eq!(transform.pixel([code; 3], 4095), clipped);
    }

    #[test]
    fn logc_black_is_black() {
        let code = (LOGC3_BLACK_CODE * 65535.0) as u16;
        let got = DcdmTransform::to_xyz(ColourSpace::LogC)
            .unwrap()
            .pixel([code; 3], 4095);
        assert_eq!(got, [0, 0, 0]);
    }

    #[test]
    fn the_frame_forms_agree_with_the_pixel_form() {
        let transform = DcdmTransform::to_xyz(ColourSpace::P3).unwrap();
        let pixels = [[65535u16, 0, 0], [0, 32768, 12345]];

        let mut le = Vec::new();
        let mut be = Vec::new();
        for px in pixels {
            for c in px {
                le.extend_from_slice(&c.to_le_bytes());
                be.extend_from_slice(&c.to_be_bytes());
            }
        }
        let mut out = vec![0u16; 6];
        transform.frame_rgb48le(&le, 65535, &mut out);
        let mut le_in_place = le.clone();
        transform.frame_rgb48_inplace(&mut le_in_place, SampleOrder::Little);
        transform.frame_rgb48_inplace(&mut be, SampleOrder::Big);

        for (i, px) in pixels.iter().enumerate() {
            let want = transform.pixel(*px, 65535);
            assert_eq!(&out[i * 3..i * 3 + 3], &want, "rgb48le pixel {i}");
            for (c, want_channel) in want.iter().enumerate() {
                let off = i * 6 + c * 2;
                assert_eq!(
                    u16::from_be_bytes([be[off], be[off + 1]]),
                    *want_channel,
                    "rgb48be pixel {i} channel {c}"
                );
                assert_eq!(
                    u16::from_le_bytes([le_in_place[off], le_in_place[off + 1]]),
                    *want_channel,
                    "in place rgb48le pixel {i} channel {c}"
                );
            }
        }
    }
}
