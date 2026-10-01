// RIFF and RF64 wav <-> interleaved samples, preserving sample format and bit depth. The
// exact pair keeps the file's own sample type, so a read/write round-trip is
// byte-identical at any depth the writer accepts; the f32 pair normalises for the
// DSP modules (upmix, crossfade, mid-side) and loses the low bits of 32-bit
// int, whose 32 significant bits do not fit an f32 mantissa. loudness keeps its
// own copy.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::Path;

pub use hound::{SampleFormat, WavSpec};

/// The form types a WAV arrives in: plain RIFF, and the two 64-bit forms
/// BS.2088 defines, whose sizes live in a `ds64` chunk.
const WAVE_FORM_TYPES: [&[u8; 4]; 3] = [b"RIFF", b"RF64", b"BW64"];

/// What a 32-bit RIFF size field holds when the real size is in the `ds64` chunk.
const SIZE_IS_IN_DS64: u32 = 0xFFFF_FFFF;

/// Form type, size and `WAVE` together.
const RIFF_HEADER_BYTES: u64 = 12;

/// A chunk's FourCC and its 32-bit size.
const CHUNK_HEADER_BYTES: u64 = 8;

/// riffSize, dataSize and sampleCount, then the table length.
const DS64_FIXED_BYTES: usize = 28;

/// A FourCC and a 64-bit size.
const DS64_TABLE_ENTRY_BYTES: usize = 12;

/// The 64-bit sizes a `ds64` chunk carries (ITU-R BS.2088): the data chunk's, plus
/// one table entry for every other chunk whose 32-bit size field is 0xFFFFFFFF.
struct Ds64Sizes {
    data_size: u64,
    table: Vec<([u8; 4], u64)>,
}

impl Ds64Sizes {
    fn parse(payload: &[u8]) -> io::Result<Self> {
        if payload.len() < DS64_FIXED_BYTES {
            return Err(invalid(format!(
                "ds64 chunk is {} bytes, too short to hold the 64-bit sizes",
                payload.len()
            )));
        }
        let data_size = u64::from_le_bytes(payload[8..16].try_into().unwrap());
        let entries = u32::from_le_bytes(payload[24..28].try_into().unwrap()) as usize;
        let wanted = DS64_FIXED_BYTES + entries * DS64_TABLE_ENTRY_BYTES;
        if payload.len() < wanted {
            return Err(invalid(format!(
                "ds64 chunk declares {entries} sizes but is only {} bytes",
                payload.len()
            )));
        }
        let table = payload[DS64_FIXED_BYTES..wanted]
            .as_chunks::<DS64_TABLE_ENTRY_BYTES>()
            .0
            .iter()
            .map(|entry| {
                (
                    entry[..4].try_into().unwrap(),
                    u64::from_le_bytes(entry[4..].try_into().unwrap()),
                )
            })
            .collect();
        Ok(Self { data_size, table })
    }

    fn size_of(&self, chunk_id: &[u8; 4]) -> Option<u64> {
        if chunk_id == b"data" {
            return Some(self.data_size);
        }
        self.table
            .iter()
            .find_map(|(id, size)| (id == chunk_id).then_some(*size))
    }
}

fn chunk_name(chunk_id: &[u8; 4]) -> String {
    String::from_utf8_lossy(chunk_id).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub id: [u8; 4],
    // where the chunk's 8-byte header starts
    pub offset: u64,
    pub size: u64,
}

impl Chunk {
    pub fn body_offset(&self) -> u64 {
        self.offset + CHUNK_HEADER_BYTES
    }

    fn end(&self) -> u64 {
        self.body_offset() + self.size + self.size % 2
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

// a file too short for a form type is not a WAV either
pub fn has_wave_form_type(path: &Path) -> io::Result<bool> {
    let mut form_type = [0u8; 4];
    match File::open(path)?.read_exact(&mut form_type) {
        Ok(()) => Ok(WAVE_FORM_TYPES.contains(&&form_type)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn read_chunks<R: Read + Seek>(reader: &mut R) -> io::Result<Vec<Chunk>> {
    let file_length = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    let mut header = [0u8; RIFF_HEADER_BYTES as usize];
    reader
        .read_exact(&mut header)
        .map_err(|_| invalid("not a RIFF/WAVE file".into()))?;
    let form_type: [u8; 4] = header[..4].try_into().unwrap();
    if !WAVE_FORM_TYPES.contains(&&form_type) {
        return Err(invalid(format!(
            "not a RIFF, RF64 or BW64 file: form type is {}",
            chunk_name(&form_type)
        )));
    }
    if &header[8..12] != b"WAVE" {
        return Err(invalid("not a WAVE file".into()));
    }

    let mut chunks = Vec::new();
    let mut ds64: Option<Ds64Sizes> = None;
    let mut position = RIFF_HEADER_BYTES;
    while position + CHUNK_HEADER_BYTES <= file_length {
        reader.seek(SeekFrom::Start(position))?;
        let mut chunk_header = [0u8; CHUNK_HEADER_BYTES as usize];
        reader.read_exact(&mut chunk_header)?;
        let chunk_id: [u8; 4] = chunk_header[..4].try_into().unwrap();
        let declared_size = u32::from_le_bytes(chunk_header[4..].try_into().unwrap());

        let chunk_size = match declared_size {
            SIZE_IS_IN_DS64 => {
                let sizes = ds64.as_ref().ok_or_else(|| {
                    invalid(format!(
                        "{} chunk takes its size from a ds64 chunk this file does not have",
                        chunk_name(&chunk_id)
                    ))
                })?;
                sizes.size_of(&chunk_id).ok_or_else(|| {
                    invalid(format!(
                        "ds64 chunk carries no size for the {} chunk",
                        chunk_name(&chunk_id)
                    ))
                })?
            }
            size => size as u64,
        };
        let chunk = Chunk {
            id: chunk_id,
            offset: position,
            size: chunk_size,
        };
        if chunk
            .body_offset()
            .checked_add(chunk_size)
            .is_none_or(|end| end > file_length)
        {
            return Err(invalid(format!(
                "{} chunk claims {chunk_size} bytes at offset {}, past the end of the {file_length} byte file",
                chunk_name(&chunk_id),
                chunk.body_offset()
            )));
        }

        if &chunk_id == b"ds64" {
            let mut payload = vec![0u8; chunk_size as usize];
            reader.read_exact(&mut payload)?;
            ds64 = Some(Ds64Sizes::parse(&payload)?);
        }
        chunks.push(chunk);
        position = chunk.end();
    }
    Ok(chunks)
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

// a fmt chunk is 16 to 40 bytes
const MAXIMUM_FMT_CHUNK_BYTES: u64 = 4096;
const PLAIN_FMT_BYTES: u32 = 16;
const EXTENSIBLE_FMT_BYTES: u32 = 40;
const EXTENSIBLE_EXTRA_BYTES: u16 = 22;
// hound's default channel mask stops at the 18 named speaker positions
const NAMED_SPEAKER_POSITIONS: u16 = 18;
const DS64_CHUNK_BYTES: u32 = DS64_FIXED_BYTES as u32;
const IO_BUFFER_BYTES: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct WavLayout {
    pub spec: WavSpec,
    // can exceed bits_per_sample / 8
    pub bytes_per_sample: u16,
    pub chunks: Vec<Chunk>,
    pub data: Chunk,
}

impl WavLayout {
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::parse(&mut File::open(path)?)
    }

    pub fn parse<R: Read + Seek>(reader: &mut R) -> io::Result<Self> {
        let chunks = read_chunks(reader)?;
        let fmt = *chunks
            .iter()
            .find(|chunk| &chunk.id == b"fmt ")
            .ok_or_else(|| invalid("no fmt chunk".into()))?;
        if fmt.size < PLAIN_FMT_BYTES as u64 {
            return Err(invalid("fmt chunk is too short".into()));
        }
        if fmt.size > MAXIMUM_FMT_CHUNK_BYTES {
            return Err(invalid(format!("fmt chunk claims {} bytes", fmt.size)));
        }
        let mut body = vec![0u8; fmt.size as usize];
        reader.seek(SeekFrom::Start(fmt.body_offset()))?;
        reader.read_exact(&mut body)?;
        let le_u16 = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);

        let mut tag = le_u16(0);
        let channels = le_u16(2);
        let sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let block_align = le_u16(12);
        let mut bits_per_sample = le_u16(14);
        if tag == WAVE_FORMAT_EXTENSIBLE {
            if fmt.size < EXTENSIBLE_FMT_BYTES as u64 {
                return Err(invalid(
                    "extensible fmt chunk is too short for a SubFormat".into(),
                ));
            }
            // files in the wild leave valid bits at zero
            if le_u16(18) > 0 {
                bits_per_sample = le_u16(18);
            }
            tag = le_u16(24);
        }
        let sample_format = match tag {
            WAVE_FORMAT_PCM => SampleFormat::Int,
            WAVE_FORMAT_IEEE_FLOAT => SampleFormat::Float,
            other => {
                return Err(invalid(format!(
                    "audio format {other:#06x} is neither linear PCM nor float"
                )));
            }
        };
        let bytes_per_sample = block_align.checked_div(channels).unwrap_or(0);
        let usable = sample_rate > 0
            && (1..=4).contains(&bytes_per_sample)
            && block_align == bytes_per_sample * channels
            && (1..=bytes_per_sample * 8).contains(&bits_per_sample)
            && (sample_format == SampleFormat::Int || bits_per_sample == 32);
        if !usable {
            return Err(invalid(format!(
                "unusable sample layout: {channels} channels, {sample_rate} Hz, \
                 {bits_per_sample} bits in {block_align} byte frames"
            )));
        }

        let data = *chunks
            .iter()
            .find(|chunk| &chunk.id == b"data")
            .ok_or_else(|| invalid("no data chunk".into()))?;
        Ok(Self {
            spec: WavSpec {
                channels,
                sample_rate,
                bits_per_sample,
                sample_format,
            },
            bytes_per_sample,
            chunks,
            data,
        })
    }

    pub fn block_align(&self) -> u64 {
        self.bytes_per_sample as u64 * self.spec.channels as u64
    }

    pub fn frames(&self) -> u64 {
        self.data.size / self.block_align()
    }
}

pub trait Sample: Sized {
    fn decode(bytes: &[u8], format: SampleFormat) -> io::Result<Self>;
    fn encode(self, format: SampleFormat, bits: u16, out: &mut [u8]) -> io::Result<()>;
}

impl Sample for i32 {
    fn decode(bytes: &[u8], format: SampleFormat) -> io::Result<Self> {
        if format != SampleFormat::Int {
            return Err(invalid("float samples read as integers".into()));
        }
        Ok(match *bytes {
            // 8-bit PCM is unsigned with silence at 128
            [b] => b as i32 - 128,
            [b0, b1] => i16::from_le_bytes([b0, b1]) as i32,
            [b0, b1, b2] => i32::from_le_bytes([0, b0, b1, b2]) >> 8,
            [b0, b1, b2, b3] => i32::from_le_bytes([b0, b1, b2, b3]),
            _ => unreachable!("WavLayout::parse keeps samples to 1 to 4 bytes"),
        })
    }

    fn encode(self, format: SampleFormat, bits: u16, out: &mut [u8]) -> io::Result<()> {
        if format != SampleFormat::Int {
            return Err(invalid("integer samples written to a float WAV".into()));
        }
        let top = 1i64 << (bits - 1);
        if !(-top..top).contains(&(self as i64)) {
            return Err(invalid(format!("sample {self} does not fit {bits} bits")));
        }
        let stored = if out.len() == 1 { self + 128 } else { self };
        out.copy_from_slice(&stored.to_le_bytes()[..out.len()]);
        Ok(())
    }
}

impl Sample for f32 {
    fn decode(bytes: &[u8], format: SampleFormat) -> io::Result<Self> {
        if format != SampleFormat::Float {
            return Err(invalid("integer samples read as float".into()));
        }
        Ok(f32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn encode(self, format: SampleFormat, _bits: u16, out: &mut [u8]) -> io::Result<()> {
        if format != SampleFormat::Float {
            return Err(invalid("float samples written to an integer WAV".into()));
        }
        out.copy_from_slice(&self.to_le_bytes());
        Ok(())
    }
}

pub struct WavReader {
    layout: WavLayout,
    source: BufReader<File>,
    samples_left: u64,
}

impl WavReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let layout = WavLayout::parse(&mut file)?;
        file.seek(SeekFrom::Start(layout.data.body_offset()))?;
        let samples_left = layout.frames() * layout.spec.channels as u64;
        Ok(Self {
            layout,
            source: BufReader::with_capacity(IO_BUFFER_BYTES, file),
            samples_left,
        })
    }

    pub fn spec(&self) -> WavSpec {
        self.layout.spec
    }

    pub fn layout(&self) -> &WavLayout {
        &self.layout
    }

    // in frames, one sample per channel
    pub fn duration(&self) -> u64 {
        self.layout.frames()
    }

    pub fn len(&self) -> u64 {
        self.layout.frames() * self.layout.spec.channels as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn samples<S: Sample>(&mut self) -> WavSamples<'_, S> {
        WavSamples {
            reader: self,
            sample_type: PhantomData,
        }
    }

    pub fn into_samples<S: Sample>(self) -> WavIntoSamples<S> {
        WavIntoSamples {
            reader: self,
            sample_type: PhantomData,
        }
    }

    fn next_sample<S: Sample>(&mut self) -> Option<io::Result<S>> {
        if self.samples_left == 0 {
            return None;
        }
        self.samples_left -= 1;
        let mut bytes = [0u8; 4];
        let bytes = &mut bytes[..self.layout.bytes_per_sample as usize];
        Some(
            self.source
                .read_exact(bytes)
                .and_then(|()| S::decode(bytes, self.layout.spec.sample_format)),
        )
    }
}

pub struct WavSamples<'a, S> {
    reader: &'a mut WavReader,
    sample_type: PhantomData<S>,
}

impl<S: Sample> Iterator for WavSamples<'_, S> {
    type Item = io::Result<S>;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next_sample()
    }
}

pub struct WavIntoSamples<S> {
    reader: WavReader,
    sample_type: PhantomData<S>,
}

impl<S: Sample> Iterator for WavIntoSamples<S> {
    type Item = io::Result<S>;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next_sample()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Riff,
    Rf64,
}

fn riff_size(form: Form, chunks_len: u64, data_len: u64) -> u64 {
    let ds64 = match form {
        Form::Riff => 0,
        Form::Rf64 => CHUNK_HEADER_BYTES + DS64_FIXED_BYTES as u64,
    };
    b"WAVE".len() as u64 + ds64 + chunks_len + CHUNK_HEADER_BYTES + data_len
}

fn header(form: Form, chunks: &[u8], data_len: u64, frames: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunks.len() + 64);
    let size = riff_size(form, chunks.len() as u64, data_len);
    match form {
        Form::Riff => {
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&(size as u32).to_le_bytes());
            out.extend_from_slice(b"WAVE");
        }
        Form::Rf64 => {
            out.extend_from_slice(b"RF64");
            out.extend_from_slice(&SIZE_IS_IN_DS64.to_le_bytes());
            out.extend_from_slice(b"WAVE");
            out.extend_from_slice(b"ds64");
            out.extend_from_slice(&DS64_CHUNK_BYTES.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&data_len.to_le_bytes());
            out.extend_from_slice(&frames.to_le_bytes());
            // no other chunk needs a 64-bit size
            out.extend_from_slice(&0u32.to_le_bytes());
        }
    }
    out.extend_from_slice(chunks);
    out.extend_from_slice(b"data");
    let data_field = match form {
        Form::Riff => data_len as u32,
        Form::Rf64 => SIZE_IS_IN_DS64,
    };
    out.extend_from_slice(&data_field.to_le_bytes());
    out
}

fn supported_bits(spec: WavSpec) -> io::Result<()> {
    let supported = match spec.sample_format {
        SampleFormat::Int => [8, 16, 24, 32].contains(&spec.bits_per_sample),
        SampleFormat::Float => spec.bits_per_sample == 32,
    };
    if spec.channels == 0 || !supported {
        return Err(invalid(format!(
            "cannot write {} channels of {}-bit {:?}",
            spec.channels, spec.bits_per_sample, spec.sample_format
        )));
    }
    Ok(())
}

fn byte_rate_fields(out: &mut Vec<u8>, spec: WavSpec) {
    let block_align = spec.channels * spec.bits_per_sample.div_ceil(8);
    out.extend_from_slice(&spec.channels.to_le_bytes());
    out.extend_from_slice(&spec.sample_rate.to_le_bytes());
    out.extend_from_slice(&(spec.sample_rate * block_align as u32).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
}

// the fmt chunk hound writes: WAVE_FORMAT_EXTENSIBLE past two channels or 16 bits
fn extensible_when_wide_fmt_chunk(spec: WavSpec) -> Vec<u8> {
    let float = spec.sample_format == SampleFormat::Float;
    let mut out = b"fmt ".to_vec();
    if spec.channels <= 2 && spec.bits_per_sample <= 16 {
        out.extend_from_slice(&PLAIN_FMT_BYTES.to_le_bytes());
        let tag = if float {
            WAVE_FORMAT_IEEE_FLOAT
        } else {
            WAVE_FORMAT_PCM
        };
        out.extend_from_slice(&tag.to_le_bytes());
        byte_rate_fields(&mut out, spec);
        out.extend_from_slice(&spec.bits_per_sample.to_le_bytes());
        return out;
    }
    out.extend_from_slice(&EXTENSIBLE_FMT_BYTES.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
    byte_rate_fields(&mut out, spec);
    out.extend_from_slice(&(spec.bits_per_sample.div_ceil(8) * 8).to_le_bytes());
    out.extend_from_slice(&EXTENSIBLE_EXTRA_BYTES.to_le_bytes());
    out.extend_from_slice(&spec.bits_per_sample.to_le_bytes());
    let named = spec.channels.min(NAMED_SPEAKER_POSITIONS) as u32;
    let channel_mask = ((1u64 << named) - 1) as u32;
    out.extend_from_slice(&channel_mask.to_le_bytes());
    let subformat = if float {
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        KSDATAFORMAT_SUBTYPE_PCM
    };
    out.extend_from_slice(&subformat);
    out
}

fn plain_pcm_fmt_chunk(spec: WavSpec) -> Vec<u8> {
    let mut out = b"fmt ".to_vec();
    out.extend_from_slice(&PLAIN_FMT_BYTES.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    byte_rate_fields(&mut out, spec);
    out.extend_from_slice(&spec.bits_per_sample.to_le_bytes());
    out
}

// RIFF while the expected frames fit its 32-bit sizes, RF64 past that
pub struct WavWriter {
    sink: BufWriter<File>,
    spec: WavSpec,
    bytes_per_sample: u16,
    form: Form,
    chunks: Vec<u8>,
    data_len: u64,
}

impl WavWriter {
    // the header hound wrote
    pub fn create(path: &Path, spec: WavSpec, expected_frames: u64) -> io::Result<Self> {
        supported_bits(spec)?;
        let chunks = extensible_when_wide_fmt_chunk(spec);
        let bytes_per_sample = spec.bits_per_sample.div_ceil(8);
        Self::new(path, spec, bytes_per_sample, chunks, expected_frames)
    }

    // a 16-byte PCM fmt chunk at any channel count
    pub fn create_plain_pcm(path: &Path, spec: WavSpec, expected_frames: u64) -> io::Result<Self> {
        supported_bits(spec)?;
        if spec.sample_format != SampleFormat::Int {
            return Err(invalid(
                "a plain PCM fmt chunk cannot describe float".into(),
            ));
        }
        let chunks = plain_pcm_fmt_chunk(spec);
        let bytes_per_sample = spec.bits_per_sample.div_ceil(8);
        Self::new(path, spec, bytes_per_sample, chunks, expected_frames)
    }

    // every chunk `source` has before its data, ds64 aside, copied byte for byte
    pub fn create_like(
        path: &Path,
        source_path: &Path,
        source: &WavLayout,
        expected_frames: u64,
    ) -> io::Result<Self> {
        let mut file = File::open(source_path)?;
        let mut chunks = Vec::new();
        for chunk in source
            .chunks
            .iter()
            .filter(|chunk| chunk.offset < source.data.offset && &chunk.id != b"ds64")
        {
            let mut bytes = vec![0u8; (chunk.end() - chunk.offset) as usize];
            file.seek(SeekFrom::Start(chunk.offset))?;
            file.read_exact(&mut bytes)?;
            chunks.extend_from_slice(&bytes);
        }
        Self::new(
            path,
            source.spec,
            source.bytes_per_sample,
            chunks,
            expected_frames,
        )
    }

    fn new(
        path: &Path,
        spec: WavSpec,
        bytes_per_sample: u16,
        chunks: Vec<u8>,
        expected_frames: u64,
    ) -> io::Result<Self> {
        let block_align = bytes_per_sample as u64 * spec.channels as u64;
        let expected_len = expected_frames * block_align;
        let form = if riff_size(Form::Riff, chunks.len() as u64, expected_len) > u32::MAX as u64 {
            Form::Rf64
        } else {
            Form::Riff
        };
        let mut sink = BufWriter::with_capacity(IO_BUFFER_BYTES, File::create(path)?);
        sink.write_all(&header(form, &chunks, 0, 0))?;
        Ok(Self {
            sink,
            spec,
            bytes_per_sample,
            form,
            chunks,
            data_len: 0,
        })
    }

    pub fn spec(&self) -> WavSpec {
        self.spec
    }

    pub fn write_sample<S: Sample>(&mut self, sample: S) -> io::Result<()> {
        let mut bytes = [0u8; 4];
        let bytes = &mut bytes[..self.bytes_per_sample as usize];
        sample.encode(self.spec.sample_format, self.spec.bits_per_sample, bytes)?;
        self.write_bytes(bytes)
    }

    // samples already laid out the way the file stores them
    pub fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.sink.write_all(bytes)?;
        self.data_len += bytes.len() as u64;
        Ok(())
    }

    pub fn finalize(mut self) -> io::Result<()> {
        let block_align = self.bytes_per_sample as u64 * self.spec.channels as u64;
        if !self.data_len.is_multiple_of(block_align) {
            return Err(invalid(format!(
                "{} bytes of samples do not make whole {block_align}-byte frames",
                self.data_len
            )));
        }
        let size = riff_size(self.form, self.chunks.len() as u64, self.data_len);
        if self.form == Form::Riff && size > u32::MAX as u64 {
            return Err(invalid(format!(
                "wrote {} bytes of samples into a RIFF header sized for less than 4 GiB",
                self.data_len
            )));
        }
        let header = header(
            self.form,
            &self.chunks,
            self.data_len,
            self.data_len / block_align,
        );
        self.sink.seek(SeekFrom::Start(0))?;
        self.sink.write_all(&header)?;
        self.sink.flush()
    }
}

/// Interleaved samples in the file's own sample type.
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    Int(Vec<i32>),
    Float(Vec<f32>),
}

impl Samples {
    pub fn len(&self) -> usize {
        match self {
            Samples::Int(v) => v.len(),
            Samples::Float(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How many channels a WAV carries, without reading its samples.
pub fn channel_count(path: &Path) -> Result<usize, String> {
    let layout = WavLayout::read(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(layout.spec.channels as usize)
}

/// Read a WAV into (spec, interleaved samples of the file's own type). Every
/// sample survives, so writing the result back reproduces the file byte for
/// byte. Use this for anything that moves samples around rather than doing
/// arithmetic on them.
pub fn read_interleaved_exact(path: &Path) -> io::Result<(WavSpec, Samples)> {
    let reader = WavReader::open(path)?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        SampleFormat::Int => Samples::Int(reader.into_samples::<i32>().collect::<Result<_, _>>()?),
        SampleFormat::Float => {
            Samples::Float(reader.into_samples::<f32>().collect::<Result<_, _>>()?)
        }
    };
    Ok((spec, samples))
}

/// Write interleaved samples of the file's own type. `samples` must match
/// `spec`'s sample format; the writer rejects the mismatch.
pub fn write_interleaved_exact(path: &Path, spec: WavSpec, samples: &Samples) -> io::Result<()> {
    let frames = samples.len() as u64 / spec.channels as u64;
    let mut w = WavWriter::create(path, spec, frames)?;
    match samples {
        Samples::Int(v) => {
            for &s in v {
                w.write_sample(s)?;
            }
        }
        Samples::Float(v) => {
            for &s in v {
                w.write_sample(s)?;
            }
        }
    }
    w.finalize()?;
    Ok(())
}

/// Read a WAV into (spec, interleaved f32 in -1.0..=1.0). Int is scaled by
/// 2^(bits-1); float passes through. 32-bit int loses its low bits here, so
/// anything that must stay bit-exact wants `read_interleaved_exact`.
pub fn read_interleaved(path: &Path) -> io::Result<(WavSpec, Vec<f32>)> {
    let reader = WavReader::open(path)?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        SampleFormat::Int => {
            let fs = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| v as f32 / fs))
                .collect::<Result<_, _>>()?
        }
        SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<_, _>>()?,
    };
    Ok((spec, samples))
}

/// Write interleaved f32 back to WAV in `spec`'s format. Int is scaled by
/// 2^(bits-1) and clamped: dsp can overshoot full scale, and wrapping a
/// narrower int would flip sign.
pub fn write_interleaved(path: &Path, spec: WavSpec, samples: &[f32]) -> io::Result<()> {
    let frames = samples.len() as u64 / spec.channels as u64;
    let mut w = WavWriter::create(path, spec, frames)?;
    match spec.sample_format {
        SampleFormat::Int => {
            let fs = (1i64 << (spec.bits_per_sample - 1)) as f64;
            let max = (fs as i64) - 1;
            let min = -(fs as i64);
            for &v in samples {
                let x = (v as f64 * fs).round() as i64;
                w.write_sample(x.clamp(min, max) as i32)?;
            }
        }
        SampleFormat::Float => {
            for &v in samples {
                w.write_sample(v)?;
            }
        }
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn spec(bits: u16, format: SampleFormat) -> WavSpec {
        WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: bits,
            sample_format: format,
        }
    }

    /// Values that only survive if every bit of the sample does: the extremes,
    /// the ends of the range and a pattern with bits set right down to the LSB.
    fn awkward_ints(bits: u16) -> Vec<i32> {
        let top = 1i64 << (bits - 1);
        let mut v = vec![0, 1, -1, top - 1, -top, top / 3, -(top / 3) - 1];
        for shift in 0..bits {
            v.push((top - 1) >> shift);
            v.push(-top + (1 << shift));
        }
        if !v.len().is_multiple_of(2) {
            v.push(0);
        }
        v.into_iter().map(|s| s as i32).collect()
    }

    #[test]
    fn exact_round_trip_is_byte_identical_for_every_format() {
        let dir = TempDir::new().unwrap();
        let cases = [
            (spec(8, SampleFormat::Int), Samples::Int(awkward_ints(8))),
            (spec(16, SampleFormat::Int), Samples::Int(awkward_ints(16))),
            (spec(24, SampleFormat::Int), Samples::Int(awkward_ints(24))),
            (spec(32, SampleFormat::Int), Samples::Int(awkward_ints(32))),
            (
                spec(32, SampleFormat::Float),
                Samples::Float(vec![0.0, 1.0, -1.0, 1e-9, -0.333_333_34, 0.999_999_94]),
            ),
        ];
        for (spec, samples) in cases {
            let first = dir.path().join(format!("{}.wav", spec.bits_per_sample));
            write_interleaved_exact(&first, spec, &samples).unwrap();
            let (read_spec, read_samples) = read_interleaved_exact(&first).unwrap();
            assert_eq!(read_spec, spec);
            assert_eq!(
                read_samples, samples,
                "{:?} {} bit lost samples",
                spec.sample_format, spec.bits_per_sample
            );

            let second = dir
                .path()
                .join(format!("{}-again.wav", spec.bits_per_sample));
            write_interleaved_exact(&second, read_spec, &read_samples).unwrap();
            assert_eq!(
                std::fs::read(&first).unwrap(),
                std::fs::read(&second).unwrap(),
                "{:?} {} bit round-trip changed the file",
                spec.sample_format,
                spec.bits_per_sample
            );
        }
    }

    #[test]
    fn prepending_silence_keeps_the_rest_bit_exact() {
        let dir = TempDir::new().unwrap();
        for bits in [16, 24, 32] {
            let original = awkward_ints(bits);
            let source = dir.path().join(format!("source{bits}.wav"));
            let spec = spec(bits, SampleFormat::Int);
            write_interleaved_exact(&source, spec, &Samples::Int(original.clone())).unwrap();

            let (spec, samples) = read_interleaved_exact(&source).unwrap();
            let Samples::Int(samples) = samples else {
                panic!("int wav read back as float");
            };
            let mut delayed = vec![0i32; 96];
            delayed.extend_from_slice(&samples);
            let shifted = dir.path().join(format!("shifted{bits}.wav"));
            write_interleaved_exact(&shifted, spec, &Samples::Int(delayed)).unwrap();

            let (_, back) = read_interleaved_exact(&shifted).unwrap();
            let Samples::Int(back) = back else {
                panic!("int wav read back as float");
            };
            assert!(
                back[..96].iter().all(|&s| s == 0),
                "{bits} bit lost silence"
            );
            assert_eq!(&back[96..], &original[..], "{bits} bit lost samples");
        }
    }

    #[test]
    fn the_normalised_pair_holds_up_to_24_bit() {
        let dir = TempDir::new().unwrap();
        for bits in [16, 24] {
            let original = awkward_ints(bits);
            let path = dir.path().join(format!("norm{bits}.wav"));
            let spec = spec(bits, SampleFormat::Int);
            write_interleaved_exact(&path, spec, &Samples::Int(original.clone())).unwrap();

            let (spec, samples) = read_interleaved(&path).unwrap();
            let again = dir.path().join(format!("norm{bits}-again.wav"));
            write_interleaved(&again, spec, &samples).unwrap();

            let (_, back) = read_interleaved_exact(&again).unwrap();
            assert_eq!(back, Samples::Int(original), "{bits} bit lost samples");
        }
    }

    #[test]
    fn writing_the_wrong_sample_type_fails_loud() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mismatch.wav");
        let error = write_interleaved_exact(
            &path,
            spec(24, SampleFormat::Int),
            &Samples::Float(vec![0.0, 0.0]),
        );
        assert!(error.is_err(), "float samples went into an int wav");
    }

    const HEAD_LIMIT_BYTES: u64 = 1 << 16;

    // the whole of a small test file, refusing anything bigger
    fn small_file_bytes(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(path)
            .unwrap()
            .take(HEAD_LIMIT_BYTES + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(
            bytes.len() as u64 <= HEAD_LIMIT_BYTES,
            "test file is too big"
        );
        bytes
    }

    fn form_type(path: &Path) -> [u8; 4] {
        let mut form = [0u8; 4];
        File::open(path).unwrap().read_exact(&mut form).unwrap();
        form
    }

    fn channel_spec(channels: u16, bits: u16, sample_format: SampleFormat) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: bits,
            sample_format,
        }
    }

    fn ffmpeg_wav(path: &Path, rf64: &str, codec: &str, channels: u32) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("sine=frequency=997:sample_rate=48000:duration=0.25")
            .args(["-ac", &channels.to_string(), "-c:a", codec, "-rf64", rf64])
            .arg(path)
            .status()
            .expect("ffmpeg has to run");
        assert!(
            status.success(),
            "ffmpeg could not write {}",
            path.display()
        );
    }

    #[test]
    fn a_small_file_is_byte_identical_to_what_hound_wrote() {
        let dir = TempDir::new().unwrap();
        let cases = [
            channel_spec(1, 8, SampleFormat::Int),
            channel_spec(2, 16, SampleFormat::Int),
            channel_spec(2, 24, SampleFormat::Int),
            channel_spec(6, 24, SampleFormat::Int),
            channel_spec(16, 24, SampleFormat::Int),
            channel_spec(2, 32, SampleFormat::Int),
            channel_spec(2, 32, SampleFormat::Float),
            channel_spec(6, 32, SampleFormat::Float),
        ];
        for spec in cases {
            let frames = 97u64;
            let ours = dir.path().join("ours.wav");
            let theirs = dir.path().join("theirs.wav");
            let mut writer = WavWriter::create(&ours, spec, frames).unwrap();
            let mut reference = hound::WavWriter::create(&theirs, spec).unwrap();
            for index in 0..frames as i32 * spec.channels as i32 {
                let value = (index * 37) % 101 - 50;
                match spec.sample_format {
                    SampleFormat::Int => {
                        writer.write_sample(value).unwrap();
                        reference.write_sample(value).unwrap();
                    }
                    SampleFormat::Float => {
                        writer.write_sample(value as f32 / 64.0).unwrap();
                        reference.write_sample(value as f32 / 64.0).unwrap();
                    }
                }
            }
            writer.finalize().unwrap();
            reference.finalize().unwrap();
            assert_eq!(
                small_file_bytes(&ours),
                small_file_bytes(&theirs),
                "{spec:?} differs from hound"
            );
        }
    }

    #[test]
    fn an_rf64_file_reads_the_same_samples_as_its_riff_twin() {
        let dir = TempDir::new().unwrap();
        for codec in ["pcm_s16le", "pcm_s24le", "pcm_f32le"] {
            let riff = dir.path().join("riff.wav");
            let rf64 = dir.path().join("rf64.wav");
            ffmpeg_wav(&riff, "never", codec, 6);
            ffmpeg_wav(&rf64, "always", codec, 6);
            assert_eq!(&form_type(&rf64), b"RF64", "{codec}");

            let riff_reader = WavReader::open(&riff).unwrap();
            let rf64_reader = WavReader::open(&rf64).unwrap();
            assert_eq!(rf64_reader.spec(), riff_reader.spec(), "{codec}");
            assert_eq!(rf64_reader.duration(), 12_000, "{codec}");
            match riff_reader.spec().sample_format {
                SampleFormat::Int => {
                    let expected: Vec<i32> = riff_reader
                        .into_samples()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    let got: Vec<i32> = rf64_reader
                        .into_samples()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    assert!(expected.iter().any(|&s| s != 0), "{codec} tone is silent");
                    assert_eq!(got, expected, "{codec}");
                }
                SampleFormat::Float => {
                    let expected: Vec<f32> = riff_reader
                        .into_samples()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    let got: Vec<f32> = rf64_reader
                        .into_samples()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    assert_eq!(got, expected, "{codec}");
                }
            }
        }
    }

    #[test]
    fn a_payload_past_4_gib_gets_an_rf64_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("long.wav");
        // 16 channels of 24-bit at 48 kHz over 88.8 minutes
        let spec = channel_spec(16, 24, SampleFormat::Int);
        let frames = 127_859 * 2_000u64;
        let block_align = 16 * 3u64;
        let data_len = frames * block_align;
        assert!(data_len > u32::MAX as u64);

        let mut writer = WavWriter::create(&path, spec, frames).unwrap();
        for channel in 0..16 {
            writer.write_sample(channel * 1000 - 7000).unwrap();
        }
        // the rest of the payload stays unwritten, set_len makes it sparse
        writer.data_len = data_len;
        writer.finalize().unwrap();
        let header_len = small_file_bytes(&path).len() as u64 - block_align;
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(header_len + data_len)
            .unwrap();

        let mut head = vec![0u8; header_len as usize];
        File::open(&path).unwrap().read_exact(&mut head).unwrap();
        assert_eq!(&head[0..4], b"RF64");
        assert_eq!(&head[4..8], &SIZE_IS_IN_DS64.to_le_bytes());
        assert_eq!(&head[12..16], b"ds64");
        let ds64_u64 = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().unwrap());
        assert_eq!(ds64_u64(20), header_len + data_len - 8, "riff size");
        assert_eq!(ds64_u64(28), data_len, "data size");
        assert_eq!(ds64_u64(36), frames, "sample count");
        assert_eq!(&head[header_len as usize - 8..][..4], b"data");
        assert_eq!(
            &head[header_len as usize - 4..],
            &SIZE_IS_IN_DS64.to_le_bytes()
        );

        let mut reader = WavReader::open(&path).unwrap();
        assert_eq!(reader.spec(), spec);
        assert_eq!(reader.duration(), frames);
        let first_frame: Vec<i32> = reader.samples().take(16).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            first_frame,
            (0..16).map(|c| c * 1000 - 7000).collect::<Vec<_>>()
        );

        // ffprobe reads only the header
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=channels,duration_ts",
            ])
            .args(["-of", "csv=p=0"])
            .arg(&path)
            .output()
            .expect("ffprobe has to run");
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&probe.stdout).trim(),
            format!("16,{frames}")
        );
    }

    #[test]
    fn writing_past_what_a_riff_header_holds_fails_loud() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lied.wav");
        let mut writer =
            WavWriter::create(&path, channel_spec(2, 16, SampleFormat::Int), 10).unwrap();
        writer.data_len = u32::MAX as u64;
        assert!(writer.finalize().is_err());
    }

    #[test]
    fn a_copied_header_drops_ds64_and_keeps_the_other_chunks() {
        let dir = TempDir::new().unwrap();
        let rf64 = dir.path().join("rf64.wav");
        ffmpeg_wav(&rf64, "always", "pcm_s24le", 2);
        let layout = WavLayout::read(&rf64).unwrap();
        let copy = dir.path().join("copy.wav");
        let mut writer = WavWriter::create_like(&copy, &rf64, &layout, 1).unwrap();
        writer.write_bytes(&[1, 2, 3, 4, 5, 6]).unwrap();
        writer.finalize().unwrap();

        let copied = WavLayout::read(&copy).unwrap();
        assert_eq!(&form_type(&copy), b"RIFF");
        assert_eq!(copied.spec, layout.spec);
        assert_eq!(copied.frames(), 1);
        let ids = |layout: &WavLayout| -> Vec<[u8; 4]> {
            layout
                .chunks
                .iter()
                .map(|chunk| chunk.id)
                .filter(|id| id != b"ds64")
                .collect()
        };
        assert_eq!(ids(&copied), ids(&layout));
    }

    #[test]
    fn a_chunk_running_past_the_end_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("short.wav");
        let mut writer =
            WavWriter::create(&path, channel_spec(2, 16, SampleFormat::Int), 4).unwrap();
        for _ in 0..8 {
            writer.write_sample(0i32).unwrap();
        }
        writer.finalize().unwrap();
        let full = small_file_bytes(&path);
        std::fs::write(&path, &full[..full.len() - 2]).unwrap();
        let error = WavLayout::read(&path).unwrap_err().to_string();
        assert!(error.contains("past the end"), "{error}");
    }
}
