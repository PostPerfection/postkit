use super::bit_reader::BitReader;
use super::{FrameRate, IabError, SampleRate};

// ST 2098-2 Table B.1, the odd taps only, the even ones are zero
const INTERPOLATION_ODD_TAPS: [i64; 16] = [
    -138, 305, -618, 1128, -1952, 3377, -6450, 20688, 20688, -6450, 3377, -1952, 1128, -618, 305,
    -138,
];

const PREDICTOR_BUFFER_MASK: usize = 63;

struct PredictorRegion {
    sub_block_count: usize,
    direct_form_coefficients: Vec<i32>,
}

// ST 2098-2 Table 30
fn sub_block_layout(reader: &BitReader, frame_rate: FrameRate) -> Result<(usize, usize), IabError> {
    Ok(match frame_rate {
        FrameRate::Fps24 => (10, 200),
        FrameRate::Fps25 => (10, 192),
        FrameRate::Fps30 => (8, 200),
        FrameRate::Fps48 => (5, 200),
        FrameRate::Fps50 => (5, 192),
        FrameRate::Fps60 => (4, 200),
        FrameRate::Fps96 => (5, 100),
        FrameRate::Fps100 => (4, 120),
        FrameRate::Fps120 => (4, 100),
        FrameRate::Fps24000Over1001 => {
            return Err(reader.invalid(
                "FrameRate",
                9,
                "AudioDataDLC is not carried at a non-integer frame rate",
            ));
        }
    })
}

pub(super) fn decode(
    coded: &[u8],
    sample_rate: SampleRate,
    frame_rate: FrameRate,
) -> Result<Vec<i32>, IabError> {
    let mut reader = BitReader::new(coded, "AudioDataDLC");
    let (sub_block_count, sub_block_size_48k) = sub_block_layout(&reader, frame_rate)?;
    let dlc_sample_rate = match reader.read(2, "DLCSampleRate")? {
        0 => SampleRate::Hz48000,
        1 => SampleRate::Hz96000,
        code => {
            return Err(reader.invalid("DLCSampleRate", code.into(), "codes 2 and 3 are reserved"));
        }
    };
    if dlc_sample_rate == SampleRate::Hz48000 && sample_rate == SampleRate::Hz96000 {
        return Err(reader.invalid(
            "DLCSampleRate",
            u64::from(dlc_sample_rate.hertz()),
            "a 48 kHz AudioDataDLC has no 96 kHz layer to decode",
        ));
    }
    let shift_bits = reader.read(5, "ShiftBits")?;

    let regions_48k = read_predictor_regions(&mut reader, sub_block_count, "NumPredRegions48")?;
    let residual_48k = read_residuals(&mut reader, sub_block_count, sub_block_size_48k)?;
    let pcm_48k = apply_predictor(&regions_48k, residual_48k, sub_block_size_48k);
    // ST 2098-2 B.5 lets a 48 kHz decoder skip the 96 kHz extension layer
    if sample_rate == SampleRate::Hz48000 {
        if dlc_sample_rate == SampleRate::Hz48000 {
            reader.expect_end("DLCSize")?;
        }
        return Ok(pcm_48k
            .into_iter()
            .map(|sample| sample << shift_bits)
            .collect());
    }

    let sub_block_size_96k = sub_block_size_48k * 2;
    let regions_96k = read_predictor_regions(&mut reader, sub_block_count, "NumPredRegions96")?;
    let residual_96k = read_residuals(&mut reader, sub_block_count, sub_block_size_96k)?;
    let extension = apply_predictor(&regions_96k, residual_96k, sub_block_size_96k);
    reader.expect_end("DLCSize")?;
    Ok(upsample(&pcm_48k)
        .into_iter()
        .zip(extension)
        .map(|(base, extension)| base.wrapping_add(extension) << shift_bits)
        .collect())
}

fn read_predictor_regions(
    reader: &mut BitReader,
    sub_block_count: usize,
    region_count_field: &'static str,
) -> Result<Vec<PredictorRegion>, IabError> {
    let region_count = reader.read(2, region_count_field)?;
    let mut regions = Vec::new();
    for _ in 0..region_count {
        let region_length = reader.read(4, "RegionLength")? as usize;
        let order = reader.read(5, "Order")? as usize;
        let mut lattice_coefficients = Vec::with_capacity(order);
        for _ in 0..order {
            lattice_coefficients.push(reader.read(10, "KCoeff")? as i32);
        }
        regions.push(PredictorRegion {
            sub_block_count: region_length,
            direct_form_coefficients: direct_form_coefficients(&lattice_coefficients),
        });
    }
    let covered_sub_blocks: usize = regions.iter().map(|region| region.sub_block_count).sum();
    if region_count > 0 && covered_sub_blocks != sub_block_count {
        return Err(reader.invalid(
            "RegionLength",
            covered_sub_blocks as u64,
            "the region lengths add up to NumDLCSubBlocks",
        ));
    }
    Ok(regions)
}

// ST 2098-2 B.7, the result holds coefficients 1 to order at indices 0 to order - 1
fn direct_form_coefficients(lattice_coefficients: &[i32]) -> Vec<i32> {
    let order = lattice_coefficients.len();
    let mut coefficients = vec![0i32; order + 1];
    coefficients[0] = 1 << 20;
    let mut updated = vec![0i32; order + 1];
    for j in 1..=order {
        let reflection = (lattice_coefficients[j - 1] - 512) << 11;
        coefficients[j] = 0;
        for k in 1..=j {
            let product = i64::from(reflection) * i64::from(coefficients[j - k]);
            updated[k] = coefficients[k].wrapping_add((product >> 20) as i32);
        }
        coefficients[1..=j].copy_from_slice(&updated[1..=j]);
    }
    coefficients.split_off(1)
}

fn read_residuals(
    reader: &mut BitReader,
    sub_block_count: usize,
    sub_block_size: usize,
) -> Result<Vec<i32>, IabError> {
    let mut residuals = Vec::with_capacity(sub_block_count * sub_block_size);
    for _ in 0..sub_block_count {
        let rice_coded = reader.read_flag("CodeType")?;
        if rice_coded {
            let remainder_bits = reader.read(5, "RiceRemBits")?;
            for _ in 0..sub_block_size {
                let mut quotient: u64 = 0;
                while reader.read_flag("UnaryBit")? {
                    quotient += 1;
                }
                let remainder = if remainder_bits == 0 {
                    0
                } else {
                    reader.read(remainder_bits, "Residual")?
                };
                let magnitude = (quotient << remainder_bits) + u64::from(remainder);
                residuals.push(read_signed(reader, magnitude)?);
            }
        } else {
            let magnitude_bits = reader.read(5, "BitDepth")?;
            for _ in 0..sub_block_size {
                let magnitude = if magnitude_bits == 0 {
                    0
                } else {
                    reader.read(magnitude_bits, "Residual")?
                };
                residuals.push(read_signed(reader, magnitude.into())?);
            }
        }
    }
    Ok(residuals)
}

fn read_signed(reader: &mut BitReader, magnitude: u64) -> Result<i32, IabError> {
    if magnitude == 0 {
        return Ok(0);
    }
    let magnitude = i32::try_from(magnitude)
        .map_err(|_| reader.invalid("Residual", magnitude, "a residual fits 32 bits"))?;
    Ok(if reader.read_flag("Sign")? {
        -magnitude
    } else {
        magnitude
    })
}

// ST 2098-2 B.8
fn apply_predictor(
    regions: &[PredictorRegion],
    residuals: Vec<i32>,
    sub_block_size: usize,
) -> Vec<i32> {
    if regions.is_empty() {
        return residuals;
    }
    let mut history = [0i32; PREDICTOR_BUFFER_MASK + 1];
    let mut newest = 0usize;
    let mut output = Vec::with_capacity(residuals.len());
    let mut residual_samples = residuals.into_iter();
    for region in regions {
        for residual in residual_samples
            .by_ref()
            .take(region.sub_block_count * sub_block_size)
        {
            let mut accumulator: i64 = 0;
            let mut tap = newest;
            for coefficient in &region.direct_form_coefficients {
                accumulator = accumulator
                    .wrapping_sub(i64::from(history[tap]).wrapping_mul(i64::from(*coefficient)));
                tap = tap.wrapping_sub(1) & PREDICTOR_BUFFER_MASK;
            }
            let sample = residual.wrapping_add((accumulator >> 20) as i32);
            newest = (newest + 1) & PREDICTOR_BUFFER_MASK;
            history[newest] = sample;
            output.push(sample);
        }
    }
    output
}

// ST 2098-2 B.9
fn upsample(base: &[i32]) -> Vec<i32> {
    let mut history = [0i32; PREDICTOR_BUFFER_MASK + 1];
    let mut newest = 0usize;
    let mut output = Vec::with_capacity(base.len() * 2);
    for sample in base {
        history[newest] = *sample;
        output.push(history[newest.wrapping_sub(8) & PREDICTOR_BUFFER_MASK]);
        let mut accumulator: i64 = 0;
        let mut tap = newest;
        for coefficient in INTERPOLATION_ODD_TAPS {
            accumulator += i64::from(history[tap]) * coefficient;
            tap = tap.wrapping_sub(1) & PREDICTOR_BUFFER_MASK;
        }
        output.push((accumulator >> 15) as i32);
        newest = (newest + 1) & PREDICTOR_BUFFER_MASK;
    }
    output
}
