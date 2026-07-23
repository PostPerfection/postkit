use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// DCDM colour encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DcdmColourEncoding {
    /// X'Y'Z' 12-bit (SMPTE 428-1)
    Xyz12Bit,
    /// X'Y'Z' 16-bit
    Xyz16Bit,
}

/// Output colour space of the transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DcdmTarget {
    /// CIE X'Y'Z' with DCI white and 48/52.37 companding (SMPTE 428-1), the DCDM standard.
    #[default]
    Xyz,
    /// P3-D65 RGB, 2.6 gamma. a mastering target (P3 primaries, D65 white), not a DCDM;
    /// source white maps to full-scale, no DCI companding.
    P3D65,
}

/// DCDM creation options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DcdmOptions {
    /// Source image sequence (DPX/TIFF/EXR)
    pub input_dir: PathBuf,
    /// Output DCDM TIFF sequence
    pub output_dir: PathBuf,
    pub encoding: DcdmColourEncoding,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Source colour space for conversion
    pub colour_space: String,
    /// Output colour space (X'Y'Z' by default, or P3-D65 RGB)
    pub target: DcdmTarget,
    /// Optional 3D LUT for colour transform
    pub lut_path: PathBuf,
}

impl Default for DcdmOptions {
    fn default() -> Self {
        Self {
            input_dir: PathBuf::new(),
            output_dir: PathBuf::new(),
            encoding: DcdmColourEncoding::Xyz12Bit,
            width: 4096,
            height: 2160,
            fps_num: 24,
            fps_den: 1,
            colour_space: String::new(),
            target: DcdmTarget::Xyz,
            lut_path: PathBuf::new(),
        }
    }
}

/// Result of DCDM operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DcdmResult {
    pub success: bool,
    pub error: String,
    pub frames_written: u64,
    pub output_dir: PathBuf,
}

/// Create DCDM (Digital Cinema Distribution Master) from source images.
///
/// Uses ffmpeg to convert source images to X'Y'Z' TIFF sequence.
pub fn create_dcdm(opts: &DcdmOptions) -> DcdmResult {
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return DcdmResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    // Find source frames
    let frames = match crate::encode::find_source_frames(&opts.input_dir) {
        Ok(f) => f,
        Err(e) => {
            return DcdmResult {
                success: false,
                error: format!("Failed to find source frames: {e}"),
                ..Default::default()
            };
        }
    };

    if frames.is_empty() {
        return DcdmResult {
            success: false,
            error: "No source frames found".into(),
            ..Default::default()
        };
    }

    let space = match source_space(&opts.colour_space) {
        Ok(s) => s,
        Err(e) => {
            return DcdmResult {
                success: false,
                error: e,
                ..Default::default()
            };
        }
    };

    // ffmpeg only decodes and resizes; it cannot write X'Y'Z' TIFF (its tiff
    // encoder rejects xyz12le and silently falls back to rgb48le), so the
    // colour transform and TIFF writing happen here.
    let (width, height) = if opts.width > 0 && opts.height > 0 {
        (opts.width, opts.height)
    } else {
        match crate::probe::probe_video(&frames[0]) {
            Some(info) if info.width > 0 && info.height > 0 => (info.width, info.height),
            _ => {
                return DcdmResult {
                    success: false,
                    error: format!(
                        "Could not determine source dimensions from {}",
                        frames[0].display()
                    ),
                    ..Default::default()
                };
            }
        }
    };

    let first = &frames[0];
    let parent = first.parent().unwrap_or(Path::new("."));
    let ext = first.extension().and_then(|e| e.to_str()).unwrap_or("dpx");
    let input_pattern = parent.join(format!("*.{ext}"));

    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-pattern_type")
        .arg("glob")
        .arg("-i")
        .arg(input_pattern.to_string_lossy().as_ref());

    if !opts.lut_path.as_os_str().is_empty() && opts.lut_path.exists() {
        cmd.arg("-vf")
            .arg(format!("lut3d={}", opts.lut_path.display()));
    }

    cmd.arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("-pix_fmt")
        .arg("rgb48le")
        .arg("-f")
        .arg("rawvideo")
        .arg("-an")
        .arg("pipe:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let mut ffmpeg = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return DcdmResult {
                success: false,
                error: format!("Failed to run ffmpeg: {e}"),
                ..Default::default()
            };
        }
    };

    let Some(mut stdout) = ffmpeg.stdout.take() else {
        let _ = ffmpeg.kill();
        return DcdmResult {
            success: false,
            error: "Failed to capture ffmpeg stdout".into(),
            ..Default::default()
        };
    };

    let xf = space.to_target(opts.target);
    let max_code = opts.encoding.max_code_value();
    let lin = build_linear_lut(xf.gamma);
    let pixels = width as usize * height as usize;
    let mut frame_buf = vec![0u8; pixels * 3 * 2];
    let mut xyz_buf = vec![0u16; pixels * 3];
    let mut frames_written = 0u64;

    loop {
        match crate::encode::read_exact_or_eof(&mut stdout, &mut frame_buf) {
            crate::encode::ReadResult::Ok => {}
            crate::encode::ReadResult::Eof => break,
            crate::encode::ReadResult::Err(e) => {
                let _ = ffmpeg.kill();
                let _ = ffmpeg.wait();
                return DcdmResult {
                    success: false,
                    error: format!("Read error after {frames_written} frames: {e}"),
                    frames_written,
                    output_dir: opts.output_dir.clone(),
                };
            }
        }

        convert_frame(&frame_buf, &xf, &lin, max_code, &mut xyz_buf);

        let path = opts
            .output_dir
            .join(format!("dcdm_{frames_written:06}.tif"));
        if let Err(e) = write_xyz_tiff(&path, width, height, &xyz_buf) {
            let _ = ffmpeg.kill();
            let _ = ffmpeg.wait();
            return DcdmResult {
                success: false,
                error: format!("Failed to write {}: {e}", path.display()),
                frames_written,
                output_dir: opts.output_dir.clone(),
            };
        }
        frames_written += 1;
    }

    match ffmpeg.wait() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            return DcdmResult {
                success: false,
                error: format!("ffmpeg exited with {s}"),
                frames_written,
                output_dir: opts.output_dir.clone(),
            };
        }
        Err(e) => {
            return DcdmResult {
                success: false,
                error: format!("ffmpeg wait failed: {e}"),
                frames_written,
                output_dir: opts.output_dir.clone(),
            };
        }
    }

    if frames_written == 0 {
        return DcdmResult {
            success: false,
            error: "ffmpeg produced no frames".into(),
            ..Default::default()
        };
    }

    DcdmResult {
        success: true,
        error: String::new(),
        frames_written,
        output_dir: opts.output_dir.clone(),
    }
}

/// DCI reference white luminance in cd/m^2 (SMPTE 431-2).
const DCI_REFERENCE_WHITE: f32 = 48.0;
/// Peak luminance the DCDM encoding normalises against (SMPTE 428-1).
const DCI_PEAK_LUMINANCE: f32 = 52.37;
/// DCDM encoding gamma (SMPTE 428-1).
const DCDM_GAMMA: f32 = 2.6;

impl DcdmColourEncoding {
    /// Largest code value the encoding quantises to.
    fn max_code_value(self) -> u16 {
        match self {
            DcdmColourEncoding::Xyz12Bit => 4095,
            DcdmColourEncoding::Xyz16Bit => 65535,
        }
    }
}

/// Source colour space description for the X'Y'Z' transform.
struct SourceSpace {
    /// linear RGB to CIE XYZ, row major
    to_xyz: [[f32; 3]; 3],
    /// gamma used to linearise the incoming code values
    gamma: f32,
    /// linear scale mapping source white onto the DCI reference white
    scale: f32,
}

/// The resolved linear transform for one (source, target) pair: linear source
/// RGB -> output space, plus the luminance scale applied before the 2.6 gamma.
struct Transform {
    /// linear source RGB to the output space (X'Y'Z' or P3-D65 linear RGB)
    matrix: [[f32; 3]; 3],
    /// gamma used to linearise the incoming code values
    gamma: f32,
    /// linear scale applied to the matrix output
    scale: f32,
}

impl SourceSpace {
    /// Compose the source->XYZ matrix with the chosen output target.
    fn to_target(&self, target: DcdmTarget) -> Transform {
        match target {
            DcdmTarget::Xyz => Transform {
                matrix: self.to_xyz,
                gamma: self.gamma,
                scale: self.scale,
            },
            DcdmTarget::P3D65 => Transform {
                // linear source RGB -> XYZ -> P3-D65 linear RGB
                matrix: mat_mul(&xyz_to_p3d65(), &self.to_xyz),
                gamma: self.gamma,
                // rgb mastering target: source white -> full-scale, no dci companding
                scale: 1.0,
            },
        }
    }
}

// P3 primaries (SMPTE RP 431-2) and the D65 white point (CIE), as (x, y).
const P3_PRIMARIES: [[f64; 2]; 3] = [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]];
const D65_WHITE: [f64; 2] = [0.3127, 0.3290];

/// CIE XYZ -> P3-D65 linear RGB, derived from the P3 primaries + D65 white.
fn xyz_to_p3d65() -> [[f32; 3]; 3] {
    let m = invert3(rgb_to_xyz_matrix(&P3_PRIMARIES, D65_WHITE));
    let mut out = [[0.0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = m[i][j] as f32;
        }
    }
    out
}

/// RGB->XYZ matrix from chromaticities: build the primary matrix, solve for the
/// per-channel scale that lands the white point, then scale the columns.
fn rgb_to_xyz_matrix(primaries: &[[f64; 2]; 3], white: [f64; 2]) -> [[f64; 3]; 3] {
    let col = |x: f64, y: f64| [x / y, 1.0, (1.0 - x - y) / y];
    let mut p = [[0.0f64; 3]; 3];
    for j in 0..3 {
        let c = col(primaries[j][0], primaries[j][1]);
        for i in 0..3 {
            p[i][j] = c[i];
        }
    }
    let w = col(white[0], white[1]);
    let s = mat_vec(&invert3(p), w);
    let mut m = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            m[i][j] = p[i][j] * s[j];
        }
    }
    m
}

fn mat_vec(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
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

fn invert3(m: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let inv_det = 1.0 / det;
    let mut r = [[0.0f64; 3]; 3];
    r[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det;
    r[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det;
    r[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det;
    r[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det;
    r[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det;
    r[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det;
    r[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det;
    r[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det;
    r[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det;
    r
}

/// Resolve a source colour space name to its transform.
///
/// An empty name is treated as Rec.709, matching the wizard CLIs' own default.
fn source_space(name: &str) -> Result<SourceSpace, String> {
    let dci_scale = DCI_REFERENCE_WHITE / DCI_PEAK_LUMINANCE;
    let space = match name.trim().to_lowercase().as_str() {
        "" | "rec709" | "bt709" | "srgb" => SourceSpace {
            to_xyz: [
                [0.412_390_8, 0.357_584_3, 0.180_480_8],
                [0.212_639, 0.715_168_7, 0.072_192_3],
                [0.019_330_8, 0.119_194_8, 0.950_532_2],
            ],
            // gamma 2.2 for display-referred Rec.709, matching libdcp rec709_to_xyz,
            // DoM and grok. Was 2.4 (Rec.1886); harmonized 2026-07-23.
            gamma: 2.2,
            scale: dci_scale,
        },
        "p3" | "dcip3" | "dci-p3" | "p3dci" | "smpte431" => SourceSpace {
            to_xyz: [
                [0.445_169_8, 0.277_134_4, 0.172_282_7],
                [0.209_491_7, 0.721_595_2, 0.068_913_1],
                [0.0, 0.047_060_6, 0.907_378_4],
            ],
            gamma: DCDM_GAMMA,
            scale: dci_scale,
        },
        "rec2020" | "bt2020" => SourceSpace {
            to_xyz: [
                [0.636_958, 0.144_616_9, 0.168_881],
                [0.262_700_2, 0.677_998_1, 0.059_301_7],
                [0.0, 0.028_072_7, 1.060_985_1],
            ],
            gamma: 2.4,
            scale: dci_scale,
        },
        // already X'Y'Z': decode and requantise, the luminance scaling is baked in
        "xyz" | "ciexyz" => SourceSpace {
            to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            gamma: DCDM_GAMMA,
            scale: 1.0,
        },
        other => {
            return Err(format!(
                "Unsupported source colour space '{other}' for DCDM conversion. \
                 Use rec709, p3, rec2020 or xyz, or supply a 3D LUT that lands in one of those."
            ));
        }
    };
    Ok(space)
}

/// Table mapping every 16-bit code value to its linear light value.
fn build_linear_lut(gamma: f32) -> Vec<f32> {
    (0..=u16::MAX)
        .map(|v| (v as f32 / 65535.0).powf(gamma))
        .collect()
}

/// Convert one rgb48le frame into the target's code values (X'Y'Z' or P3-D65).
fn convert_frame(rgb: &[u8], xf: &Transform, lin: &[f32], max_code: u16, out: &mut [u16]) {
    let inv_gamma = 1.0 / DCDM_GAMMA;
    let max = max_code as f32;

    for (px, xyz) in rgb.chunks_exact(6).zip(out.chunks_exact_mut(3)) {
        let r = lin[u16::from_le_bytes([px[0], px[1]]) as usize];
        let g = lin[u16::from_le_bytes([px[2], px[3]]) as usize];
        let b = lin[u16::from_le_bytes([px[4], px[5]]) as usize];

        for (row, slot) in xf.matrix.iter().zip(xyz.iter_mut()) {
            let v = (row[0] * r + row[1] * g + row[2] * b) * xf.scale;
            *slot = (v.clamp(0.0, 1.0).powf(inv_gamma) * max).round() as u16;
        }
    }
}

/// Write X'Y'Z' code values as a 16-bit three-channel TIFF.
fn write_xyz_tiff(
    path: &Path,
    width: u32,
    height: u32,
    data: &[u16],
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = tiff::encoder::TiffEncoder::new(file)?;
    enc.write_image::<tiff::encoder::colortype::RGB16>(width, height, data)?;
    Ok(())
}

/// Convert DCDM back to viewable format (e.g. Rec.709 ProRes for review).
pub fn export_dcdm(
    dcdm_dir: &Path,
    output_dir: &Path,
    target_colour_space: Option<&str>,
) -> DcdmResult {
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return DcdmResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    let input_pattern = dcdm_dir.join("*.tif");
    let output_file = output_dir.join("review.mov");

    let colour_filter = match target_colour_space {
        Some("rec709") | Some("Rec709") | None => "colorspace=all=bt709:iall=bt709",
        Some("p3") | Some("P3") => "colorspace=all=bt709:iprimaries=smpte431",
        Some(_) => "colorspace=all=bt709",
    };

    let output = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-pattern_type")
        .arg("glob")
        .arg("-i")
        .arg(input_pattern.to_string_lossy().as_ref())
        .arg("-vf")
        .arg(colour_filter)
        .arg("-c:v")
        .arg("prores_ks")
        .arg("-profile:v")
        .arg("3")
        .arg(&output_file)
        .output();

    match output {
        Ok(o) if o.status.success() => DcdmResult {
            success: true,
            error: String::new(),
            frames_written: 0,
            output_dir: output_dir.to_path_buf(),
        },
        Ok(o) => DcdmResult {
            success: false,
            error: String::from_utf8_lossy(&o.stderr).into_owned(),
            ..Default::default()
        },
        Err(e) => DcdmResult {
            success: false,
            error: format!("Failed to run ffmpeg: {e}"),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_pixel(rgb: [u16; 3], colour_space: &str, encoding: DcdmColourEncoding) -> [u16; 3] {
        convert_pixel_to(rgb, colour_space, DcdmTarget::Xyz, encoding)
    }

    fn convert_pixel_to(
        rgb: [u16; 3],
        colour_space: &str,
        target: DcdmTarget,
        encoding: DcdmColourEncoding,
    ) -> [u16; 3] {
        let xf = source_space(colour_space).unwrap().to_target(target);
        let lin = build_linear_lut(xf.gamma);
        let mut bytes = Vec::new();
        for c in rgb {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        let mut out = [0u16; 3];
        convert_frame(&bytes, &xf, &lin, encoding.max_code_value(), &mut out);
        out
    }

    #[test]
    fn reference_white_encodes_to_dci_code_value() {
        // full-scale white is the DCI reference white: 48/52.37 encoded with
        // gamma 2.6 gives the canonical DCDM Y' of 3960 at 12 bits
        let out = convert_pixel([65535; 3], "rec709", DcdmColourEncoding::Xyz12Bit);
        assert_eq!(out[1], 3960);
        // X and Z follow the D65 white point
        assert!(out[0] < out[1] && out[1] < out[2], "got {out:?}");
    }

    #[test]
    fn black_encodes_to_zero() {
        assert_eq!(
            convert_pixel([0; 3], "rec709", DcdmColourEncoding::Xyz12Bit),
            [0, 0, 0]
        );
    }

    #[test]
    fn twelve_and_sixteen_bit_encodings_differ() {
        let twelve = convert_pixel([65535; 3], "rec709", DcdmColourEncoding::Xyz12Bit);
        let sixteen = convert_pixel([65535; 3], "rec709", DcdmColourEncoding::Xyz16Bit);
        assert_eq!(twelve[1], 3960);
        assert!((sixteen[1] as i32 - 63376).abs() <= 2, "got {sixteen:?}");
    }

    #[test]
    fn conversion_is_not_a_passthrough() {
        // guards the defect this replaced, where both encodings mapped to
        // rgb48le and no colour transform happened at all
        let out = convert_pixel([65535, 0, 0], "rec709", DcdmColourEncoding::Xyz16Bit);
        assert_ne!(out, [65535, 0, 0]);
        assert!(
            out[0] > 0 && out[1] > 0 && out[2] > 0,
            "pure red has non-zero X, Y and Z: {out:?}"
        );
    }

    #[test]
    fn rec709_mid_grey_uses_gamma_2_2() {
        // locks the display-referred linearization at gamma 2.2 (libdcp/DoM/grok).
        // white/red/black don't constrain gamma; a mid value does.
        let out = convert_pixel([32768; 3], "rec709", DcdmColourEncoding::Xyz12Bit);
        // independent f64 reference: (0.5^2.2)·white·(48/52.37) then ^(1/2.6)
        let lin = (32768.0f64 / 65535.0).powf(2.2);
        let coeff = 48.0f64 / 52.37;
        let y = (lin * coeff).powf(1.0 / 2.6) * 4095.0;
        assert!(
            (out[1] as f64 - y).abs() <= 2.0,
            "mid-grey Y {} vs gamma-2.2 reference {y:.1}",
            out[1]
        );
    }

    #[test]
    fn p3_and_rec709_share_luma_but_differ_in_chromaticity() {
        let r = convert_pixel([65535; 3], "rec709", DcdmColourEncoding::Xyz12Bit);
        let p = convert_pixel([65535; 3], "p3", DcdmColourEncoding::Xyz12Bit);
        assert_eq!(r[1], p[1], "both normalise Y to the reference white");
        assert_ne!(r[2], p[2], "D65 and DCI white points differ in Z");
    }

    #[test]
    fn xyz_source_requantises_without_rescaling() {
        let out = convert_pixel([30000, 40000, 50000], "xyz", DcdmColourEncoding::Xyz16Bit);
        for (got, want) in out.iter().zip([30000u16, 40000, 50000]) {
            assert!((*got as i32 - want as i32).abs() <= 1, "got {out:?}");
        }
    }

    #[test]
    fn converts_every_pixel_in_a_frame() {
        let xf = source_space("rec709").unwrap().to_target(DcdmTarget::Xyz);
        let lin = build_linear_lut(xf.gamma);
        let mut bytes = Vec::new();
        for px in [[65535u16; 3], [0; 3], [65535; 3]] {
            for c in px {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        let mut out = vec![0u16; 9];
        convert_frame(&bytes, &xf, &lin, 4095, &mut out);
        assert_eq!(out[1], 3960);
        assert_eq!(&out[3..6], &[0, 0, 0]);
        assert_eq!(out[7], 3960);
    }

    #[test]
    fn p3d65_matrix_matches_published() {
        // derived P3-D65 RGB->XYZ (from primaries) vs the published SMPTE RP 431-2
        // matrix (colour-science reference), within f32 rounding.
        let m = rgb_to_xyz_matrix(&P3_PRIMARIES, D65_WHITE);
        let pub_m = [
            [0.4865709, 0.2656677, 0.1982173],
            [0.2289746, 0.6917385, 0.0792869],
            [0.0, 0.0451134, 1.0439444],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (m[i][j] - pub_m[i][j]).abs() < 1e-6,
                    "[{i}][{j}] {} vs {}",
                    m[i][j],
                    pub_m[i][j]
                );
            }
        }
    }

    #[test]
    fn p3d65_white_maps_to_full_scale_neutral() {
        // rec709 white is D65; through the P3-D65 target it lands on the P3-D65
        // white, i.e. neutral (1,1,1) linear, encoded at full 12-bit scale.
        let out = convert_pixel_to(
            [65535; 3],
            "rec709",
            DcdmTarget::P3D65,
            DcdmColourEncoding::Xyz12Bit,
        );
        for c in out {
            assert_eq!(c, 4095, "P3-D65 white not full-scale neutral: {out:?}");
        }
    }

    #[test]
    fn p3d65_red_matches_reference() {
        // rec709 red (1,0,0) in linear P3-D65 is (0.822462, 0.033194, 0.017083),
        // then gamma 2.6 to 12-bit. independent of the impl (f64 reference).
        let lin = [0.822_462f64, 0.033_194, 0.017_083];
        let want: [u16; 3] =
            std::array::from_fn(|i| (lin[i].powf(1.0 / 2.6) * 4095.0).round() as u16);
        let got = convert_pixel_to(
            [65535, 0, 0],
            "rec709",
            DcdmTarget::P3D65,
            DcdmColourEncoding::Xyz12Bit,
        );
        for i in 0..3 {
            assert!(
                (got[i] as i32 - want[i] as i32).abs() <= 3,
                "channel {i}: got {got:?} want {want:?}"
            );
        }
        // rec709 red sits inside the wider P3 gamut, so no channel clips or goes negative.
        assert!(got[0] > got[1] && got[1] > got[2], "red ordering: {got:?}");
    }

    #[test]
    fn p3d65_differs_from_xyz_target() {
        let xyz = convert_pixel_to(
            [40000, 50000, 20000],
            "rec709",
            DcdmTarget::Xyz,
            DcdmColourEncoding::Xyz12Bit,
        );
        let p3 = convert_pixel_to(
            [40000, 50000, 20000],
            "rec709",
            DcdmTarget::P3D65,
            DcdmColourEncoding::Xyz12Bit,
        );
        assert_ne!(xyz, p3);
    }

    #[test]
    fn rejects_colour_spaces_it_cannot_transform() {
        assert!(source_space("aces").is_err());
        assert!(source_space("acescg").is_err());
        assert!(source_space("logc").is_err());
        assert!(source_space("rec709").is_ok());
        assert!(source_space("P3").is_ok(), "names are case insensitive");
        assert!(source_space("").is_ok(), "empty defaults to rec709");
    }
}
