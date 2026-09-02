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

    let mut components = Vec::with_capacity(DCI_COMPONENT_COUNT);
    let mut size = None;
    for index in 0..DCI_COMPONENT_COUNT {
        let comp = unsafe { &*image.comps.add(index) };
        if comp.dx != 1 || comp.dy != 1 {
            return Err(format!(
                "component {index} is subsampled {}x{}, and a DCP picture is 4:4:4",
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
        match size {
            None => size = Some((comp.w, comp.h, comp.prec)),
            Some(first) if first != (comp.w, comp.h, comp.prec) => {
                return Err(
                    "the components disagree on size or precision, so they cannot be one picture"
                        .to_string(),
                );
            }
            Some(_) => {}
        }

        let (width, height, stride) = (comp.w as usize, comp.h as usize, comp.stride as usize);
        let mut plane = Vec::with_capacity(width * height);
        for row in 0..height {
            unsafe { extend_row(&mut plane, comp, row * stride, width) }
                .map_err(|e| format!("component {index}: {e}"))?;
        }
        components.push(plane);
    }

    let (width, height, precision) = size.expect("three components were read");
    Ok(DecodedFrame {
        width,
        height,
        precision,
        components,
    })
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
        };
        let error = frame.to_xyz12le().unwrap_err();
        assert!(error.contains("component 2"), "{error}");
    }

    #[test]
    fn an_empty_codestream_is_refused() {
        let error = decode(Vec::new(), 0).unwrap_err();
        assert!(!error.is_empty());
    }
}
