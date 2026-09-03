use ebur128::{EbuR128, Mode};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek};
use std::path::Path;

/// EBU R128 loudness measurement result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoudnessResult {
    /// Integrated loudness in LUFS.
    pub integrated_lufs: f64,
    /// Loudness range in LU.
    pub range_lu: f64,
    /// True peak in dBTP.
    pub true_peak_dbtp: f64,
    /// Short-term loudness max in LUFS.
    pub short_term_max_lufs: f64,
    /// Whether measurement succeeded.
    pub success: bool,
    pub error: String,
}

/// Measure audio loudness per EBU R128 from a WAV or a PCM MXF, in one streamed
/// pass.
pub fn measure_loudness(input: &Path) -> LoudnessResult {
    match stream_loudness(input) {
        Ok((true_peak_dbtp, summary)) => LoudnessResult {
            integrated_lufs: summary.integrated_lufs,
            range_lu: summary.range_lu,
            true_peak_dbtp,
            short_term_max_lufs: summary.short_term_max_lufs,
            success: true,
            error: String::new(),
        },
        Err(error) => LoudnessResult {
            success: false,
            error: error.to_string(),
            ..Default::default()
        },
    }
}

pub fn measure_true_peak_dbtp(input: &Path) -> Result<f64, AdjustError> {
    stream_true_peak(&open_pcm(input)?, None)
}

// frames per add_frames call, about a second of audio at 48 kHz
const TRUE_PEAK_BLOCK_FRAMES: usize = 48_000;

// blocks a meter may fall behind by before the reader waits for it
const QUEUED_BLOCKS_PER_METER: usize = 2;

// every integer width is widened to i32 scale before this divide
const FULL_SCALE_I32: f32 = 2_147_483_648.0;

// short term loudness is read at R128's own 100 ms block cadence
const SHORT_TERM_SUB_BLOCK_MILLISECONDS: u32 = 100;

const MILLISECONDS_PER_SECOND: u32 = 1000;

// the 3 s short term window is this many sub blocks
const SHORT_TERM_WINDOW_SUB_BLOCKS: usize = 30;

// BS.1770 loudness of an energy is 10 * log10(energy) minus this
const R128_LOUDNESS_OFFSET_DB: f64 = 0.691;

struct R128Summary {
    integrated_lufs: f64,
    range_lu: f64,
    short_term_max_lufs: f64,
}

// one meter over every channel, reading short term loudness as the audio goes in
struct R128Meter {
    meter: EbuR128,
    sub_block_samples: usize,
    pending: Vec<f32>,
    sub_block_energies: std::collections::VecDeque<f64>,
    short_term_max_lufs: f64,
}

impl R128Meter {
    fn new(channels: u32, sample_rate: u32) -> Result<Self, AdjustError> {
        let meter = EbuR128::new(channels, sample_rate, Mode::I | Mode::LRA)?;
        let sub_block_frames =
            (sample_rate * SHORT_TERM_SUB_BLOCK_MILLISECONDS / MILLISECONDS_PER_SECOND) as usize;
        let sub_block_samples = sub_block_frames * channels as usize;
        Ok(Self {
            meter,
            sub_block_samples,
            pending: Vec::with_capacity(sub_block_samples),
            sub_block_energies: std::collections::VecDeque::with_capacity(
                SHORT_TERM_WINDOW_SUB_BLOCKS,
            ),
            short_term_max_lufs: f64::NEG_INFINITY,
        })
    }

    fn add(&mut self, interleaved: &[f32]) -> Result<(), AdjustError> {
        let mut rest = interleaved;
        if !self.pending.is_empty() {
            let wanted = (self.sub_block_samples - self.pending.len()).min(rest.len());
            let (head, tail) = rest.split_at(wanted);
            self.pending.extend_from_slice(head);
            rest = tail;
            if self.pending.len() < self.sub_block_samples {
                return Ok(());
            }
            let mut full = std::mem::take(&mut self.pending);
            self.add_sub_block(&full)?;
            full.clear();
            self.pending = full;
        }
        let mut sub_blocks = rest.chunks_exact(self.sub_block_samples);
        for sub_block in &mut sub_blocks {
            self.add_sub_block(sub_block)?;
        }
        self.pending.extend_from_slice(sub_blocks.remainder());
        Ok(())
    }

    fn add_sub_block(&mut self, interleaved: &[f32]) -> Result<(), AdjustError> {
        self.meter.add_frames_f32(interleaved)?;
        // loudness_shortterm re-sums the whole 3 s window on every call
        let loudness = self
            .meter
            .loudness_window(SHORT_TERM_SUB_BLOCK_MILLISECONDS)?;
        let energy = if loudness.is_finite() {
            10f64.powf((loudness + R128_LOUDNESS_OFFSET_DB) / 10.0)
        } else {
            0.0
        };
        if self.sub_block_energies.len() == SHORT_TERM_WINDOW_SUB_BLOCKS {
            self.sub_block_energies.pop_front();
        }
        self.sub_block_energies.push_back(energy);
        if self.sub_block_energies.len() < SHORT_TERM_WINDOW_SUB_BLOCKS {
            return Ok(());
        }
        let window_energy =
            self.sub_block_energies.iter().sum::<f64>() / SHORT_TERM_WINDOW_SUB_BLOCKS as f64;
        if window_energy > 0.0 {
            let short_term = 10.0 * window_energy.log10() - R128_LOUDNESS_OFFSET_DB;
            self.short_term_max_lufs = self.short_term_max_lufs.max(short_term);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<R128Summary, AdjustError> {
        // a part sub block gets no short term reading of its own
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.meter.add_frames_f32(&tail)?;
        }
        Ok(R128Summary {
            integrated_lufs: self.meter.loudness_global()?,
            range_lu: self.meter.loudness_range()?,
            short_term_max_lufs: self.short_term_max_lufs,
        })
    }
}

fn stream_loudness(input: &Path) -> Result<(f64, R128Summary), AdjustError> {
    let pcm = open_pcm(input)?;
    let channels = pcm.spec.channels as u32;
    let rate = pcm.spec.sample_rate;
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<f32>>(QUEUED_BLOCKS_PER_METER);
    let meter_thread = std::thread::spawn(move || -> Result<R128Summary, AdjustError> {
        let mut meter = R128Meter::new(channels, rate)?;
        while let Ok(interleaved) = receiver.recv() {
            meter.add(&interleaved)?;
        }
        meter.finish()
    });
    let true_peak = stream_true_peak(&pcm, Some(sender));
    let summary = meter_thread.join().expect("r128 meter thread panicked");
    Ok((true_peak?, summary?))
}

// per channel true peak, handing `also` the same interleaved blocks when it is there
fn stream_true_peak(
    input: &PcmInput,
    also: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
) -> Result<f64, AdjustError> {
    let spec = input.spec;
    let channels = spec.channels as usize;

    let mut senders = Vec::with_capacity(channels);
    let mut receivers = Vec::with_capacity(channels);
    for _ in 0..channels {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<f32>>(QUEUED_BLOCKS_PER_METER);
        senders.push(sender);
        receivers.push(receiver);
    }

    // true peak state is per channel, a mono meter per thread is exact
    std::thread::scope(|scope| {
        let workers: Vec<_> = receivers
            .into_iter()
            .map(|receiver| {
                let rate = spec.sample_rate;
                scope.spawn(move || -> Result<f64, AdjustError> {
                    let mut meter = EbuR128::new(1, rate, Mode::TRUE_PEAK)?;
                    while let Ok(plane) = receiver.recv() {
                        meter.add_frames_f32(&plane)?;
                    }
                    Ok(meter.true_peak(0)?)
                })
            })
            .collect();

        let fed = read_blocks(input, |interleaved| {
            let frames = interleaved.len() / channels;
            let mut planes: Vec<Vec<f32>> =
                (0..channels).map(|_| Vec::with_capacity(frames)).collect();
            for frame in interleaved.chunks_exact(channels) {
                for (plane, &sample) in planes.iter_mut().zip(frame) {
                    plane.push(sample);
                }
            }
            for (sender, plane) in senders.iter().zip(planes) {
                // a worker that stopped early says why through its join
                let _ = sender.send(plane);
            }
            if let Some(sender) = &also {
                let _ = sender.send(interleaved.to_vec());
            }
            Ok(())
        });
        drop(senders);
        drop(also);

        let mut peak_linear = 0.0f64;
        for worker in workers {
            peak_linear = peak_linear.max(worker.join().expect("true peak worker panicked")?);
        }
        fed?;
        Ok(20.0 * peak_linear.log10())
    })
}

// raw data bytes, only when the chunk is exactly the counted samples at their natural width
struct PackedData {
    reader: std::io::BufReader<std::fs::File>,
    byte_count: usize,
}

// hound's only signal that a file carries no riff header at all
const HOUND_NO_RIFF_TAG: &str = "no RIFF tag found";

// where the interleaved samples come from and how many of them there are
enum PcmContainer {
    Wav,
    Mxf {
        edit_units: u32,
        bytes_per_edit_unit: usize,
    },
}

struct PcmInput {
    path: std::path::PathBuf,
    spec: WavSpec,
    container: PcmContainer,
}

fn open_pcm(input: &Path) -> Result<PcmInput, AdjustError> {
    let (spec, frames, container) = match WavReader::open(input) {
        Ok(reader) => (reader.spec(), reader.duration() as u64, PcmContainer::Wav),
        Err(hound::Error::FormatError(HOUND_NO_RIFF_TAG)) => open_pcm_mxf(input)?,
        Err(error) => return Err(error.into()),
    };
    if spec.channels == 0 || frames == 0 {
        return Err(AdjustError::Empty);
    }
    Ok(PcmInput {
        path: input.to_path_buf(),
        spec,
        container,
    })
}

fn open_pcm_mxf(input: &Path) -> Result<(WavSpec, u64, PcmContainer), AdjustError> {
    let mut reader = asdcplib::pcm::MxfReader::new();
    reader.open_read(&input.to_string_lossy())?;
    let descriptor = reader.audio_descriptor()?;
    let sample_rate = descriptor.audio_sampling_rate.numerator.max(0) as u32;
    let spec = WavSpec {
        channels: descriptor.channel_count as u16,
        sample_rate,
        bits_per_sample: descriptor.quantization_bits as u16,
        sample_format: SampleFormat::Int,
    };
    if widen_samples(spec).is_none() {
        return Err(AdjustError::UnsupportedPcmDepth(spec.bits_per_sample));
    }
    let edit_rate = descriptor.edit_rate;
    if edit_rate.numerator <= 0 || descriptor.block_align == 0 {
        return Err(AdjustError::Empty);
    }
    let frames_per_edit_unit = (sample_rate as u64 * edit_rate.denominator.max(1) as u64)
        .div_ceil(edit_rate.numerator as u64);
    let bytes_per_edit_unit = frames_per_edit_unit as usize * descriptor.block_align as usize;
    let edit_units = descriptor.container_duration;
    Ok((
        spec,
        frames_per_edit_unit * edit_units as u64,
        PcmContainer::Mxf {
            edit_units,
            bytes_per_edit_unit,
        },
    ))
}

type WidenSamples = fn(&[u8], &mut Vec<f32>);

fn widen_samples(spec: WavSpec) -> Option<WidenSamples> {
    Some(match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => |bytes: &[u8], out: &mut Vec<f32>| {
            out.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&b| f32::from_le_bytes(b)),
            );
        },
        (SampleFormat::Int, 32) => |bytes: &[u8], out: &mut Vec<f32>| {
            out.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&b| i32::from_le_bytes(b) as f32 / FULL_SCALE_I32),
            );
        },
        (SampleFormat::Int, 24) => |bytes: &[u8], out: &mut Vec<f32>| {
            out.extend(
                bytes
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 / FULL_SCALE_I32),
            );
        },
        (SampleFormat::Int, 16) => |bytes: &[u8], out: &mut Vec<f32>| {
            out.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i32::from_le_bytes([0, 0, b[0], b[1]]) as f32 / FULL_SCALE_I32),
            );
        },
        (SampleFormat::Int, 8) => |bytes: &[u8], out: &mut Vec<f32>| {
            out.extend(
                bytes
                    .iter()
                    .map(|&b| (((b as i32) - 128) << 24) as f32 / FULL_SCALE_I32),
            );
        },
        _ => return None,
    })
}

fn packed_data(input: &Path, spec: WavSpec) -> Result<Option<PackedData>, AdjustError> {
    let parsed = WavReader::open(input)?;
    let sample_count = parsed.len() as usize;
    let mut reader = parsed.into_inner();
    let data_start = reader.stream_position().map_err(hound::Error::from)?;
    let file_length = std::fs::metadata(input).map_err(hound::Error::from)?.len();
    let byte_count = (file_length - data_start) as usize;
    if byte_count != sample_count * (spec.bits_per_sample as usize / 8) {
        return Ok(None);
    }
    Ok(Some(PackedData { reader, byte_count }))
}

fn read_blocks(
    input: &PcmInput,
    on_block: impl FnMut(&[f32]) -> Result<(), AdjustError>,
) -> Result<(), AdjustError> {
    match input.container {
        PcmContainer::Wav => read_wav_blocks(&input.path, input.spec, on_block),
        PcmContainer::Mxf {
            edit_units,
            bytes_per_edit_unit,
        } => read_mxf_blocks(input, edit_units, bytes_per_edit_unit, on_block),
    }
}

// one edit unit at a time out of the mxf, batched up to about a second per block
fn read_mxf_blocks(
    input: &PcmInput,
    edit_units: u32,
    bytes_per_edit_unit: usize,
    mut on_block: impl FnMut(&[f32]) -> Result<(), AdjustError>,
) -> Result<(), AdjustError> {
    let spec = input.spec;
    let widen =
        widen_samples(spec).ok_or(AdjustError::UnsupportedPcmDepth(spec.bits_per_sample))?;
    let block_align = spec.channels as usize * (spec.bits_per_sample as usize / 8);
    let frames_per_edit_unit = (bytes_per_edit_unit / block_align).max(1);
    let edit_units_per_block = (TRUE_PEAK_BLOCK_FRAMES / frames_per_edit_unit).max(1);

    let mut reader = asdcplib::pcm::MxfReader::new();
    reader.open_read(&input.path.to_string_lossy())?;
    let mut essence = vec![0u8; bytes_per_edit_unit];
    let mut samples: Vec<f32> =
        Vec::with_capacity(edit_units_per_block * frames_per_edit_unit * spec.channels as usize);
    for edit_unit in 0..edit_units {
        let read = reader.read_frame(edit_unit, &mut essence, None, None)?;
        let whole_frames = read - read % block_align;
        widen(&essence[..whole_frames], &mut samples);
        if (edit_unit as usize + 1).is_multiple_of(edit_units_per_block) {
            on_block(&samples)?;
            samples.clear();
        }
    }
    if !samples.is_empty() {
        on_block(&samples)?;
    }
    Ok(())
}

// hound's per sample iterator is the fallback for what the byte path will not claim
fn read_wav_blocks(
    input: &Path,
    spec: WavSpec,
    mut on_block: impl FnMut(&[f32]) -> Result<(), AdjustError>,
) -> Result<(), AdjustError> {
    let channels = spec.channels as usize;
    let block_samples = TRUE_PEAK_BLOCK_FRAMES * channels;
    let mut samples: Vec<f32> = Vec::with_capacity(block_samples);

    if let Some(widen) = widen_samples(spec)
        && let Some(mut packed) = packed_data(input, spec)?
    {
        let mut bytes = vec![0u8; block_samples * (spec.bits_per_sample as usize / 8)];
        let mut left = packed.byte_count;
        while left > 0 {
            let wanted = left.min(bytes.len());
            packed
                .reader
                .read_exact(&mut bytes[..wanted])
                .map_err(hound::Error::from)?;
            left -= wanted;
            samples.clear();
            widen(&bytes[..wanted], &mut samples);
            on_block(&samples)?;
        }
        return Ok(());
    }

    let mut reader = WavReader::open(input)?;
    match spec.sample_format {
        SampleFormat::Int => {
            let shift = 32 - spec.bits_per_sample;
            for sample in reader.samples::<i32>() {
                samples.push((sample? << shift) as f32 / FULL_SCALE_I32);
                if samples.len() == block_samples {
                    on_block(&samples)?;
                    samples.clear();
                }
            }
        }
        SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                samples.push(sample?);
                if samples.len() == block_samples {
                    on_block(&samples)?;
                    samples.clear();
                }
            }
        }
    }
    if !samples.is_empty() {
        on_block(&samples)?;
    }
    Ok(())
}

// Leq(m) (ISO 21727) cinema loudness, CCIR 468 weighted.

/// Result of a Leq(m) (ISO 21727) measurement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeqMResult {
    /// Leq(m) in dB, referenced to the SMPTE B-chain calibration.
    pub leq_m_db: f64,
    pub success: bool,
    pub error: String,
}

// fft block length; a power of two keeps rustfft fast. at 48 kHz this is ~0.68 s
// per block, far longer than the m weighting's impulse response, so applying the
// weighting per block (circular convolution) has negligible boundary error.
const LEQ_BLOCK: usize = 32768;

// cinema B-chain calibration as leqm-nrt derives it: 85 dB SPL over 20 uPa is
// 0.355655882 Pa RMS, whose peak 0.502973372 Pa is 88.010299957 dB, and the
// reference tone sits at -20 dBFS, so full scale is 108.010299957 dB.
const LEQ_M_REFERENCE_OFFSET_DB: f64 = 108.010_299_957;

/// The M weighting ISO 21727 tabulates, in Hz and in dB relative to 2 kHz.
const M_WEIGHTING_HZ: [f64; 21] = [
    31.0, 63.0, 100.0, 200.0, 400.0, 800.0, 1000.0, 2000.0, 3150.0, 4000.0, 5000.0, 6300.0, 7100.0,
    8000.0, 9000.0, 10000.0, 12500.0, 14000.0, 16000.0, 20000.0, 31500.0,
];
const M_WEIGHTING_DB: [f64; 21] = [
    -35.5, -29.5, -25.4, -19.4, -13.4, -7.5, -5.6, 0.0, 3.4, 4.9, 6.1, 6.6, 6.4, 5.8, 4.5, 2.5,
    -5.6, -10.9, -17.3, -27.8, -48.3,
];

/// M weighting at `frequency` in dB, the table read straight between its points
/// the way leqm-nrt reads it. The ITU-R 468 rational curve the table samples is
/// convex between them, so it runs 0.2 to 0.5 dB hotter over most of the band
/// and a broadband programme measures that much louder through it. Outside the
/// tabulated band the end values hold.
fn m_weighting_db(frequency: f64) -> f64 {
    let last = M_WEIGHTING_HZ.len() - 1;
    if frequency <= M_WEIGHTING_HZ[0] {
        return M_WEIGHTING_DB[0];
    }
    if frequency >= M_WEIGHTING_HZ[last] {
        return M_WEIGHTING_DB[last];
    }
    let upper = M_WEIGHTING_HZ
        .iter()
        .position(|&hz| hz > frequency)
        .unwrap_or(last);
    let lower = upper - 1;
    let span =
        (frequency - M_WEIGHTING_HZ[lower]) / (M_WEIGHTING_HZ[upper] - M_WEIGHTING_HZ[lower]);
    M_WEIGHTING_DB[lower] + span * (M_WEIGHTING_DB[upper] - M_WEIGHTING_DB[lower])
}

/// Squared M weighting for every fft bin at `sample_rate`, so a block costs one
/// multiply per bin.
fn bin_weights_squared(sample_rate: u32) -> Vec<f64> {
    let n = LEQ_BLOCK;
    (0..n)
        .map(|k| {
            // bin k above Nyquist mirrors the positive frequency n-k
            let bin = if k <= n / 2 { k } else { n - k };
            let frequency = bin as f64 * sample_rate as f64 / n as f64;
            let gain = 10f64.powf(m_weighting_db(frequency) / 20.0);
            gain * gain
        })
        .collect()
}

/// Sum of squares of the M-weighted block (Parseval in the frequency domain),
/// i.e. the weighted energy contributed by `samples`. `samples` is zero-padded
/// to the fft length, so its true length still sets the sample count.
fn weighted_block_energy(
    fft: &dyn rustfft::Fft<f32>,
    samples: &[f32],
    bin_weights_squared: &[f64],
) -> f64 {
    let n = LEQ_BLOCK;
    let mut buf = vec![Complex::<f32>::new(0.0, 0.0); n];
    for (dst, &s) in buf.iter_mut().zip(samples) {
        dst.re = s;
    }
    fft.process(&mut buf);
    let mut energy = 0.0f64;
    for (c, weight) in buf.iter().zip(bin_weights_squared) {
        let mag2 = (c.re as f64) * (c.re as f64) + (c.im as f64) * (c.im as f64);
        energy += mag2 * weight;
    }
    // Parseval: sum_n |x_n|^2 = (1/N) sum_k |X_k|^2
    energy / n as f64
}

/// Per-channel level corrections in dB by DCP channel index, the ones
/// leqm-nrt takes and DCP-o-matic passes it. The surrounds and the Lc/Rc pair
/// are 3 dB down and the non-programme channels are excluded outright.
const LEQ_M_CHANNEL_CORRECTIONS_DB: [f64; 16] = [
    0.0,    // L
    0.0,    // R
    0.0,    // C
    0.0,    // LFE
    -3.0,   // Ls
    -3.0,   // Rs
    -144.0, // HI
    -144.0, // VI
    -3.0,   // Lc
    -3.0,   // Rc
    -3.0,   // BsL
    -3.0,   // BsR
    -144.0, // motion data
    -144.0, // sync signal
    -144.0, // sign language
    -144.0, // unused
];

// a channel index past the table carries no programme audio either
const LEQ_M_UNLISTED_CHANNEL_CORRECTION_DB: f64 = -144.0;

fn channel_energy_scales(channels: usize) -> Vec<f64> {
    (0..channels)
        .map(|channel| {
            let db = LEQ_M_CHANNEL_CORRECTIONS_DB
                .get(channel)
                .copied()
                .unwrap_or(LEQ_M_UNLISTED_CHANNEL_CORRECTION_DB);
            // the correction scales amplitude, so energy moves by twice the dB
            10f64.powf(db / 10.0)
        })
        .collect()
}

// the weighted energy of every whole LEQ_BLOCK of frames fed in so far, plus
// the part block, summed over the corrected channels
struct LeqMEnergy {
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    bin_weights_squared: Vec<f64>,
    channels: usize,
    channel_energy_scales: Vec<f64>,
    plane: Vec<f32>,
    pending: Vec<f32>,
    total_energy: f64,
    total_frames: u64,
}

impl LeqMEnergy {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let block_samples = LEQ_BLOCK * channels.max(1);
        Self {
            fft: FftPlanner::<f32>::new().plan_fft_forward(LEQ_BLOCK),
            bin_weights_squared: bin_weights_squared(sample_rate),
            channels,
            channel_energy_scales: channel_energy_scales(channels),
            plane: Vec::with_capacity(LEQ_BLOCK),
            pending: Vec::with_capacity(block_samples),
            total_energy: 0.0,
            total_frames: 0,
        }
    }

    fn add(&mut self, interleaved: &[f32]) {
        let block_samples = LEQ_BLOCK * self.channels;
        let mut rest = interleaved;
        if !self.pending.is_empty() {
            let wanted = (block_samples - self.pending.len()).min(rest.len());
            let (head, tail) = rest.split_at(wanted);
            self.pending.extend_from_slice(head);
            rest = tail;
            if self.pending.len() < block_samples {
                return;
            }
            let mut full = std::mem::take(&mut self.pending);
            self.add_block(&full);
            full.clear();
            self.pending = full;
        }
        let mut blocks = rest.chunks_exact(block_samples);
        for block in &mut blocks {
            self.add_block(block);
        }
        self.pending.extend_from_slice(blocks.remainder());
    }

    fn add_block(&mut self, interleaved: &[f32]) {
        let frames = interleaved.len() / self.channels;
        for channel in 0..self.channels {
            self.plane.clear();
            self.plane
                .extend(interleaved.iter().skip(channel).step_by(self.channels));
            self.total_energy +=
                weighted_block_energy(self.fft.as_ref(), &self.plane, &self.bin_weights_squared)
                    * self.channel_energy_scales[channel];
        }
        self.total_frames += frames as u64;
    }

    fn finish(mut self) -> f64 {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.add_block(&tail);
        }
        if self.total_frames == 0 {
            return f64::NEG_INFINITY;
        }
        let mean_square = self.total_energy / self.total_frames as f64;
        10.0 * mean_square.log10() + LEQ_M_REFERENCE_OFFSET_DB
    }
}

/// Compute Leq(m) (ISO 21727) in dB from mono PCM samples in full-scale units
/// (-1.0..=1.0). The signal is CCIR 468-weighted and its equivalent continuous
/// level is referenced to the cinema B-chain calibration.
pub fn leq_m_from_samples(samples: &[f32], sample_rate: u32) -> f64 {
    leq_m_from_interleaved(samples, 1, sample_rate)
}

/// Compute Leq(m) (ISO 21727) in dB from interleaved PCM in full-scale units
/// (-1.0..=1.0). Each channel is CCIR 468-weighted on its own, corrected by
/// [`LEQ_M_CHANNEL_CORRECTIONS_DB`] for its DCP channel index, and the channel
/// energies are summed.
pub fn leq_m_from_interleaved(interleaved: &[f32], channels: usize, sample_rate: u32) -> f64 {
    if interleaved.is_empty() || channels == 0 || sample_rate == 0 {
        return f64::NEG_INFINITY;
    }
    let mut energy = LeqMEnergy::new(channels, sample_rate);
    energy.add(interleaved);
    energy.finish()
}

/// Measure Leq(m) (ISO 21727) of a WAV or PCM MXF file, streamed in bounded
/// blocks at the file's own sample rate, summing the corrected channel
/// energies.
pub fn measure_leq_m(audio_file: &Path) -> LeqMResult {
    if !audio_file.exists() {
        return LeqMResult {
            error: "Audio file not found".into(),
            ..Default::default()
        };
    }
    match stream_leq_m(audio_file) {
        Ok(leq_m_db) => LeqMResult {
            leq_m_db,
            success: true,
            error: String::new(),
        },
        Err(error) => LeqMResult {
            error: error.to_string(),
            ..Default::default()
        },
    }
}

fn stream_leq_m(input: &Path) -> Result<f64, AdjustError> {
    let pcm = open_pcm(input)?;
    let channels = pcm.spec.channels as usize;
    let mut energy = LeqMEnergy::new(channels, pcm.spec.sample_rate);
    read_blocks(&pcm, |interleaved| {
        energy.add(interleaved);
        Ok(())
    })?;
    Ok(energy.finish())
}

// Loudness adjustment (dom#1382): pure sample-domain gain to hit a target.

/// Which loudness quantity a gain adjustment targets.
#[derive(Debug, Clone, Copy)]
pub enum LoudnessTarget {
    /// EBU R128 integrated loudness, in LUFS (e.g. -20.0 for theatrical).
    IntegratedLufs(f64),
    /// ISO 21727 Leq(m), in dB.
    LeqM(f64),
}

/// Default true-peak ceiling, dBTP.
pub const DEFAULT_TRUE_PEAK_CEILING_DBTP: f64 = -1.0;

/// The numbers behind moving a measured level to a target with a linear gain,
/// and the resulting true peak (all before any file is written).
#[derive(Debug, Clone, Copy)]
pub struct GainPlan {
    pub measured_db: f64,
    pub target_db: f64,
    pub gain_db: f64,
    pub input_true_peak_dbtp: f64,
    pub resulting_true_peak_dbtp: f64,
    pub true_peak_ceiling_dbtp: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum AdjustError {
    #[error("wav i/o: {0}")]
    Wav(#[from] hound::Error),
    #[error("mxf pcm i/o: {0}")]
    Mxf(#[from] asdcplib::Error),
    #[error("{0}-bit pcm is not a depth the loudness readers widen")]
    UnsupportedPcmDepth(u16),
    #[error("ebur128: {0}")]
    Ebur128(#[from] ebur128::Error),
    #[error("no audio samples to measure")]
    Empty,
    #[error("measured level is {0} dB; cannot compute a finite gain")]
    NonFiniteMeasurement(f64),
    #[error(
        "true-peak ceiling exceeded: a gain of {gain_db:.2} dB would raise true peak from \
         {input_true_peak_dbtp:.2} to {resulting_true_peak_dbtp:.2} dBTP, above the \
         {true_peak_ceiling_dbtp:.2} dBTP ceiling (only {headroom_db:.2} dB headroom); \
         pass a lower target"
    )]
    TruePeakExceeded {
        gain_db: f64,
        input_true_peak_dbtp: f64,
        resulting_true_peak_dbtp: f64,
        true_peak_ceiling_dbtp: f64,
        headroom_db: f64,
    },
}

// decoded PCM in its native form. gain is applied by scaling the raw values, so
// int stays int at the same bit depth and float stays float.
enum Pcm {
    Int(Vec<i32>),
    Float(Vec<f32>),
}

impl Pcm {
    fn frame_count(&self, channels: usize) -> usize {
        let len = match self {
            Pcm::Int(s) => s.len(),
            Pcm::Float(s) => s.len(),
        };
        len / channels.max(1)
    }

    // interleaved, normalized to full-scale (-1.0..=1.0), for loudness measurement.
    fn normalized(&self, bits_per_sample: u16) -> Vec<f32> {
        match self {
            Pcm::Int(s) => {
                let fs = (1i64 << (bits_per_sample - 1)) as f32;
                s.iter().map(|&v| v as f32 / fs).collect()
            }
            Pcm::Float(s) => s.clone(),
        }
    }
}

fn load_pcm(input: &Path) -> Result<(WavSpec, Pcm), AdjustError> {
    let reader = WavReader::open(input)?;
    let spec = reader.spec();
    let pcm = match spec.sample_format {
        SampleFormat::Int => Pcm::Int(reader.into_samples::<i32>().collect::<Result<_, _>>()?),
        SampleFormat::Float => Pcm::Float(reader.into_samples::<f32>().collect::<Result<_, _>>()?),
    };
    Ok((spec, pcm))
}

// measured level (per the target's metric) and the input true peak in dBTP.
fn measure(spec: &WavSpec, pcm: &Pcm, target: LoudnessTarget) -> Result<(f64, f64), AdjustError> {
    let channels = spec.channels as usize;
    if channels == 0 || pcm.frame_count(channels) == 0 {
        return Err(AdjustError::Empty);
    }
    let interleaved = pcm.normalized(spec.bits_per_sample);

    let mut meter = EbuR128::new(
        spec.channels as u32,
        spec.sample_rate,
        Mode::I | Mode::TRUE_PEAK,
    )?;
    meter.add_frames_f32(&interleaved)?;
    let mut peak_linear = 0.0f64;
    for ch in 0..spec.channels as u32 {
        peak_linear = peak_linear.max(meter.true_peak(ch)?);
    }
    let true_peak_dbtp = 20.0 * peak_linear.log10();

    let measured = match target {
        LoudnessTarget::IntegratedLufs(_) => meter.loudness_global()?,
        LoudnessTarget::LeqM(_) => leq_m_from_interleaved(&interleaved, channels, spec.sample_rate),
    };
    Ok((measured, true_peak_dbtp))
}

/// Compute the gain needed to move `input` to `target` and check it against the
/// true-peak ceiling. Returns the plan, or fails loud (no writing, no limiting).
pub fn plan_gain(
    input: &Path,
    target: LoudnessTarget,
    true_peak_ceiling_dbtp: f64,
) -> Result<GainPlan, AdjustError> {
    let (spec, pcm) = load_pcm(input)?;
    let (measured_db, input_true_peak_dbtp) = measure(&spec, &pcm, target)?;
    if !measured_db.is_finite() {
        return Err(AdjustError::NonFiniteMeasurement(measured_db));
    }
    let target_db = match target {
        LoudnessTarget::IntegratedLufs(t) | LoudnessTarget::LeqM(t) => t,
    };
    let gain_db = target_db - measured_db;
    // a linear gain shifts true peak by exactly the same dB.
    let resulting_true_peak_dbtp = input_true_peak_dbtp + gain_db;
    if resulting_true_peak_dbtp > true_peak_ceiling_dbtp {
        return Err(AdjustError::TruePeakExceeded {
            gain_db,
            input_true_peak_dbtp,
            resulting_true_peak_dbtp,
            true_peak_ceiling_dbtp,
            headroom_db: true_peak_ceiling_dbtp - input_true_peak_dbtp,
        });
    }
    Ok(GainPlan {
        measured_db,
        target_db,
        gain_db,
        input_true_peak_dbtp,
        resulting_true_peak_dbtp,
        true_peak_ceiling_dbtp,
    })
}

/// Apply a pure sample-domain gain to a WAV, preserving format, bit depth,
/// channel count and sample rate. No resampling, no re-encode, no limiting.
pub fn apply_gain(input: &Path, output: &Path, gain_db: f64) -> Result<(), AdjustError> {
    let (spec, pcm) = load_pcm(input)?;
    let scale = 10f64.powf(gain_db / 20.0);
    let mut writer = WavWriter::create(output, spec)?;
    match pcm {
        Pcm::Int(samples) => {
            for s in samples {
                // scale the raw integer directly, so the bit depth is unchanged.
                writer.write_sample((s as f64 * scale).round() as i32)?;
            }
        }
        Pcm::Float(samples) => {
            let g = scale as f32;
            for s in samples {
                writer.write_sample(s * g)?;
            }
        }
    }
    writer.finalize()?;
    Ok(())
}

/// Measure `input`, compute the gain to hit `target`, guard the true-peak
/// ceiling, then write the adjusted WAV to `output`. Fails loud (writing
/// nothing) if the gain would breach the ceiling.
pub fn adjust_loudness(
    input: &Path,
    output: &Path,
    target: LoudnessTarget,
    true_peak_ceiling_dbtp: f64,
) -> Result<GainPlan, AdjustError> {
    let plan = plan_gain(input, target, true_peak_ceiling_dbtp)?;
    apply_gain(input, output, plan.gain_db)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::process::Command;

    // what ffmpeg's loudnorm filter measured, the oracle these tests hold to
    struct LoudnormOracle {
        integrated_lufs: f64,
        range_lu: f64,
        true_peak_dbtp: f64,
    }

    fn parse_loudnorm(stderr: &str) -> Option<LoudnormOracle> {
        let start = stderr.rfind('{')?;
        let end = stderr[start..].find('}')?;
        let value: serde_json::Value =
            serde_json::from_str(&stderr[start..start + end + 1]).ok()?;
        let field = |name: &str| value[name].as_str().and_then(|s| s.parse::<f64>().ok());
        Some(LoudnormOracle {
            integrated_lufs: field("input_i")?,
            range_lu: field("input_lra")?,
            true_peak_dbtp: field("input_tp")?,
        })
    }

    fn loudnorm_oracle(input: &Path) -> LoudnormOracle {
        let output = Command::new("ffmpeg")
            .args([
                "-i",
                &input.to_string_lossy(),
                "-af",
                "loudnorm=print_format=json",
                "-f",
                "null",
                "-",
            ])
            .output()
            .expect("ffmpeg must be on PATH");
        let stderr = String::from_utf8_lossy(&output.stderr);
        parse_loudnorm(&stderr).unwrap_or_else(|| panic!("no loudnorm json in:\n{stderr}"))
    }

    // largest finite `S:` (short-term LUFS) in ebur128 stderr output
    fn parse_short_term_max(stderr: &str) -> Option<f64> {
        let mut max: Option<f64> = None;
        for line in stderr.lines() {
            if let Some(pos) = line.find("S:") {
                let rest = line[pos + 2..].trim_start();
                let token = rest.split_whitespace().next().unwrap_or("");
                if let Ok(v) = token.parse::<f64>()
                    && v.is_finite()
                {
                    max = Some(max.map_or(v, |m: f64| m.max(v)));
                }
            }
        }
        max
    }

    fn ebur128_short_term_max_oracle(input: &Path) -> f64 {
        let output = Command::new("ffmpeg")
            .args([
                "-i",
                &input.to_string_lossy(),
                "-af",
                "ebur128",
                "-f",
                "null",
                "-",
            ])
            .output()
            .expect("ffmpeg must be on PATH");
        let stderr = String::from_utf8_lossy(&output.stderr);
        parse_short_term_max(&stderr).unwrap_or_else(|| panic!("no ebur128 S: lines in:\n{stderr}"))
    }

    #[test]
    fn parses_max_short_term_from_ebur128() {
        let stderr = "\
[Parsed_ebur128_0 @ 0x1] t: 1.0 M: -30.0 S: -25.0 I: -24.0 LUFS LRA: 2.0 LU
[Parsed_ebur128_0 @ 0x1] t: 2.0 M: -20.0 S: -18.5 I: -23.0 LUFS LRA: 3.0 LU
[Parsed_ebur128_0 @ 0x1] t: 3.0 M: -40.0 S: -inf I: -23.0 LUFS LRA: 3.0 LU
";
        let v = parse_short_term_max(stderr).unwrap();
        assert!((v - (-18.5)).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn short_term_none_without_measurements() {
        assert!(parse_short_term_max("no ebur128 lines here").is_none());
    }

    #[test]
    fn the_m_curve_reads_the_iso_21727_table() {
        for (frequency, table_db) in M_WEIGHTING_HZ.iter().zip(M_WEIGHTING_DB) {
            assert!(
                (m_weighting_db(*frequency) - table_db).abs() < 1e-12,
                "{frequency} Hz reads {} dB, the table says {table_db}",
                m_weighting_db(*frequency)
            );
        }
        // between points it is a straight line in dB: half way from 100 Hz
        // (-25.4) to 200 Hz (-19.4)
        assert!((m_weighting_db(150.0) - (-22.4)).abs() < 1e-9);
        // and it holds its end values outside the tabulated band
        assert_eq!(m_weighting_db(10.0), -35.5);
        assert_eq!(m_weighting_db(40000.0), -48.3);
        // the fft bin weights carry the same curve, squared
        let weights = bin_weights_squared(48000);
        let bin_2khz = (2000.0 * LEQ_BLOCK as f64 / 48000.0).round() as usize;
        assert!(
            (weights[bin_2khz] - 1.0).abs() < 1e-3,
            "{}",
            weights[bin_2khz]
        );
    }

    // A full-scale (amplitude 1.0) sine at 1 kHz has RMS 1/sqrt(2), i.e. mean
    // square 0.5, so 10*log10(0.5) = -3.01 dBFS. M weighting is -5.6 dB at
    // 1 kHz, and full scale is 108.01 dB, so Leq(m) = 108.01 - 3.01 - 5.6 =
    // 99.40 dB.
    #[test]
    fn full_scale_1khz_sine_matches_derived_leq_m() {
        let sr = 48000u32;
        let n = sr as usize; // 1 second
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let leq = leq_m_from_samples(&samples, sr);
        assert!(
            (leq - 99.40).abs() < 0.1,
            "Leq(m) was {leq}, expected ~99.40 dB"
        );
    }

    // Halving the amplitude drops the level by 6.02 dB.
    #[test]
    fn half_amplitude_sine_is_6db_lower() {
        let sr = 48000u32;
        let n = sr as usize;
        let full: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let half: Vec<f32> = full.iter().map(|s| s * 0.5).collect();
        let d = leq_m_from_samples(&full, sr) - leq_m_from_samples(&half, sr);
        assert!(
            (d - 6.02).abs() < 0.1,
            "difference was {d} dB, expected ~6.02"
        );
    }

    // write a `seconds`-long `freq` Hz tone at `amplitude` (0..1 full-scale) to a
    // wav with `spec` on every channel.
    fn write_tone(path: &Path, spec: WavSpec, freq: f32, amplitude: f32, seconds: f32) {
        let sr = spec.sample_rate;
        let frames = (sr as f32 * seconds) as usize;
        let mut w = WavWriter::create(path, spec).unwrap();
        let full_scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
        for i in 0..frames {
            let s = amplitude * (2.0 * PI * freq * i as f32 / sr as f32).sin();
            for _ in 0..spec.channels {
                match spec.sample_format {
                    SampleFormat::Int => w.write_sample((s * full_scale).round() as i32).unwrap(),
                    SampleFormat::Float => w.write_sample(s).unwrap(),
                }
            }
        }
        w.finalize().unwrap();
    }

    fn int_spec(channels: u16, bits: u16) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: 48000,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        }
    }

    #[test]
    fn adjust_hits_integrated_lufs_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        write_tone(&src, int_spec(2, 16), 1000.0, 0.5, 3.0);

        let target = LoudnessTarget::IntegratedLufs(-23.0);
        let plan = adjust_loudness(&src, &dst, target, DEFAULT_TRUE_PEAK_CEILING_DBTP).unwrap();
        // re-measure the written file; its measured level must now be the target.
        let (remeasured, _) =
            measure(&int_spec(2, 16), &load_pcm(&dst).unwrap().1, target).unwrap();
        assert!(
            (remeasured - (-23.0)).abs() < 0.3,
            "re-measured {remeasured} LUFS, target -23; plan {plan:?}"
        );
    }

    #[test]
    fn adjust_hits_leq_m_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        write_tone(&src, int_spec(1, 16), 1000.0, 0.5, 2.0);

        let target = LoudnessTarget::LeqM(85.0);
        adjust_loudness(&src, &dst, target, DEFAULT_TRUE_PEAK_CEILING_DBTP).unwrap();
        let (remeasured, _) =
            measure(&int_spec(1, 16), &load_pcm(&dst).unwrap().1, target).unwrap();
        assert!(
            (remeasured - 85.0).abs() < 0.15,
            "re-measured {remeasured} dB Leq(m), target 85"
        );
    }

    #[test]
    fn headroom_exceeded_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        write_tone(&src, int_spec(1, 16), 1000.0, 0.5, 2.0);

        // a 120 dB Leq(m) target needs ~+24 dB, pushing the ~-6 dBTP peak well
        // over the -1 dBTP ceiling.
        let err = adjust_loudness(&src, &dst, LoudnessTarget::LeqM(120.0), -1.0).unwrap_err();
        match err {
            AdjustError::TruePeakExceeded {
                gain_db,
                input_true_peak_dbtp,
                resulting_true_peak_dbtp,
                true_peak_ceiling_dbtp,
                headroom_db,
            } => {
                assert!(gain_db > 0.0, "expected positive gain, got {gain_db}");
                assert!((true_peak_ceiling_dbtp - (-1.0)).abs() < 1e-9);
                assert!((resulting_true_peak_dbtp - (input_true_peak_dbtp + gain_db)).abs() < 1e-9);
                assert!(
                    (headroom_db - (true_peak_ceiling_dbtp - input_true_peak_dbtp)).abs() < 1e-9
                );
                assert!(gain_db > headroom_db, "gain must exceed the headroom");
            }
            other => panic!("expected TruePeakExceeded, got {other:?}"),
        }
        // nothing was written.
        assert!(!dst.exists());
    }

    // a `seconds`-long `freq` Hz tone at `amplitude` on one channel, silence on
    // the rest, so the measured peak is the tone's and nothing sums into it.
    fn write_tone_on_channel(
        path: &Path,
        spec: WavSpec,
        on_channel: u16,
        freq: f32,
        amplitude: f32,
        seconds: f32,
    ) {
        let sr = spec.sample_rate;
        let frames = (sr as f32 * seconds) as usize;
        let mut w = WavWriter::create(path, spec).unwrap();
        let full_scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
        for i in 0..frames {
            let s = amplitude * (2.0 * PI * freq * i as f32 / sr as f32).sin();
            for channel in 0..spec.channels {
                let value = if channel == on_channel { s } else { 0.0 };
                match spec.sample_format {
                    SampleFormat::Int => {
                        w.write_sample((value * full_scale).round() as i32).unwrap()
                    }
                    SampleFormat::Float => w.write_sample(value).unwrap(),
                }
            }
        }
        w.finalize().unwrap();
    }

    // one meter over the whole normalized buffer in a single call: the answer
    // blocking, de-interleaving and threading must not move.
    fn true_peak_in_one_call(path: &Path) -> f64 {
        let (spec, pcm) = load_pcm(path).unwrap();
        let interleaved = pcm.normalized(spec.bits_per_sample);
        let mut meter =
            EbuR128::new(spec.channels as u32, spec.sample_rate, Mode::TRUE_PEAK).unwrap();
        meter.add_frames_f32(&interleaved).unwrap();
        let mut peak = 0.0f64;
        for channel in 0..spec.channels as u32 {
            peak = peak.max(meter.true_peak(channel).unwrap());
        }
        20.0 * peak.log10()
    }

    #[test]
    fn true_peak_of_a_half_amplitude_tone_matches_a_single_meter() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tone.wav");
        write_tone_on_channel(&src, int_spec(2, 24), 0, 997.0, 0.5, 4.0);

        let measured = measure_true_peak_dbtp(&src).unwrap();
        assert!(
            (measured - (-6.02)).abs() < 0.1,
            "measured {measured} dBTP, expected ~-6.02"
        );
        let reference = true_peak_in_one_call(&src);
        assert!(
            (measured - reference).abs() < 1e-9,
            "streamed {measured} dBTP, single meter {reference}"
        );
    }

    #[test]
    fn true_peak_agrees_with_loudnorm() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tone.wav");
        write_tone_on_channel(&src, int_spec(2, 24), 0, 997.0, 0.5, 4.0);

        let measured = measure_true_peak_dbtp(&src).unwrap();
        let oracle = loudnorm_oracle(&src);
        let difference = (measured - oracle.true_peak_dbtp).abs();
        assert!(
            difference < 0.1,
            "rust {measured} dBTP, loudnorm {} dBTP",
            oracle.true_peak_dbtp
        );
    }

    // a 997 Hz tone stepping -30, -12, -30 dBFS on the left with the right 6 dB
    // down, so the integrated level, the range and the short term max all differ
    fn write_stepped_tone(path: &Path, spec: WavSpec, step_seconds: f32) {
        const RIGHT_CHANNEL_SCALE: f32 = 0.5;
        let amplitudes = [0.031_622_776, 0.251_188_64, 0.031_622_776];
        let sample_rate = spec.sample_rate;
        let step_frames = (sample_rate as f32 * step_seconds) as usize;
        let full_scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
        let mut writer = WavWriter::create(path, spec).unwrap();
        for (step, amplitude) in amplitudes.iter().enumerate() {
            for frame in 0..step_frames {
                let index = (step * step_frames + frame) as f32;
                let value = amplitude * (2.0 * PI * 997.0 * index / sample_rate as f32).sin();
                for channel in 0..spec.channels {
                    let scaled = if channel == 0 {
                        value
                    } else {
                        value * RIGHT_CHANNEL_SCALE
                    };
                    writer
                        .write_sample((scaled * full_scale).round() as i32)
                        .unwrap();
                }
            }
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn loudness_agrees_with_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stepped.wav");
        write_stepped_tone(&src, int_spec(2, 24), 6.0);

        let measured = measure_loudness(&src);
        assert!(measured.success, "{}", measured.error);
        let oracle = loudnorm_oracle(&src);
        assert!(
            (measured.integrated_lufs - oracle.integrated_lufs).abs() < 0.1,
            "rust {} LUFS, loudnorm {} LUFS",
            measured.integrated_lufs,
            oracle.integrated_lufs
        );
        assert!(
            (measured.range_lu - oracle.range_lu).abs() < 0.5,
            "rust {} LU, loudnorm {} LU",
            measured.range_lu,
            oracle.range_lu
        );
        assert!(
            (measured.true_peak_dbtp - oracle.true_peak_dbtp).abs() < 0.1,
            "rust {} dBTP, loudnorm {} dBTP",
            measured.true_peak_dbtp,
            oracle.true_peak_dbtp
        );
        let short_term_oracle = ebur128_short_term_max_oracle(&src);
        assert!(
            (measured.short_term_max_lufs - short_term_oracle).abs() < 0.15,
            "rust {} LUFS, ebur128 filter {short_term_oracle} LUFS",
            measured.short_term_max_lufs
        );
    }

    #[test]
    fn leq_m_matches_the_in_memory_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stepped6.wav");
        write_stepped_tone(&src, int_spec(6, 24), 2.0);

        let streamed = measure_leq_m(&src);
        assert!(streamed.success, "{}", streamed.error);
        let (spec, pcm) = load_pcm(&src).unwrap();
        let interleaved = pcm.normalized(spec.bits_per_sample);
        let in_memory =
            leq_m_from_interleaved(&interleaved, spec.channels as usize, spec.sample_rate);
        assert!(
            (streamed.leq_m_db - in_memory).abs() < 0.01,
            "streamed {} dB, in memory {in_memory} dB",
            streamed.leq_m_db
        );
    }

    #[test]
    fn six_channels_of_one_tone_sum_by_their_corrections() {
        let dir = tempfile::tempdir().unwrap();
        let mono = dir.path().join("mono.wav");
        let six = dir.path().join("six.wav");
        write_tone(&mono, int_spec(1, 24), 1000.0, 0.5, 2.0);
        write_tone(&six, int_spec(6, 24), 1000.0, 0.5, 2.0);

        let from_mono = measure_leq_m(&mono);
        let from_six = measure_leq_m(&six);
        assert!(from_six.success, "{}", from_six.error);
        // L R C LFE at 0 dB and Ls Rs at -3 dB, in energy
        let expected = 10.0 * (4.0 + 2.0 * 10f64.powf(-0.3)).log10();
        let measured = from_six.leq_m_db - from_mono.leq_m_db;
        assert!(
            (measured - expected).abs() < 0.05,
            "six channels are {measured} dB over mono, expected {expected}"
        );
    }

    #[test]
    fn the_hearing_impaired_channel_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let on_left = dir.path().join("left.wav");
        let on_hi = dir.path().join("hi.wav");
        write_tone_on_channel(&on_left, int_spec(8, 24), 0, 1000.0, 0.5, 2.0);
        write_tone_on_channel(&on_hi, int_spec(8, 24), 6, 1000.0, 0.5, 2.0);

        let left = measure_leq_m(&on_left);
        let hearing_impaired = measure_leq_m(&on_hi);
        assert!(hearing_impaired.success, "{}", hearing_impaired.error);
        assert!(
            hearing_impaired.leq_m_db < left.leq_m_db - 100.0,
            "HI read {} dB against L's {} dB",
            hearing_impaired.leq_m_db,
            left.leq_m_db
        );
    }

    // this crate's own pcm wrap, so the mxf under test is one a dcp would carry
    fn wrap_pcm_mxf(wav: &Path, output: &Path) {
        let options = crate::mxf_wrap::MxfWrapOptions {
            input_files: vec![wav.to_path_buf()],
            output: output.to_path_buf(),
            essence_type: crate::mxf_wrap::EssenceType::Pcm,
            standard: crate::mxf_wrap::MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: None,
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
        };
        let wrapped = crate::mxf_wrap::mxf_wrap(&options);
        assert!(wrapped.success, "pcm wrap failed: {}", wrapped.error);
    }

    #[test]
    fn a_pcm_mxf_measures_the_same_as_its_wav() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("stepped6.wav");
        let mxf = dir.path().join("stepped6.mxf");
        write_stepped_tone(&wav, int_spec(6, 24), 2.0);
        wrap_pcm_mxf(&wav, &mxf);

        let from_wav = measure_leq_m(&wav);
        let from_mxf = measure_leq_m(&mxf);
        assert!(from_mxf.success, "{}", from_mxf.error);
        assert!(
            (from_mxf.leq_m_db - from_wav.leq_m_db).abs() < 0.01,
            "mxf {} dB, wav {} dB",
            from_mxf.leq_m_db,
            from_wav.leq_m_db
        );

        let wav_loudness = measure_loudness(&wav);
        let mxf_loudness = measure_loudness(&mxf);
        assert!(mxf_loudness.success, "{}", mxf_loudness.error);
        for (name, from_mxf, from_wav) in [
            (
                "integrated",
                mxf_loudness.integrated_lufs,
                wav_loudness.integrated_lufs,
            ),
            ("range", mxf_loudness.range_lu, wav_loudness.range_lu),
            (
                "true peak",
                mxf_loudness.true_peak_dbtp,
                wav_loudness.true_peak_dbtp,
            ),
            (
                "short term max",
                mxf_loudness.short_term_max_lufs,
                wav_loudness.short_term_max_lufs,
            ),
        ] {
            assert!(
                (from_mxf - from_wav).abs() < 0.01,
                "{name}: mxf {from_mxf}, wav {from_wav}"
            );
        }

        let peak_from_mxf = measure_true_peak_dbtp(&mxf).unwrap();
        let peak_from_wav = measure_true_peak_dbtp(&wav).unwrap();
        assert!(
            (peak_from_mxf - peak_from_wav).abs() < 0.01,
            "mxf {peak_from_mxf} dBTP, wav {peak_from_wav} dBTP"
        );
    }

    #[test]
    fn a_file_that_is_neither_wav_nor_mxf_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("audio.dat");
        std::fs::write(&bogus, b"neither riff nor an mxf partition pack").unwrap();

        let loudness = measure_loudness(&bogus);
        assert!(!loudness.success);
        assert!(!loudness.error.is_empty());
        let leq_m = measure_leq_m(&bogus);
        assert!(!leq_m.success);
        assert!(!leq_m.error.is_empty());
        assert!(measure_true_peak_dbtp(&bogus).is_err());
    }

    #[test]
    fn a_missing_file_fails_loud() {
        let missing = Path::new("/nonexistent/loudness/input.wav");
        let loudness = measure_loudness(missing);
        assert!(!loudness.success);
        assert!(!loudness.error.is_empty());
        let leq_m = measure_leq_m(missing);
        assert!(!leq_m.success);
        assert!(!leq_m.error.is_empty());
    }

    #[test]
    fn a_wav_with_no_frames_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("silent.wav");
        WavWriter::create(&src, int_spec(2, 24))
            .unwrap()
            .finalize()
            .unwrap();

        let loudness = measure_loudness(&src);
        assert!(!loudness.success);
        assert!(!loudness.error.is_empty());
        let leq_m = measure_leq_m(&src);
        assert!(!leq_m.success);
        assert!(!leq_m.error.is_empty());
    }

    #[test]
    fn true_peak_of_a_wav_with_no_frames_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("silent.wav");
        WavWriter::create(&src, int_spec(2, 24))
            .unwrap()
            .finalize()
            .unwrap();

        match measure_true_peak_dbtp(&src) {
            Err(AdjustError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[test]
    fn true_peak_is_the_same_at_16_bit_and_as_float() {
        let dir = tempfile::tempdir().unwrap();
        let float_spec = WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut measured = Vec::new();
        for (name, spec) in [
            ("i24.wav", int_spec(2, 24)),
            ("i16.wav", int_spec(2, 16)),
            ("f32.wav", float_spec),
        ] {
            let src = dir.path().join(name);
            write_tone_on_channel(&src, spec, 0, 997.0, 0.5, 4.0);
            measured.push((name, measure_true_peak_dbtp(&src).unwrap()));
        }
        let (_, reference) = measured[0];
        for (name, value) in &measured {
            assert!(
                (value - reference).abs() < 0.01,
                "{name} measured {value} dBTP, 24 bit measured {reference}"
            );
        }
    }

    #[test]
    fn bit_depth_and_channels_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let dst = dir.path().join("out.wav");
        let spec = int_spec(2, 24);
        write_tone(&src, spec, 1000.0, 0.5, 1.0);

        adjust_loudness(
            &src,
            &dst,
            LoudnessTarget::LeqM(80.0),
            DEFAULT_TRUE_PEAK_CEILING_DBTP,
        )
        .unwrap();

        let out = WavReader::open(&dst).unwrap();
        let out_spec = out.spec();
        assert_eq!(out_spec.channels, 2);
        assert_eq!(out_spec.bits_per_sample, 24);
        assert_eq!(out_spec.sample_format, SampleFormat::Int);
        assert_eq!(out.duration(), WavReader::open(&src).unwrap().duration());
    }
}
