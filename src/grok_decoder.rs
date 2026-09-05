//! In-process JPEG 2000 decoding via Grok FFI.
//!
//! The codestream goes in as bytes and the samples come out as planar
//! components, so nothing is written to disk and no process is spawned. That
//! matters at DCP bitrates: ffmpeg's software J2K decoder manages a few frames a
//! second on a 2K 125 Mb/s track where grok manages fifteen, and a decode that
//! spawns a process per frame pays for it again.
//!
//! `reduce` discards that many highest resolution levels, so 1 is half the width
//! and height and 2 is a quarter. A scrub thumbnail and a black-frame test need
//! no more than that, and a quarter-resolution decode is several times faster
//! again.
//!
//! Enable with the `grok-ffi` cargo feature. Without it every call here refuses
//! by name: falling back to a decoder that runs at a few frames a second would
//! read as a hang rather than as a slower path.

/// One decoded frame, one buffer per component in row order with no padding.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Bits per sample, as the codestream declares them.
    pub precision: u8,
    pub components: Vec<Vec<i32>>,
    /// A component came back subsampled and was upsampled to the frame size.
    /// ST 2067-21 allows 4:2:2 only as CDCI, so such a frame is YCbCr and a
    /// 4:4:4 one is RGB.
    pub chroma_subsampled: bool,
}

/// The picture components a DCP frame carries, X'Y'Z'.
const DCI_COMPONENT_COUNT: usize = 3;
/// Bits per sample DCI requires, and what the packed layout below carries.
const DCI_PRECISION_BITS: u32 = 12;
/// xyz12le holds each 12-bit sample in the high bits of a 16-bit word.
const XYZ12LE_SAMPLE_SHIFT: u32 = 4;

impl DecodedFrame {
    /// The three components pixel-interleaved at the codestream's own
    /// precision, the order an image file holds them in.
    pub fn interleaved_samples(&self) -> Result<Vec<u16>, String> {
        if self.components.len() != DCI_COMPONENT_COUNT {
            return Err(format!(
                "a picture frame has {DCI_COMPONENT_COUNT} components, this codestream has {}",
                self.components.len()
            ));
        }
        let samples = self.width as usize * self.height as usize;
        let mut interleaved = Vec::with_capacity(samples * DCI_COMPONENT_COUNT);
        for sample in 0..samples {
            for component in &self.components {
                interleaved.push(component[sample].clamp(0, u16::MAX as i32) as u16);
            }
        }
        Ok(interleaved)
    }

    /// Pack the components into ffmpeg's `xyz12le` layout, which is what
    /// postkit's colour transforms read: X, Y, Z per pixel, each a
    /// little-endian 16-bit word holding its 12-bit sample in the high bits.
    ///
    /// A codestream at another precision is normalised to 12 bits rather than
    /// refused, because the shift is the only thing that changes and a wrong
    /// shift is a silently dark or blown-out picture.
    pub fn to_xyz12le(&self) -> Result<Vec<u8>, String> {
        if self.components.len() != DCI_COMPONENT_COUNT {
            return Err(format!(
                "a picture frame has {DCI_COMPONENT_COUNT} components, this codestream has {}",
                self.components.len()
            ));
        }
        let samples = self.width as usize * self.height as usize;
        for (index, component) in self.components.iter().enumerate() {
            if component.len() != samples {
                return Err(format!(
                    "component {index} holds {} samples, not the {samples} the frame is",
                    component.len()
                ));
            }
        }

        let mut packed = Vec::with_capacity(samples * DCI_COMPONENT_COUNT * 2);
        let (x, y, z) = (
            &self.components[0],
            &self.components[1],
            &self.components[2],
        );
        for sample in 0..samples {
            for component in [x, y, z] {
                let value = to_twelve_bits(component[sample], self.precision);
                packed.extend_from_slice(&(value << XYZ12LE_SAMPLE_SHIFT).to_le_bytes());
            }
        }
        Ok(packed)
    }
}

/// One sample at `precision` bits, as a 12-bit value clamped into range.
fn to_twelve_bits(sample: i32, precision: u8) -> u16 {
    let precision = u32::from(precision).max(1);
    let shifted = match precision.cmp(&DCI_PRECISION_BITS) {
        std::cmp::Ordering::Equal => sample,
        std::cmp::Ordering::Greater => sample >> (precision - DCI_PRECISION_BITS),
        std::cmp::Ordering::Less => sample << (DCI_PRECISION_BITS - precision),
    };
    shifted.clamp(0, (1 << DCI_PRECISION_BITS) - 1) as u16
}

/// Decode a codestream held in memory, discarding `reduce` highest resolution
/// levels (0 for full resolution).
///
/// Takes the buffer by value because grok reads the stream in place.
#[cfg(feature = "grok-ffi")]
pub fn decode(codestream: Vec<u8>, reduce: u8) -> Result<DecodedFrame, String> {
    decode_with_threads(codestream, reduce, 0)
}

/// [`decode`] on `num_threads` grok threads, 0 for the shared pool and 1 for
/// the calling thread alone.
#[cfg(feature = "grok-ffi")]
pub fn decode_with_threads(
    mut codestream: Vec<u8>,
    reduce: u8,
    num_threads: u32,
) -> Result<DecodedFrame, String> {
    use grokj2k_sys::{
        grk_decompress, grk_decompress_init, grk_decompress_parameters, grk_decompress_read_header,
        grk_header_info, grk_image, grk_object_unref, grk_stream_params,
    };

    if codestream.is_empty() {
        return Err("cannot decode an empty codestream".to_string());
    }

    // the codec is released on every path out of here, including the error ones
    struct Codec(*mut grokj2k_sys::grk_object);
    impl Drop for Codec {
        fn drop(&mut self) {
            unsafe { grk_object_unref(self.0) };
        }
    }

    unsafe {
        let mut stream_params: grk_stream_params = std::mem::zeroed();
        stream_params.buf = codestream.as_mut_ptr();
        stream_params.buf_len = codestream.len();

        let mut params: grk_decompress_parameters = std::mem::zeroed();
        params.core.reduce = reduce;
        params.num_threads = num_threads;

        let codec = grk_decompress_init(&mut stream_params, &mut params);
        if codec.is_null() {
            return Err("grok could not open the codestream".to_string());
        }
        let codec = Codec(codec);

        let mut header: grk_header_info = std::mem::zeroed();
        if !grk_decompress_read_header(codec.0, &mut header) {
            return Err("grok could not read the codestream header".to_string());
        }
        if !grk_decompress(codec.0, std::ptr::null_mut()) {
            return Err("grok could not decode the codestream".to_string());
        }

        let image: *mut grk_image = grokj2k_sys::grk_decompress_get_image(codec.0);
        if image.is_null() {
            return Err("grok decoded the codestream but produced no image".to_string());
        }
        read_image(&*image)
    }
}

/// Copy grok's components out into owned buffers, dropping the row padding its
/// stride may carry.
#[cfg(feature = "grok-ffi")]
unsafe fn read_image(image: &grokj2k_sys::grk_image) -> Result<DecodedFrame, String> {
    let count = image.numcomps as usize;
    if count < DCI_COMPONENT_COUNT {
        return Err(format!(
            "a picture frame has {DCI_COMPONENT_COUNT} components, this codestream has {count}"
        ));
    }
    if image.comps.is_null() {
        return Err("grok reported components but the array is null".to_string());
    }

    let picture = unsafe {
        std::slice::from_raw_parts(image.comps as *const grokj2k_sys::grk_image_comp, count)
    };
    let picture = &picture[..DCI_COMPONENT_COUNT];
    for (index, comp) in picture.iter().enumerate() {
        if !matches!(comp.dx, 1 | SUBSAMPLING_FACTOR) || !matches!(comp.dy, 1 | SUBSAMPLING_FACTOR)
        {
            return Err(format!(
                "component {index} is subsampled {}x{}, and a picture is 4:4:4, 4:2:2 or 4:2:0",
                comp.dx, comp.dy
            ));
        }
        if comp.sgnd {
            return Err(format!(
                "component {index} is signed, which a picture is not"
            ));
        }
        if comp.data.is_null() {
            return Err(format!("component {index} carries no samples"));
        }
        if comp.prec != picture[0].prec {
            return Err(
                "the components disagree on size or precision, so they cannot be one picture"
                    .to_string(),
            );
        }
    }

    let width = picture.iter().map(|comp| comp.w).max().unwrap_or(0);
    let height = picture.iter().map(|comp| comp.h).max().unwrap_or(0);
    let mut components = Vec::with_capacity(DCI_COMPONENT_COUNT);
    let mut chroma_subsampled = false;
    for (index, comp) in picture.iter().enumerate() {
        let covers_frame = comp.w * u32::from(comp.dx) >= width
            && comp.h * u32::from(comp.dy) >= height
            && comp.w <= width
            && comp.h <= height;
        if !covers_frame {
            return Err(
                "the components disagree on size or precision, so they cannot be one picture"
                    .to_string(),
            );
        }

        let (plane_width, plane_height) = (comp.w as usize, comp.h as usize);
        let stride = comp.stride as usize;
        let mut plane = Vec::with_capacity(plane_width * plane_height);
        for row in 0..plane_height {
            unsafe { extend_row(&mut plane, comp, row * stride, plane_width) }
                .map_err(|e| format!("component {index}: {e}"))?;
        }

        if plane_width < width as usize {
            plane = upsample_columns(&plane, plane_width, plane_height, width as usize);
            chroma_subsampled = true;
        }
        if plane_height < height as usize {
            plane = upsample_rows(&plane, width as usize, plane_height, height as usize);
            chroma_subsampled = true;
        }
        components.push(plane);
    }

    Ok(DecodedFrame {
        width,
        height,
        precision: picture[0].prec,
        components,
        chroma_subsampled,
    })
}

/// The only subsampling factor read here, horizontally or vertically.
#[cfg(any(feature = "grok-ffi", test))]
const SUBSAMPLING_FACTOR: u8 = 2;

/// The mean of two samples, rounded up at the half.
#[cfg(any(feature = "grok-ffi", test))]
fn rounded_mean(left: i32, right: i32) -> i32 {
    (left + right + 1) / 2
}

/// A plane at double width: output sample 2i is input i, 2i+1 the mean of i and
/// i+1, and the last one repeats its own sample.
#[cfg(any(feature = "grok-ffi", test))]
fn upsample_columns(plane: &[i32], width: usize, height: usize, out_width: usize) -> Vec<i32> {
    let mut out = vec![0i32; out_width * height];
    let step = usize::from(SUBSAMPLING_FACTOR);
    for row in 0..height {
        let source = &plane[row * width..(row + 1) * width];
        let target = &mut out[row * out_width..(row + 1) * out_width];
        for (index, &sample) in source.iter().enumerate() {
            let left = index * step;
            if left >= out_width {
                break;
            }
            target[left] = sample;
            if left + 1 >= out_width {
                break;
            }
            let next = source.get(index + 1).copied().unwrap_or(sample);
            target[left + 1] = rounded_mean(sample, next);
        }
    }
    out
}

/// [`upsample_columns`] down the rows instead.
#[cfg(any(feature = "grok-ffi", test))]
fn upsample_rows(plane: &[i32], width: usize, height: usize, out_height: usize) -> Vec<i32> {
    let mut out = vec![0i32; width * out_height];
    let step = usize::from(SUBSAMPLING_FACTOR);
    for index in 0..height {
        let source = &plane[index * width..(index + 1) * width];
        let top = index * step;
        if top >= out_height {
            break;
        }
        out[top * width..(top + 1) * width].copy_from_slice(source);
        if top + 1 >= out_height {
            break;
        }
        let next = if index + 1 < height { index + 1 } else { index };
        for column in 0..width {
            out[(top + 1) * width + column] =
                rounded_mean(source[column], plane[next * width + column]);
        }
    }
    out
}

/// Copy `width` samples starting `offset` samples into `comp.data` onto `plane`.
///
/// grok sizes its samples to the codestream: a 12-bit picture comes back as
/// 16-bit, not the 32-bit the struct's `int32` name suggests, so the width has
/// to be read from `data_type` rather than assumed.
#[cfg(feature = "grok-ffi")]
unsafe fn extend_row(
    plane: &mut Vec<i32>,
    comp: &grokj2k_sys::grk_image_comp,
    offset: usize,
    width: usize,
) -> Result<(), String> {
    use grokj2k_sys::{
        _grk_data_type_GRK_INT_8, _grk_data_type_GRK_INT_16, _grk_data_type_GRK_INT_32,
    };
    match comp.data_type {
        t if t == _grk_data_type_GRK_INT_32 => {
            let row = unsafe { (comp.data as *const i32).add(offset) };
            plane.extend_from_slice(unsafe { std::slice::from_raw_parts(row, width) });
        }
        t if t == _grk_data_type_GRK_INT_16 => {
            let row = unsafe { (comp.data as *const i16).add(offset) };
            let row = unsafe { std::slice::from_raw_parts(row, width) };
            plane.extend(row.iter().map(|&sample| i32::from(sample)));
        }
        t if t == _grk_data_type_GRK_INT_8 => {
            let row = unsafe { (comp.data as *const i8).add(offset) };
            let row = unsafe { std::slice::from_raw_parts(row, width) };
            plane.extend(row.iter().map(|&sample| i32::from(sample)));
        }
        other => {
            return Err(format!(
                "samples are data type {other}, and only the integer types are read here"
            ));
        }
    }
    Ok(())
}

/// Refusal when postkit was built without the `grok-ffi` feature.
#[cfg(not(feature = "grok-ffi"))]
pub fn decode(_codestream: Vec<u8>, _reduce: u8) -> Result<DecodedFrame, String> {
    Err("postkit was built without the `grok-ffi` feature, so it cannot decode JPEG 2000".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "grok-ffi")]
    const CINEMA_4K_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cinema4k_grey_4096x2160.j2c"
    ));

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_4k_fixture_decodes_on_the_shared_pool() {
        crate::grok_encoder::initialize(0);
        let frame = decode(CINEMA_4K_FIXTURE.to_vec(), 0).expect("4K fixture decodes");
        assert_eq!((frame.width, frame.height), (4096, 2160));
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_4k_fixture_decodes_on_one_thread() {
        crate::grok_encoder::initialize(0);
        let frame =
            decode_with_threads(CINEMA_4K_FIXTURE.to_vec(), 0, 1).expect("4K fixture decodes");
        assert_eq!((frame.width, frame.height), (4096, 2160));
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_4k_fixture_decodes_reduced() {
        crate::grok_encoder::initialize(0);
        let frame = decode(CINEMA_4K_FIXTURE.to_vec(), 1).expect("4K fixture decodes at half size");
        assert_eq!((frame.width, frame.height), (2048, 1080));
    }

    #[test]
    fn twelve_bit_samples_pass_through_and_others_are_normalised() {
        assert_eq!(to_twelve_bits(4095, 12), 4095);
        assert_eq!(to_twelve_bits(0, 12), 0);
        assert_eq!(to_twelve_bits(255, 8), 4080, "8-bit scales up by 16");
        assert_eq!(to_twelve_bits(65535, 16), 4095, "16-bit scales down by 16");
        assert_eq!(
            to_twelve_bits(-3, 12),
            0,
            "a negative sample clamps to black"
        );
        assert_eq!(
            to_twelve_bits(9000, 12),
            4095,
            "an over-range sample clamps"
        );
    }

    #[test]
    fn packing_puts_the_sample_in_the_high_bits_of_each_word() {
        let frame = DecodedFrame {
            width: 2,
            height: 1,
            precision: 12,
            components: vec![vec![4095, 0], vec![1, 2], vec![16, 32]],
            chroma_subsampled: false,
        };
        let packed = frame.to_xyz12le().unwrap();
        assert_eq!(packed.len(), 2 * 3 * 2, "two pixels of three 16-bit words");
        // 4095 << 4 is 0xFFF0, little-endian
        assert_eq!(&packed[0..2], &[0xF0, 0xFF]);
        assert_eq!(&packed[2..4], &(1u16 << 4).to_le_bytes());
        assert_eq!(&packed[4..6], &(16u16 << 4).to_le_bytes());
        // second pixel
        assert_eq!(&packed[6..8], &[0x00, 0x00]);
        assert_eq!(&packed[8..10], &(2u16 << 4).to_le_bytes());
    }

    #[test]
    fn a_frame_that_is_not_three_components_is_refused() {
        let frame = DecodedFrame {
            width: 1,
            height: 1,
            precision: 12,
            components: vec![vec![0]],
            chroma_subsampled: false,
        };
        let error = frame.to_xyz12le().unwrap_err();
        assert!(error.contains("3 components"), "{error}");
    }

    #[test]
    fn a_component_short_of_samples_is_refused() {
        let frame = DecodedFrame {
            width: 4,
            height: 4,
            precision: 12,
            components: vec![vec![0; 16], vec![0; 16], vec![0; 3]],
            chroma_subsampled: false,
        };
        let error = frame.to_xyz12le().unwrap_err();
        assert!(error.contains("component 2"), "{error}");
    }

    #[test]
    fn an_empty_codestream_is_refused() {
        let error = decode(Vec::new(), 0).unwrap_err();
        assert!(!error.is_empty());
    }

    #[test]
    fn doubling_a_row_lands_the_new_sample_between_its_neighbours() {
        let doubled = upsample_columns(&[100, 200, 300], 3, 1, 6);
        assert_eq!(doubled, vec![100, 150, 200, 250, 300, 300]);
    }

    #[test]
    fn doubling_rows_lands_the_new_row_between_its_neighbours() {
        let doubled = upsample_rows(&[10, 20, 50, 60], 2, 2, 4);
        assert_eq!(doubled, vec![10, 20, 30, 40, 50, 60, 50, 60]);
    }

    /// Where the chroma planes step from one flat side to the other.
    #[cfg(feature = "grok-ffi")]
    const CHROMA_EDGE_COLUMN: usize = 32;
    #[cfg(feature = "grok-ffi")]
    const YCBCR_FRAME: (u32, u32) = (128, 72);
    #[cfg(feature = "grok-ffi")]
    const MID_GREY_12BIT: i32 = 2048;
    #[cfg(feature = "grok-ffi")]
    const CHROMA_LOW_12BIT: i32 = 1024;
    #[cfg(feature = "grok-ffi")]
    const CHROMA_HIGH_12BIT: i32 = 3072;

    /// A losslessly encoded 4:2:2 codestream: flat mid-grey luma, and chroma
    /// planes that step from low to high halfway across.
    #[cfg(feature = "grok-ffi")]
    fn encode_ycbcr422_frame() -> Vec<u8> {
        let (width, height) = YCBCR_FRAME;
        let chroma_width = width as usize / 2;
        let luma = vec![MID_GREY_12BIT; (width * height) as usize];
        let mut chroma = Vec::with_capacity(chroma_width * height as usize);
        for _ in 0..height {
            for column in 0..chroma_width {
                chroma.push(if column < CHROMA_EDGE_COLUMN {
                    CHROMA_LOW_12BIT
                } else {
                    CHROMA_HIGH_12BIT
                });
            }
        }
        let profile = crate::j2k::ImfProfile::for_raster(width, height).unwrap();
        let levels = crate::j2k::imf_levels(width, height, 24.0, 200_000_000).unwrap();
        let params = crate::grok_encoder::CompressParams {
            irreversible: false,
            compression_ratio: 1.0,
            mct: false,
            apply_xyz_transform: false,
            profile: crate::j2k::imf_rsiz(profile, levels),
            num_resolutions: 3,
            ..Default::default()
        };
        crate::grok_encoder::initialize(0);
        crate::grok_encoder::compress_yuv422_frame(
            [&luma, &chroma, &chroma],
            width,
            height,
            12,
            &params,
        )
        .expect("a 4:2:2 frame compresses")
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn a_422_codestream_decodes_with_its_chroma_upsampled() {
        let (width, height) = YCBCR_FRAME;
        let frame = decode(encode_ycbcr422_frame(), 0).expect("4:2:2 fixture decodes");
        assert_eq!((frame.width, frame.height), (width, height));
        assert!(
            frame.chroma_subsampled,
            "a 4:2:2 codestream has to read back as subsampled, or the preview would take its \
             YCbCr samples for RGB"
        );
        let samples = (width * height) as usize;
        for (index, component) in frame.components.iter().enumerate() {
            assert_eq!(
                component.len(),
                samples,
                "component {index} came back at the wrong size"
            );
        }

        assert!(
            frame.components[0].iter().all(|&y| y == MID_GREY_12BIT),
            "the flat luma plane did not survive a lossless round trip"
        );

        let edge = CHROMA_EDGE_COLUMN * 2;
        for plane in &frame.components[1..] {
            assert_eq!(
                plane[0], CHROMA_LOW_12BIT,
                "the left side of the chroma edge"
            );
            assert_eq!(
                plane[edge - 2],
                CHROMA_LOW_12BIT,
                "the last chroma sample before the edge"
            );
            assert_eq!(
                plane[edge - 1],
                (CHROMA_LOW_12BIT + CHROMA_HIGH_12BIT) / 2,
                "the interpolated sample has to land between the two sides"
            );
            assert_eq!(plane[edge], CHROMA_HIGH_12BIT, "the right side of the edge");
            assert_eq!(
                plane[width as usize - 1],
                CHROMA_HIGH_12BIT,
                "the last column repeats its own chroma sample"
            );
        }
    }
}
