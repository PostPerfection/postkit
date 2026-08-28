use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::colour::{ColourSpace, DcdmTransform};

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

    let transform = match source_transform(&opts.colour_space, opts.target) {
        Ok(t) => t,
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

    let list_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            return DcdmResult {
                success: false,
                error: format!("Failed to create a working directory: {e}"),
                ..Default::default()
            };
        }
    };
    let frame_list = list_dir.path().join("frames.ffconcat");
    if let Err(e) = crate::encode::write_image_concat_list(
        &frames,
        crate::encode::FrameRate::new(opts.fps_num, opts.fps_den),
        &frame_list,
    ) {
        return DcdmResult {
            success: false,
            error: e,
            ..Default::default()
        };
    }

    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-y", "-v", "error", "-nostats", "-f", "concat", "-safe", "0", "-i",
    ])
    .arg(&frame_list);

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
        .stderr(std::process::Stdio::piped());

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

    let max_code = opts.encoding.max_code_value();
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

        transform.frame_rgb48le(&frame_buf, max_code, &mut xyz_buf);

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

    let mut stderr_text = String::new();
    if let Some(mut stderr) = ffmpeg.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut stderr, &mut stderr_text);
    }
    match ffmpeg.wait() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            return DcdmResult {
                success: false,
                error: format!("ffmpeg exited with {s}: {}", stderr_text.trim()),
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

impl DcdmColourEncoding {
    /// Largest code value the encoding quantises to.
    fn max_code_value(self) -> u16 {
        match self {
            DcdmColourEncoding::Xyz12Bit => 4095,
            DcdmColourEncoding::Xyz16Bit => 65535,
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

/// Resolve a source colour space name to the [`ColourSpace`] it names.
///
/// An empty name is treated as Rec.709, matching the wizard CLIs' own default.
fn source_space(name: &str) -> Result<ColourSpace, String> {
    match name.trim().to_lowercase().as_str() {
        "" | "rec709" | "bt709" | "srgb" => Ok(ColourSpace::Rec709),
        "p3" | "dcip3" | "dci-p3" | "p3dci" | "smpte431" => Ok(ColourSpace::P3),
        "rec2020" | "bt2020" => Ok(ColourSpace::Rec2020),
        "xyz" | "ciexyz" => Ok(ColourSpace::Xyz),
        "logc" | "logc3" | "arrilogc" => Ok(ColourSpace::LogC),
        other => Err(format!(
            "Unsupported source colour space '{other}' for DCDM conversion. \
             Use rec709, p3, rec2020, xyz or logc, or supply a 3D LUT that lands in one of those."
        )),
    }
}

/// The per-frame transform from a named source colour space into `target`.
fn source_transform(name: &str, target: DcdmTarget) -> Result<DcdmTransform, String> {
    let space = source_space(name)?;
    let source = crate::colour::source_space(space)?;
    Ok(match target {
        DcdmTarget::Xyz => {
            DcdmTransform::new(space, source.to_xyz, source.linearisation, source.scale)
        }
        DcdmTarget::P3D65 => DcdmTransform::new(
            space,
            // linear source RGB -> XYZ -> P3-D65 linear RGB
            mat_mul(&xyz_to_p3d65(), &source.to_xyz),
            source.linearisation,
            // rgb mastering target: source white -> full-scale, no dci companding
            1.0,
        ),
    })
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

/// Rate of the review movie: `export_dcdm` takes no rate and cinema material is 24.
const DCDM_REVIEW_FPS: u32 = 24;

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

    let frames: Vec<PathBuf> = match crate::encode::find_source_frames(dcdm_dir) {
        Ok(f) => f
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "tif"))
            .collect(),
        Err(e) => {
            return DcdmResult {
                success: false,
                error: format!("Failed to list {}: {e}", dcdm_dir.display()),
                ..Default::default()
            };
        }
    };
    let list_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            return DcdmResult {
                success: false,
                error: format!("Failed to create a working directory: {e}"),
                ..Default::default()
            };
        }
    };
    let frame_list = list_dir.path().join("frames.ffconcat");
    if let Err(e) = crate::encode::write_image_concat_list(
        &frames,
        crate::encode::FrameRate::whole(DCDM_REVIEW_FPS),
        &frame_list,
    ) {
        return DcdmResult {
            success: false,
            error: e,
            ..Default::default()
        };
    }
    let output_file = output_dir.join("review.mov");

    let colour_filter = match target_colour_space {
        Some("rec709") | Some("Rec709") | None => "colorspace=all=bt709:iall=bt709",
        Some("p3") | Some("P3") => "colorspace=all=bt709:iprimaries=smpte431",
        Some(_) => "colorspace=all=bt709",
    };

    let output = std::process::Command::new("ffmpeg")
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&frame_list)
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
        let transform = source_transform(colour_space, target).unwrap();
        let mut bytes = Vec::new();
        for c in rgb {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        let mut out = [0u16; 3];
        transform.frame_rgb48le(&bytes, encoding.max_code_value(), &mut out);
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
        let transform = source_transform("rec709", DcdmTarget::Xyz).unwrap();
        let mut bytes = Vec::new();
        for px in [[65535u16; 3], [0; 3], [65535; 3]] {
            for c in px {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        let mut out = vec![0u16; 9];
        transform.frame_rgb48le(&bytes, 4095, &mut out);
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
        assert!(source_space("rec709").is_ok());
        assert!(source_space("logc").is_ok());
        assert!(source_space("P3").is_ok(), "names are case insensitive");
        assert!(source_space("").is_ok(), "empty defaults to rec709");
    }

    #[test]
    fn the_public_transform_matches_the_file_pipeline() {
        // create_dcdm and DcdmTransform::to_xyz must be one transform, not two
        // implementations of it
        for space in [ColourSpace::Rec709, ColourSpace::P3, ColourSpace::Rec2020] {
            let name = format!("{space:?}").to_lowercase();
            let public = DcdmTransform::to_xyz(space).unwrap();
            for rgb in [[65535u16, 0, 0], [0, 32768, 12345], [65535; 3], [0; 3]] {
                assert_eq!(
                    public.pixel(rgb, 4095),
                    convert_pixel(rgb, &name, DcdmColourEncoding::Xyz12Bit),
                    "{space:?} {rgb:?}"
                );
            }
        }
    }
}
