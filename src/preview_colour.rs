//! The IMF App 2E display transform: what the essence descriptor's
//! ColorPrimaries and TransferCharacteristic say the samples are, turned into
//! 8-bit Rec.709 for a monitor.
//!
//! An HDR frame is tone mapped with the ITU-R BT.2390 EETF from the mastering
//! display's peak down to a 100 cd/m² SDR peak, a wide gamut frame is matrixed
//! into Rec.709 through XYZ and clipped, and a 4:2:2 frame's YCbCr samples are
//! converted to RGB first. A Rec.709 SDR frame comes out of the same path
//! unchanged, code for code, as the pass-through that used to be here.

use crate::colour::{ColourSpace, REC709_GAMMA, linear_rgb_to_rec709, mat_vec, source_space};
use crate::grok_decoder::DecodedFrame;
use crate::preview::{PreviewError, ResolvedPicture, Rgb8Frame};
use asdcplib::jp2k::{
    COLOR_PRIMARIES_BT709, COLOR_PRIMARIES_BT2020, COLOR_PRIMARIES_P3D65,
    TRANSFER_CHARACTERISTIC_BT709, TRANSFER_CHARACTERISTIC_BT2020, TRANSFER_CHARACTERISTIC_ST2084,
};
use std::path::Path;

/// What an App 2E picture carries, and the only depth the code values below are
/// right for.
const IMF_PRECISION_BITS: u8 = 12;
const IMF_COMPONENT_COUNT: usize = 3;
const TWELVE_BIT_LEVELS: usize = 4096;
const TWELVE_BIT_MAX: f32 = 4095.0;
const EIGHT_BIT_LEVELS: usize = 256;
/// How far a 12-bit code is shifted down to reach 8 bits.
const TWELVE_TO_EIGHT_BIT_SHIFT: usize = 16;

/// SMPTE ST 2084 EOTF constants, and the peak the encoding normalises against.
const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;
const PQ_PEAK_NITS: f64 = 10000.0;

/// ITU-R BT.2100 HLG OETF constants, the signal it changes shape at, the system
/// gamma of the OOTF and the display peak that OOTF is written for.
const HLG_A: f64 = 0.178_832_77;
const HLG_B: f64 = 0.284_668_92;
const HLG_C: f64 = 0.559_910_73;
const HLG_OETF_SPLIT_SIGNAL: f64 = 0.5;
const HLG_SYSTEM_GAMMA: f64 = 1.2;
const HLG_NOMINAL_PEAK_NITS: f64 = 1000.0;

/// The SDR peak the tone map lands on, and display linear 1.0 after it.
const SDR_PEAK_NITS: f64 = 100.0;
/// The source peak assumed when the descriptor carries no mastering display.
const DEFAULT_SOURCE_PEAK_NITS: f64 = 1000.0;
/// ST 2086 counts MasteringDisplayMaximumLuminance in 0.0001 cd/m² steps.
const MASTERING_LUMINANCE_STEPS_PER_NIT: f64 = 10_000.0;
/// Where the BT.2390 EETF's hermite spline takes over from the straight line.
const EETF_KNEE_SLOPE: f64 = 1.5;
const EETF_KNEE_OFFSET: f64 = 0.5;

/// ITU-R BT.709 luma coefficients for red and blue.
const BT709_LUMA_RED_BLUE: [f32; 2] = [0.2126, 0.0722];
/// ITU-R BT.2020 non-constant-luminance luma coefficients for red and blue.
const BT2020_LUMA_RED_BLUE: [f32; 2] = [0.2627, 0.0593];

/// Narrow range at 12 bits: luma black and white, and the chroma midpoint with
/// the code range either side of it.
const NARROW_LUMA_BLACK: f32 = 256.0;
const NARROW_LUMA_WHITE: f32 = 3760.0;
const NARROW_CHROMA_MIDPOINT: f32 = 2048.0;
const NARROW_CHROMA_HALF_RANGE: f32 = 1792.0;

/// The colour primaries an App 2E essence descriptor signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPrimaries {
    Bt709,
    P3D65,
    Bt2020,
}

/// The transfer characteristic an App 2E essence descriptor signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayTransfer {
    Bt709,
    Pq,
    Hlg,
}

/// What the picture's samples are, resolved from the descriptor's ULs.
#[derive(Debug, Clone, Copy)]
pub struct PictureColour {
    pub primaries: DisplayPrimaries,
    pub transfer: DisplayTransfer,
    /// The ST 2086 mastering display peak in the descriptor's own 0.0001 cd/m²
    /// steps, `None` when it signals none.
    pub mastering_display_max_luminance: Option<u32>,
}

/// Resolve the descriptor's colour ULs, refusing only one this module has no
/// reading for. Unsignalled colour reads as Rec.709: packages exist that signal
/// nothing.
pub fn resolve_picture_colour(resolved: &ResolvedPicture) -> Result<PictureColour, PreviewError> {
    const UNRECOGNISED: &str = "and the preview has no display transform for it";
    let file = resolved.mxf.display();

    let transfer = match resolved.transfer_characteristic {
        None => {
            tracing::warn!(
                "{file} signals no transfer characteristic, so the preview assumes Rec.709"
            );
            DisplayTransfer::Bt709
        }
        Some(ul) if ul == TRANSFER_CHARACTERISTIC_BT709 => DisplayTransfer::Bt709,
        Some(ul) if ul == TRANSFER_CHARACTERISTIC_ST2084 => DisplayTransfer::Pq,
        // the BT.2020 transfer UL is what an App 2E HLG master signals
        Some(ul) if ul == TRANSFER_CHARACTERISTIC_BT2020 => DisplayTransfer::Hlg,
        Some(ul) => {
            return Err(PreviewError::Display(format!(
                "{file} signals the unrecognised transfer characteristic {ul:02x?}, {UNRECOGNISED}"
            )));
        }
    };

    let primaries = match resolved.color_primaries {
        None => {
            tracing::warn!("{file} signals no colour primaries, so the preview assumes Rec.709");
            DisplayPrimaries::Bt709
        }
        Some(ul) if ul == COLOR_PRIMARIES_BT709 => DisplayPrimaries::Bt709,
        Some(ul) if ul == COLOR_PRIMARIES_P3D65 => DisplayPrimaries::P3D65,
        Some(ul) if ul == COLOR_PRIMARIES_BT2020 => DisplayPrimaries::Bt2020,
        Some(ul) => {
            return Err(PreviewError::Display(format!(
                "{file} signals the unrecognised colour primaries {ul:02x?}, {UNRECOGNISED}"
            )));
        }
    };

    Ok(PictureColour {
        primaries,
        transfer,
        mastering_display_max_luminance: resolved.mastering_display_max_luminance,
    })
}

/// A decoded App 2E frame as packed 8-bit Rec.709 RGB.
pub fn render_display_rgb8(
    decoded: &DecodedFrame,
    colour: &PictureColour,
    mxf: &Path,
) -> Result<Rgb8Frame, PreviewError> {
    if decoded.components.len() != IMF_COMPONENT_COUNT {
        return Err(PreviewError::Display(format!(
            "{} decodes to {} components, and the preview shows only 3-component picture",
            mxf.display(),
            decoded.components.len()
        )));
    }
    if decoded.precision != IMF_PRECISION_BITS {
        return Err(PreviewError::Display(format!(
            "{} decodes at {} bits a sample, and the preview shows only {IMF_PRECISION_BITS}-bit \
             IMF picture",
            mxf.display(),
            decoded.precision
        )));
    }
    let samples = decoded.width as usize * decoded.height as usize;
    for (index, component) in decoded.components.iter().enumerate() {
        if component.len() != samples {
            return Err(PreviewError::Display(format!(
                "component {index} of {} holds {} samples, not the {samples} the frame is",
                mxf.display(),
                component.len()
            )));
        }
    }

    let to_display_linear = transfer_lookup(colour);
    let to_rec709 = rec709_matrix(colour.primaries);
    let ycbcr = decoded
        .chroma_subsampled
        .then(|| ycbcr_to_rgb_matrix(colour.primaries));
    let encode = DisplayEncode::new();

    let mut data = Vec::with_capacity(samples * IMF_COMPONENT_COUNT);
    for sample in 0..samples {
        let mut codes = [
            decoded.components[0][sample],
            decoded.components[1][sample],
            decoded.components[2][sample],
        ];
        if let Some(matrix) = &ycbcr {
            codes = ycbcr_codes_to_rgb_codes(matrix, codes);
        }
        let mut rgb =
            codes.map(|code| to_display_linear[(code.clamp(0, TWELVE_BIT_MAX as i32)) as usize]);
        if let Some(matrix) = &to_rec709 {
            rgb = mat_vec(matrix, rgb);
        }
        for channel in rgb {
            data.push(encode.code(channel));
        }
    }

    Ok(Rgb8Frame {
        width: decoded.width,
        height: decoded.height,
        data,
    })
}

/// Every 12-bit code value as display linear light, 1.0 at the 100 cd/m² SDR
/// peak, so no pixel pays for a `powf`.
fn transfer_lookup(colour: &PictureColour) -> Vec<f32> {
    let tone_map = ToneMap::new(source_peak_nits(colour));
    (0..TWELVE_BIT_LEVELS)
        .map(|code| {
            let signal = code as f32 / TWELVE_BIT_MAX;
            match colour.transfer {
                DisplayTransfer::Bt709 => signal.powf(REC709_GAMMA),
                DisplayTransfer::Pq => {
                    (tone_map.nits(pq_eotf_nits(f64::from(signal))) / SDR_PEAK_NITS) as f32
                }
                DisplayTransfer::Hlg => {
                    (tone_map.nits(hlg_nits(f64::from(signal))) / SDR_PEAK_NITS) as f32
                }
            }
        })
        .collect()
}

/// The mastering display's peak in cd/m², or the nominal 1000 when the
/// descriptor signals none.
fn source_peak_nits(colour: &PictureColour) -> f64 {
    match colour.mastering_display_max_luminance {
        Some(steps) if steps > 0 => f64::from(steps) / MASTERING_LUMINANCE_STEPS_PER_NIT,
        _ => DEFAULT_SOURCE_PEAK_NITS,
    }
}

/// SMPTE ST 2084's EOTF: a normalised code value as absolute cd/m².
fn pq_eotf_nits(signal: f64) -> f64 {
    let encoded = signal.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    let numerator = (encoded - PQ_C1).max(0.0);
    let denominator = PQ_C2 - PQ_C3 * encoded;
    (numerator / denominator).powf(1.0 / PQ_M1) * PQ_PEAK_NITS
}

/// ST 2084's EOTF the other way: cd/m² as a normalised code value.
fn pq_inverse_eotf(nits: f64) -> f64 {
    let luminance = (nits / PQ_PEAK_NITS).clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * luminance) / (1.0 + PQ_C3 * luminance)).powf(PQ_M2)
}

/// A normalised HLG code value as absolute cd/m²: the inverse OETF, then the
/// BT.2100 OOTF.
///
/// The OOTF is defined on the pixel's scene luminance, and this applies it to
/// each channel on its own, which is the same thing on a neutral and slightly
/// more saturated off it.
fn hlg_nits(signal: f64) -> f64 {
    let signal = signal.clamp(0.0, 1.0);
    let scene = if signal <= HLG_OETF_SPLIT_SIGNAL {
        signal * signal / 3.0
    } else {
        (((signal - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    };
    HLG_NOMINAL_PEAK_NITS * scene.powf(HLG_SYSTEM_GAMMA)
}

/// The ITU-R BT.2390 EETF from one source peak to the SDR peak, with black at 0
/// so the spline's black lift term drops out.
struct ToneMap {
    source_peak_pq: f64,
    max_luminance: f64,
    knee_start: f64,
}

impl ToneMap {
    fn new(source_peak_nits: f64) -> Self {
        let source_peak_pq = pq_inverse_eotf(source_peak_nits);
        let max_luminance = pq_inverse_eotf(SDR_PEAK_NITS) / source_peak_pq;
        Self {
            source_peak_pq,
            max_luminance,
            knee_start: EETF_KNEE_SLOPE * max_luminance - EETF_KNEE_OFFSET,
        }
    }

    fn nits(&self, nits: f64) -> f64 {
        if nits <= 0.0 {
            return 0.0;
        }
        let signal = (pq_inverse_eotf(nits) / self.source_peak_pq).min(1.0);
        if signal < self.knee_start {
            return pq_eotf_nits(signal * self.source_peak_pq);
        }
        let along_knee = (signal - self.knee_start) / (1.0 - self.knee_start);
        let squared = along_knee * along_knee;
        let cubed = squared * along_knee;
        let mapped = (2.0 * cubed - 3.0 * squared + 1.0) * self.knee_start
            + (cubed - 2.0 * squared + along_knee) * (1.0 - self.knee_start)
            + (-2.0 * cubed + 3.0 * squared) * self.max_luminance;
        pq_eotf_nits(mapped.min(1.0) * self.source_peak_pq)
    }
}

/// The matrix from the source's linear RGB into linear Rec.709, or `None` when
/// the source already is Rec.709 and the identity would only leak rounding.
fn rec709_matrix(primaries: DisplayPrimaries) -> Option<[[f32; 3]; 3]> {
    let space = match primaries {
        DisplayPrimaries::Bt709 => return None,
        DisplayPrimaries::P3D65 => ColourSpace::P3D65,
        DisplayPrimaries::Bt2020 => ColourSpace::Rec2020,
    };
    let source = source_space(space).expect("a display primary set has a matrix");
    let rec709 = source_space(ColourSpace::Rec709).expect("Rec.709 has a matrix");
    Some(linear_rgb_to_rec709(&rec709.to_xyz, &source.to_xyz))
}

/// The non-constant-luminance YCbCr to R'G'B' matrix of the signalled
/// primaries. P3-D65 has no luma coefficients of its own, so it takes BT.709's.
fn ycbcr_to_rgb_matrix(primaries: DisplayPrimaries) -> [[f32; 3]; 3] {
    let [red, blue] = match primaries {
        DisplayPrimaries::Bt2020 => BT2020_LUMA_RED_BLUE,
        DisplayPrimaries::Bt709 | DisplayPrimaries::P3D65 => BT709_LUMA_RED_BLUE,
    };
    let green = 1.0 - red - blue;
    [
        [1.0, 0.0, 2.0 * (1.0 - red)],
        [
            1.0,
            -2.0 * blue * (1.0 - blue) / green,
            -2.0 * red * (1.0 - red) / green,
        ],
        [1.0, 2.0 * (1.0 - blue), 0.0],
    ]
}

/// Narrow-range 12-bit YCbCr codes as full-range 12-bit R'G'B' codes.
fn ycbcr_codes_to_rgb_codes(matrix: &[[f32; 3]; 3], codes: [i32; 3]) -> [i32; 3] {
    let luma = (codes[0] as f32 - NARROW_LUMA_BLACK) / (NARROW_LUMA_WHITE - NARROW_LUMA_BLACK);
    let blue_chroma = (codes[1] as f32 - NARROW_CHROMA_MIDPOINT) / NARROW_CHROMA_HALF_RANGE;
    let red_chroma = (codes[2] as f32 - NARROW_CHROMA_MIDPOINT) / NARROW_CHROMA_HALF_RANGE;
    mat_vec(matrix, [luma, blue_chroma, red_chroma])
        .map(|channel| (channel.clamp(0.0, 1.0) * TWELVE_BIT_MAX).round() as i32)
}

/// Display linear light as the 8-bit code the monitor gets: the inverse of
/// gamma 2.2 at 12 bits with the low four bits dropped, which is what the
/// Rec.709 pass-through this replaced did in one shift.
///
/// The thresholds run the encode backwards instead of calling `powf` per pixel,
/// and land on the same code because both sides evaluate the same expression.
struct DisplayEncode {
    /// the display linear value each 8-bit code starts at
    thresholds: Vec<f32>,
}

impl DisplayEncode {
    fn new() -> Self {
        Self {
            thresholds: (0..EIGHT_BIT_LEVELS)
                .map(|code| {
                    ((code * TWELVE_TO_EIGHT_BIT_SHIFT) as f32 / TWELVE_BIT_MAX).powf(REC709_GAMMA)
                })
                .collect(),
        }
    }

    fn code(&self, linear: f32) -> u8 {
        let above = self.thresholds.partition_point(|start| *start <= linear);
        above.saturating_sub(1) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID_GREY_12BIT: i32 = 2048;

    fn colour(primaries: DisplayPrimaries, transfer: DisplayTransfer) -> PictureColour {
        PictureColour {
            primaries,
            transfer,
            mastering_display_max_luminance: None,
        }
    }

    fn one_pixel(codes: [i32; 3], chroma_subsampled: bool) -> DecodedFrame {
        DecodedFrame {
            width: 1,
            height: 1,
            precision: 12,
            components: codes.iter().map(|&code| vec![code]).collect(),
            chroma_subsampled,
        }
    }

    fn render(codes: [i32; 3], colour: &PictureColour, chroma_subsampled: bool) -> [u8; 3] {
        let frame = one_pixel(codes, chroma_subsampled);
        let out = render_display_rgb8(&frame, colour, Path::new("test.mxf")).unwrap();
        [out.data[0], out.data[1], out.data[2]]
    }

    #[test]
    fn a_rec709_frame_comes_out_as_the_shift_by_four_it_used_to() {
        let rec709 = colour(DisplayPrimaries::Bt709, DisplayTransfer::Bt709);
        for code in 0..4096i32 {
            let rendered = render([code, code, code], &rec709, false);
            let shifted = (code >> 4) as u8;
            assert_eq!(
                rendered, [shifted; 3],
                "12-bit code {code} has to reach the screen as {shifted}"
            );
        }
    }

    #[test]
    fn pq_black_stays_black_and_the_source_peak_reaches_white() {
        let pq = colour(DisplayPrimaries::Bt2020, DisplayTransfer::Pq);
        assert_eq!(render([0, 0, 0], &pq, false), [0, 0, 0]);
        let peak =
            (pq_inverse_eotf(DEFAULT_SOURCE_PEAK_NITS) * f64::from(TWELVE_BIT_MAX)).round() as i32;
        assert_eq!(render([peak; 3], &pq, false), [255, 255, 255]);
    }

    #[test]
    fn the_tone_map_lands_the_source_peak_on_the_sdr_peak() {
        let tone_map = ToneMap::new(DEFAULT_SOURCE_PEAK_NITS);
        assert!(
            (tone_map.nits(DEFAULT_SOURCE_PEAK_NITS) - SDR_PEAK_NITS).abs() < 0.01,
            "{} nits is not the SDR peak",
            tone_map.nits(DEFAULT_SOURCE_PEAK_NITS)
        );
        assert_eq!(tone_map.nits(0.0), 0.0);
        assert!(
            tone_map.nits(10.0) < 10.001,
            "below the knee the tone map has to leave the signal alone"
        );
    }

    #[test]
    fn a_75_percent_hlg_signal_is_the_203_nits_bt2408_calls_reference_white() {
        let nits = hlg_nits(0.75);
        assert!((nits - 203.0).abs() < 1.0, "75% HLG is {nits} nits");
    }

    #[test]
    fn the_pq_curve_round_trips_its_own_inverse() {
        for nits in [0.1, 1.0, 100.0, 203.0, 1000.0, 4000.0, 10000.0] {
            let round_tripped = pq_eotf_nits(pq_inverse_eotf(nits));
            assert!(
                (round_tripped - nits).abs() < nits * 1e-6,
                "{nits} nits came back as {round_tripped}"
            );
        }
    }

    #[test]
    fn a_mastering_display_peak_reads_in_cd_per_square_metre() {
        let mut hdr = colour(DisplayPrimaries::Bt2020, DisplayTransfer::Pq);
        assert_eq!(source_peak_nits(&hdr), DEFAULT_SOURCE_PEAK_NITS);
        hdr.mastering_display_max_luminance = Some(40_000_000);
        assert_eq!(source_peak_nits(&hdr), 4000.0);
    }

    #[test]
    fn a_wide_gamut_red_clips_its_other_two_channels() {
        for primaries in [DisplayPrimaries::Bt2020, DisplayPrimaries::P3D65] {
            let sdr = colour(primaries, DisplayTransfer::Bt709);
            let [red, green, blue] = render([4095, 0, 0], &sdr, false);
            assert_eq!(
                (green, blue),
                (0, 0),
                "{primaries:?} red is outside Rec.709, so green and blue clip"
            );
            assert!(red > 200, "{primaries:?} red rendered at {red}");
        }
    }

    #[test]
    fn wide_gamut_white_stays_neutral() {
        for primaries in [DisplayPrimaries::Bt2020, DisplayPrimaries::P3D65] {
            let sdr = colour(primaries, DisplayTransfer::Bt709);
            assert_eq!(
                render([4095; 3], &sdr, false),
                [255, 255, 255],
                "{primaries:?} shares D65 with Rec.709, so its white is Rec.709 white"
            );
        }
    }

    #[test]
    fn narrow_range_ycbcr_grey_renders_grey() {
        let rec709 = colour(DisplayPrimaries::Bt709, DisplayTransfer::Bt709);
        let grey = render(
            [
                MID_GREY_12BIT,
                NARROW_CHROMA_MIDPOINT as i32,
                NARROW_CHROMA_MIDPOINT as i32,
            ],
            &rec709,
            true,
        );
        assert_eq!(grey[0], grey[1], "a zero chroma pixel has to be neutral");
        assert_eq!(grey[1], grey[2], "a zero chroma pixel has to be neutral");
        // (2048 - 256) / (3760 - 256) is 0.5117 of the range, and the encode is 1:1
        assert!(
            (i32::from(grey[0]) - 130).abs() <= 1,
            "mid-range narrow luma rendered at {}",
            grey[0]
        );
    }

    #[test]
    fn narrow_range_ycbcr_black_and_white_reach_the_ends() {
        let rec709 = colour(DisplayPrimaries::Bt709, DisplayTransfer::Bt709);
        let midpoint = NARROW_CHROMA_MIDPOINT as i32;
        assert_eq!(
            render(
                [NARROW_LUMA_BLACK as i32, midpoint, midpoint],
                &rec709,
                true
            ),
            [0, 0, 0]
        );
        assert_eq!(
            render(
                [NARROW_LUMA_WHITE as i32, midpoint, midpoint],
                &rec709,
                true
            ),
            [255, 255, 255]
        );
    }

    #[test]
    fn a_frame_that_is_not_twelve_bit_is_refused() {
        let mut frame = one_pixel([0, 0, 0], false);
        frame.precision = 10;
        let error = render_display_rgb8(
            &frame,
            &colour(DisplayPrimaries::Bt709, DisplayTransfer::Bt709),
            Path::new("test.mxf"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("12-bit"), "{error}");
    }
}
