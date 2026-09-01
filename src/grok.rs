use std::path::{Path, PathBuf};

/// Loaded TIFF frame: planar int32 component buffers + metadata.
pub struct TiffFrame {
    pub components: [Vec<i32>; 3],
    pub width: u32,
    pub height: u32,
    pub precision: u8,
    pub path: PathBuf,
}

/// The sample depth of a packed rgb48be frame.
const RGB48_PRECISION: u8 = 16;

impl TiffFrame {
    /// The frame as packed big-endian 16-bit RGB, the form the encoder threads
    /// burn subtitles and convert colour on. Shallower samples are shifted up
    /// to 16 bits, so a 12-bit still comes back exactly once the encoder
    /// shifts it down again.
    pub fn into_rgb48be_frame(self, index: u64) -> crate::grok_encoder::RawFrame {
        let shift = RGB48_PRECISION - self.precision;
        let [r, g, b] = self.components;
        let mut data = Vec::with_capacity(r.len() * 6);
        for ((r, g), b) in r.iter().zip(&g).zip(&b) {
            for sample in [r, g, b] {
                data.extend_from_slice(&((*sample as u16) << shift).to_be_bytes());
            }
        }
        crate::grok_encoder::RawFrame::Packed {
            data,
            width: self.width,
            height: self.height,
            precision: RGB48_PRECISION,
            index,
        }
    }
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

    // 12-bit rows are bit packed and padded to a byte
    let ch = samples_per_pixel as usize;
    let row_bytes = packed_row_bytes(width as usize * ch, PACKED_12BIT_SAMPLE_BITS);
    if raw_data.len() < row_bytes * height as usize {
        return Err(format!(
            "{} holds {} bytes of strips, fewer than its {}x{} 12-bit raster needs",
            path.display(),
            raw_data.len(),
            width,
            height
        ));
    }
    let mut samples = Vec::with_capacity(num_pixels * ch);
    for row in raw_data.chunks_exact(row_bytes).take(height as usize) {
        unpack_12bit_row(row, width as usize * ch, &mut samples);
    }

    // De-interleave into planar buffers
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

/// Bits a packed 12-bit sample takes.
const PACKED_12BIT_SAMPLE_BITS: usize = 12;
const BITS_PER_BYTE: usize = 8;
/// A TIFF row of packed samples is padded to a whole byte.
fn packed_row_bytes(samples_per_row: usize, bits_per_sample: usize) -> usize {
    (samples_per_row * bits_per_sample).div_ceil(BITS_PER_BYTE)
}

/// Unpack one row of big-endian 12-bit samples: two samples share three bytes.
fn unpack_12bit_row(row: &[u8], sample_count: usize, samples: &mut Vec<i32>) {
    let mut accumulator: u32 = 0;
    let mut pending_bits = 0usize;
    let mut unpacked = 0usize;
    for byte in row {
        accumulator = (accumulator << BITS_PER_BYTE) | u32::from(*byte);
        pending_bits += BITS_PER_BYTE;
        while pending_bits >= PACKED_12BIT_SAMPLE_BITS && unpacked < sample_count {
            pending_bits -= PACKED_12BIT_SAMPLE_BITS;
            samples.push(((accumulator >> pending_bits) & 0xfff) as i32);
            unpacked += 1;
        }
        // keep only the bits still owed to the next sample
        accumulator &= (1 << pending_bits) - 1;
    }
}

/// Pack one row of 12-bit samples big-endian, two to three bytes, padded to a
/// whole byte, the layout [`load_tiff`] reads back.
fn pack_12bit_row(samples: &[u16], packed: &mut Vec<u8>) {
    let mut accumulator: u32 = 0;
    let mut pending_bits = 0usize;
    for sample in samples {
        accumulator = (accumulator << PACKED_12BIT_SAMPLE_BITS) | u32::from(*sample & 0xfff);
        pending_bits += PACKED_12BIT_SAMPLE_BITS;
        while pending_bits >= BITS_PER_BYTE {
            pending_bits -= BITS_PER_BYTE;
            packed.push(((accumulator >> pending_bits) & 0xff) as u8);
        }
        accumulator &= (1 << pending_bits) - 1;
    }
    if pending_bits > 0 {
        packed.push(((accumulator << (BITS_PER_BYTE - pending_bits)) & 0xff) as u8);
    }
}

/// TIFF field types.
const TIFF_SHORT: u16 = 3;
const TIFF_LONG: u16 = 4;
/// The tags an uncompressed contiguous RGB TIFF needs, in ascending order.
const TIFF_IFD_ENTRY_COUNT: u16 = 10;
const TIFF_IFD_ENTRY_BYTES: u32 = 12;
const TIFF_HEADER_BYTES: u32 = 8;
const TIFF_RGB_SAMPLES_PER_PIXEL: u16 = 3;

/// Write an uncompressed RGB TIFF at `precision` bits a sample (8, 12 or 16)
/// from pixel-interleaved samples, the file [`load_tiff`] reads back exactly.
/// 12-bit samples are bit-packed the way grok and libtiff write them.
pub fn write_tiff_rgb(
    path: &Path,
    width: u32,
    height: u32,
    precision: u8,
    samples: &[u16],
) -> Result<(), String> {
    let sample_count = width as usize * height as usize * TIFF_RGB_SAMPLES_PER_PIXEL as usize;
    if samples.len() != sample_count {
        return Err(format!(
            "{}x{} RGB is {sample_count} samples, {} were given",
            width,
            height,
            samples.len()
        ));
    }
    let samples_per_row = width as usize * TIFF_RGB_SAMPLES_PER_PIXEL as usize;
    let pixels: Vec<u8> = match precision {
        8 => samples.iter().map(|s| *s as u8).collect(),
        12 => {
            let mut packed = Vec::with_capacity(
                packed_row_bytes(samples_per_row, PACKED_12BIT_SAMPLE_BITS) * height as usize,
            );
            for row in samples.chunks_exact(samples_per_row) {
                pack_12bit_row(row, &mut packed);
            }
            packed
        }
        16 => samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        other => {
            return Err(format!(
                "a TIFF is written at 8, 12 or 16 bits, not {other}"
            ));
        }
    };

    let ifd_bytes = 2 + u32::from(TIFF_IFD_ENTRY_COUNT) * TIFF_IFD_ENTRY_BYTES + 4;
    let bits_per_sample_offset = TIFF_HEADER_BYTES + ifd_bytes;
    let pixels_offset = bits_per_sample_offset + 2 * u32::from(TIFF_RGB_SAMPLES_PER_PIXEL);
    let entries: [(u16, u16, u32, u32); TIFF_IFD_ENTRY_COUNT as usize] = [
        (256, TIFF_LONG, 1, width),
        (257, TIFF_LONG, 1, height),
        (258, TIFF_SHORT, 3, bits_per_sample_offset),
        (259, TIFF_SHORT, 1, 1),
        (262, TIFF_SHORT, 1, 2),
        (273, TIFF_LONG, 1, pixels_offset),
        (277, TIFF_SHORT, 1, u32::from(TIFF_RGB_SAMPLES_PER_PIXEL)),
        (278, TIFF_LONG, 1, height),
        (279, TIFF_LONG, 1, pixels.len() as u32),
        (284, TIFF_SHORT, 1, 1),
    ];

    let mut tiff: Vec<u8> = Vec::with_capacity(pixels_offset as usize + pixels.len());
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&TIFF_HEADER_BYTES.to_le_bytes());
    tiff.extend_from_slice(&TIFF_IFD_ENTRY_COUNT.to_le_bytes());
    for (tag, field_type, count, value) in entries {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&field_type.to_le_bytes());
        tiff.extend_from_slice(&count.to_le_bytes());
        if field_type == TIFF_SHORT && count == 1 {
            tiff.extend_from_slice(&(value as u16).to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
        } else {
            tiff.extend_from_slice(&value.to_le_bytes());
        }
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..TIFF_RGB_SAMPLES_PER_PIXEL {
        tiff.extend_from_slice(&u16::from(precision).to_le_bytes());
    }
    debug_assert_eq!(tiff.len() as u32, pixels_offset);
    tiff.extend_from_slice(&pixels);
    std::fs::write(path, tiff).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(width: u32, height: u32, full_scale: u16) -> Vec<u16> {
        (0..width * height * 3)
            .map(|i| (i * 37 % (u32::from(full_scale) + 1)) as u16)
            .collect()
    }

    #[test]
    fn a_written_tiff_reads_back_at_every_depth() {
        let dir = tempfile::tempdir().unwrap();
        // an odd width makes a 12-bit row end mid-byte
        for (precision, width) in [(8u8, 6u32), (12, 5), (12, 6), (16, 7)] {
            let height = 3;
            let full_scale = ((1u32 << precision) - 1) as u16;
            let samples = ramp(width, height, full_scale);
            let path = dir.path().join(format!("{precision}bit_{width}.tif"));
            write_tiff_rgb(&path, width, height, precision, &samples).unwrap();
            let read = load_tiff(&path).unwrap();
            assert_eq!(
                (read.width, read.height, read.precision),
                (width, height, precision)
            );
            let mut interleaved = Vec::new();
            for i in 0..(width * height) as usize {
                for component in &read.components {
                    interleaved.push(component[i] as u16);
                }
            }
            assert_eq!(interleaved, samples, "{precision}-bit {width} wide");
        }
    }

    #[test]
    fn a_tiff_frame_packs_to_rgb48_with_its_samples_scaled_to_16_bits() {
        let frame = TiffFrame {
            components: [vec![0xfff, 1], vec![0, 0x800], vec![0x7ff, 0]],
            width: 2,
            height: 1,
            precision: 12,
            path: PathBuf::new(),
        };
        let crate::grok_encoder::RawFrame::Packed {
            data,
            width,
            height,
            precision,
            index,
        } = frame.into_rgb48be_frame(7)
        else {
            panic!("a tiff frame packs")
        };
        assert_eq!((width, height, precision, index), (2, 1, 16, 7));
        assert_eq!(
            data,
            vec![
                0xff, 0xf0, 0x00, 0x00, 0x7f, 0xf0, 0x00, 0x10, 0x80, 0x00, 0x00, 0x00
            ]
        );
    }
}
