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

// Rec.709 transfer function: gamma → linear
// For values > 0.081, linear = ((V + 0.099) / 1.099)^(1/0.45)
// For values <= 0.081, linear = V / 4.5
fn rec709_to_linear(v: f64) -> f64 {
    if v <= 0.081 {
        v / 4.5
    } else {
        ((v + 0.099) / 1.099).powf(1.0 / 0.45)
    }
}

// DCI 2.6 gamma: linear → X'Y'Z' (gamma-encoded)
fn linear_to_dci_gamma(v: f64) -> f64 {
    if v <= 0.0 { 0.0 } else { v.powf(1.0 / 2.6) }
}

/// Transform a 16-bit big-endian RGB frame buffer to X'Y'Z' (DCI) in-place.
///
/// Assumes `buf` contains pixels as [R_hi, R_lo, G_hi, G_lo, B_hi, B_lo, ...]
/// (rgb48be format from ffmpeg). Each sample is 16-bit unsigned big-endian.
///
/// The transform pipeline:
/// 1. Rec.709 OETF⁻¹ (gamma → linear)
/// 2. 3×3 matrix (linear Rec.709 RGB → linear CIE XYZ)
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

        // Step 2: 3×3 matrix multiply (linear RGB → linear XYZ)
        let x_lin = 0.4124564_f32 * r_lin + 0.3575761_f32 * g_lin + 0.1804375_f32 * b_lin;
        let y_lin = 0.2126729_f32 * r_lin + 0.7151522_f32 * g_lin + 0.0721750_f32 * b_lin;
        let z_lin = 0.0193339_f32 * r_lin + 0.119_192_f32 * g_lin + 0.9503041_f32 * b_lin;

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

#[cfg(test)]
mod tests_xyz {
    use super::*;

    #[test]
    fn test_black_stays_black() {
        let mut buf = [0u8; 6];
        rgb_to_xyz_inplace(&mut buf);
        assert_eq!(buf, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_white_maps_reasonably() {
        let mut buf = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        rgb_to_xyz_inplace(&mut buf);
        let x = ((buf[0] as u16) << 8) | buf[1] as u16;
        let y = ((buf[2] as u16) << 8) | buf[3] as u16;
        let z = ((buf[4] as u16) << 8) | buf[5] as u16;
        assert!(y > 60000, "Y should be near max for white, got {y}");
        assert!(x > 50000, "X should be high for white, got {x}");
        assert!(z > 50000, "Z should be high for white, got {z}");
    }
}
