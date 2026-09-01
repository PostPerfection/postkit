use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Visible watermark options.
///
/// This burns a plainly visible text mark into each frame with ffmpeg drawtext.
/// It is NOT invisible/forensic watermarking and carries no recoverable payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatermarkOptions {
    pub operator_id: String,
    pub session_id: String,
    pub strength: f32,
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
}

/// Watermark operation result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatermarkResult {
    pub success: bool,
    pub error: String,
    pub frames_processed: u64,
    pub payload_hash: String,
}

/// Burn a visible text mark (first 8 hex chars of the operator/session hash plus
/// the session id) into each frame with ffmpeg drawtext. Plainly visible, not
/// forensic.
/// The concat list needs a rate and the output is stills, so any rate does.
const FRAME_LIST_FPS: u32 = 24;

pub fn embed_watermark(opts: &WatermarkOptions) -> WatermarkResult {
    if let Err(e) = std::fs::create_dir_all(&opts.output_dir) {
        return WatermarkResult {
            success: false,
            error: format!("Failed to create output directory: {e}"),
            ..Default::default()
        };
    }

    // Payload hash from operator + session IDs, shown in the visible mark.
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(opts.operator_id.as_bytes());
    hasher.update(opts.session_id.as_bytes());
    let payload_hash = hex::encode(hasher.finalize());

    let mut frames: Vec<PathBuf> = std::fs::read_dir(&opts.input_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "tif" | "tiff" | "dpx" | "exr" | "png" | "jpg"))
        })
        .collect();
    frames.sort();

    if frames.is_empty() {
        return WatermarkResult {
            success: false,
            error: "No image frames found in input directory".into(),
            ..Default::default()
        };
    }

    let strength = if opts.strength > 0.0 {
        (opts.strength * 255.0) as u8
    } else {
        8 // very faint by default
    };

    let ext = frames
        .first()
        .and_then(|f| f.extension())
        .and_then(|e| e.to_str())
        .unwrap_or("tif");

    let list_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            return WatermarkResult {
                success: false,
                error: format!("Failed to create a working directory: {e}"),
                ..Default::default()
            };
        }
    };
    let frame_list = list_dir.path().join("frames.ffconcat");
    if let Err(e) = crate::encode::write_image_concat_list(
        &frames,
        crate::encode::FrameRate::whole(FRAME_LIST_FPS),
        &frame_list,
    ) {
        return WatermarkResult {
            success: false,
            error: e,
            ..Default::default()
        };
    }
    let output_pattern = opts.output_dir.join(format!("%06d.{ext}"));

    // Keep the mark to characters drawtext handles without filtergraph escaping
    // headaches (a raw ':' or '\'' in the text breaks ffmpeg's filter parser).
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    let watermark_text = format!("{}_{}", &payload_hash[..8], sanitize(&opts.session_id));
    let filter = format!(
        "drawtext=text='{watermark_text}':fontsize=10:fontcolor=white@0.{:02}:x=10:y=h-20",
        strength.min(99)
    );

    let output = std::process::Command::new("ffmpeg")
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&frame_list)
        .arg("-vf")
        .arg(&filter)
        .arg(&output_pattern)
        .output();

    match output {
        Ok(o) if o.status.success() => WatermarkResult {
            success: true,
            error: String::new(),
            frames_processed: frames.len() as u64,
            payload_hash,
        },
        Ok(o) => WatermarkResult {
            success: false,
            error: String::from_utf8_lossy(&o.stderr).into_owned(),
            ..Default::default()
        },
        Err(e) => WatermarkResult {
            success: false,
            error: format!("Failed to run ffmpeg: {e}"),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FRAME_WIDTH: usize = 160;
    const FRAME_HEIGHT: usize = 64;
    const FRAME_COUNT: usize = 3;
    /// The mark sits at y = h - 20 in a 10 pixel font, so nothing above this row
    /// is drawn on.
    const FIRST_MARKED_ROW: usize = FRAME_HEIGHT - 22;

    fn read_png_rgb(path: &Path) -> (usize, usize, Vec<u8>) {
        let file = std::fs::File::open(path).expect("open output frame");
        let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().expect("png header");
        let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buffer).expect("png pixels");
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        buffer.truncate(info.buffer_size());
        (info.width as usize, info.height as usize, buffer)
    }

    #[test]
    fn embed_watermark_draws_the_mark_and_leaves_the_rest_of_the_frame_alone() {
        let dir = tempfile::tempdir().unwrap();
        let input_dir = dir.path().join("in");
        std::fs::create_dir_all(&input_dir).unwrap();

        let ffmpeg = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c=red:s={FRAME_WIDTH}x{FRAME_HEIGHT}:r=1"),
                "-frames:v",
                &FRAME_COUNT.to_string(),
            ])
            .arg(input_dir.join("f_%03d.png"))
            .output()
            .expect("ffmpeg");
        assert!(
            ffmpeg.status.success(),
            "{}",
            String::from_utf8_lossy(&ffmpeg.stderr)
        );

        let output_dir = dir.path().join("out");
        let result = embed_watermark(&WatermarkOptions {
            operator_id: "operator-7".into(),
            session_id: "session-42".into(),
            strength: 0.9,
            input_dir: input_dir.clone(),
            output_dir: output_dir.clone(),
        });
        assert!(result.success, "{}", result.error);
        assert_eq!(result.frames_processed, FRAME_COUNT as u64);
        assert_eq!(result.payload_hash.len(), 64);

        let (_, _, source) = read_png_rgb(&input_dir.join("f_001.png"));
        let (width, height, marked) = read_png_rgb(&output_dir.join("000001.png"));
        assert_eq!((width, height), (FRAME_WIDTH, FRAME_HEIGHT));

        let untouched = FIRST_MARKED_ROW * FRAME_WIDTH * 3;
        assert_eq!(
            marked[..untouched],
            source[..untouched],
            "picture above the mark changed"
        );

        let drawn = marked[untouched..]
            .as_chunks::<3>()
            .0
            .iter()
            .zip(source[untouched..].as_chunks::<3>().0)
            .filter(|(after, before)| after != before)
            .count();
        assert!(drawn > 20, "only {drawn} pixels carry the mark");
    }
}
