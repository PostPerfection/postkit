use std::path::{Path, PathBuf};

/// Loaded TIFF frame: planar int32 component buffers + metadata.
pub struct TiffFrame {
    pub components: [Vec<i32>; 3],
    pub width: u32,
    pub height: u32,
    pub precision: u8,
    pub path: PathBuf,
}

/// Load a TIFF file into planar int32 component buffers.
///
/// Supports 8, 12, 16-bit RGB TIFFs. Returns 3 planar buffers (R, G, B).
pub fn load_tiff(path: &Path) -> Result<TiffFrame, String> {
    use std::io::{BufReader, Read, Seek, SeekFrom};
    use tiff::decoder::Decoder;
    use tiff::tags::Tag;

    let file =
        std::fs::File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut decoder = Decoder::new(&mut reader)
        .map_err(|e| format!("TIFF decode error for {}: {e}", path.display()))?;

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| format!("TIFF dimensions error: {e}"))?;

    // Read bits per sample (may be stored as a vector, one per component)
    let bits_per_sample = decoder
        .get_tag_u32_vec(Tag::BitsPerSample)
        .map(|v| v[0] as u8)
        .or_else(|_| decoder.get_tag_u32(Tag::BitsPerSample).map(|v| v as u8))
        .map_err(|e| format!("Cannot read BitsPerSample: {e}"))?;

    // Read samples per pixel
    let samples_per_pixel = decoder.get_tag_u32(Tag::SamplesPerPixel).unwrap_or(3) as u8;
    if samples_per_pixel < 3 {
        return Err(format!("Need ≥3 samples/pixel, got {}", samples_per_pixel));
    }

    let num_pixels = (width as usize) * (height as usize);

    // For standard bit depths (8, 16), use the tiff crate decoder
    if bits_per_sample == 8 || bits_per_sample == 16 {
        let image = decoder
            .read_image()
            .map_err(|e| format!("TIFF read error for {}: {e}", path.display()))?;

        let mut r = Vec::with_capacity(num_pixels);
        let mut g = Vec::with_capacity(num_pixels);
        let mut b = Vec::with_capacity(num_pixels);

        match image {
            tiff::decoder::DecodingResult::U8(data) => {
                let ch = samples_per_pixel as usize;
                for i in 0..num_pixels {
                    r.push(data[i * ch] as i32);
                    g.push(data[i * ch + 1] as i32);
                    b.push(data[i * ch + 2] as i32);
                }
            }
            tiff::decoder::DecodingResult::U16(data) => {
                let ch = samples_per_pixel as usize;
                for i in 0..num_pixels {
                    r.push(data[i * ch] as i32);
                    g.push(data[i * ch + 1] as i32);
                    b.push(data[i * ch + 2] as i32);
                }
            }
            _ => return Err("Unsupported TIFF sample format".to_string()),
        }

        return Ok(TiffFrame {
            components: [r, g, b],
            width,
            height,
            precision: bits_per_sample,
            path: path.to_path_buf(),
        });
    }

    // For packed bit depths (e.g. 12-bit), read raw strip data and unpack
    if bits_per_sample != 12 {
        return Err(format!("Unsupported bits/sample: {}", bits_per_sample));
    }

    // Get strip offsets and byte counts
    let strip_offsets = decoder
        .get_tag_u64_vec(Tag::StripOffsets)
        .map_err(|e| format!("Cannot read StripOffsets: {e}"))?;
    let strip_byte_counts = decoder
        .get_tag_u64_vec(Tag::StripByteCounts)
        .map_err(|e| format!("Cannot read StripByteCounts: {e}"))?;

    // Read all strip data
    let total_bytes: u64 = strip_byte_counts.iter().sum();
    let mut raw_data = Vec::with_capacity(total_bytes as usize);
    // Need to get inner reader back from decoder
    drop(decoder);
    for (offset, count) in strip_offsets.iter().zip(strip_byte_counts.iter()) {
        reader
            .seek(SeekFrom::Start(*offset))
            .map_err(|e| format!("Seek error: {e}"))?;
        let mut buf = vec![0u8; *count as usize];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("Read error: {e}"))?;
        raw_data.extend_from_slice(&buf);
    }

    // Unpack 12-bit packed samples (interleaved RGB)
    // Each pair of 12-bit values is stored in 3 bytes: [A₁₁..A₄ | A₃..A₀ B₁₁..B₈ | B₇..B₀]
    let total_samples = num_pixels * samples_per_pixel as usize;
    let mut samples = Vec::with_capacity(total_samples);
    let mut byte_idx = 0usize;
    let mut sample_idx = 0usize;
    while sample_idx < total_samples {
        if byte_idx + 2 >= raw_data.len() {
            break;
        }
        if sample_idx + 1 < total_samples {
            // Two 12-bit samples from 3 bytes
            let b0 = raw_data[byte_idx] as u16;
            let b1 = raw_data[byte_idx + 1] as u16;
            let b2 = raw_data[byte_idx + 2] as u16;
            let s0 = (b0 << 4) | (b1 >> 4);
            let s1 = ((b1 & 0x0F) << 8) | b2;
            samples.push(s0 as i32);
            samples.push(s1 as i32);
            byte_idx += 3;
            sample_idx += 2;
        } else {
            // Odd last sample
            let b0 = raw_data[byte_idx] as u16;
            let b1 = raw_data[byte_idx + 1] as u16;
            let s0 = (b0 << 4) | (b1 >> 4);
            samples.push(s0 as i32);
            byte_idx += 2;
            sample_idx += 1;
        }
    }

    // De-interleave into planar buffers
    let ch = samples_per_pixel as usize;
    let mut r = Vec::with_capacity(num_pixels);
    let mut g = Vec::with_capacity(num_pixels);
    let mut b = Vec::with_capacity(num_pixels);
    for i in 0..num_pixels {
        r.push(samples[i * ch]);
        g.push(samples[i * ch + 1]);
        b.push(samples[i * ch + 2]);
    }

    Ok(TiffFrame {
        components: [r, g, b],
        width,
        height,
        precision: bits_per_sample,
        path: path.to_path_buf(),
    })
}

/// Find the grk_compress binary.
/// Searches: 1) $HOME/bin/grok/bin/ 2) PATH via `which`
pub fn find_grk_compress() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join("bin/grok/bin/grk_compress");
        if p.exists() {
            return Some(p);
        }
    }
    // Check PATH
    std::process::Command::new("which")
        .arg("grk_compress")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
}

/// Compress a single TIFF to J2C by spawning a `grk_compress` subprocess.
///
/// Uses `-H 1` (single thread) so the caller can run many in parallel.
#[allow(clippy::too_many_arguments)]
pub fn compress_file_subprocess(
    grk_bin: &Path,
    lib_path: &str,
    input: &Path,
    output: &Path,
    ratio: f64,
    num_resolutions: u8,
    codeblock_size: u32,
    progression: &str,
) -> Result<(), String> {
    let status = std::process::Command::new(grk_bin)
        .env("LD_LIBRARY_PATH", lib_path)
        .args([
            "-i",
            &input.to_string_lossy(),
            "-o",
            &output.to_string_lossy(),
            "-r",
            &format!("{}", ratio),
            "--xyz",
            "-n",
            &format!("{}", num_resolutions),
            "-b",
            &format!("{},{}", codeblock_size, codeblock_size),
            "-p",
            progression,
            "-H",
            "1",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("Failed to spawn grk_compress: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "grk_compress failed with status {} for {}",
            status,
            input.display()
        ))
    }
}

/// Get the LD_LIBRARY_PATH for grok libraries.
pub fn grok_lib_path() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let p = format!("{}/bin/grok/lib64", home);
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    String::new()
}
