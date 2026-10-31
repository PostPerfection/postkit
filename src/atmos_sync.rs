const ATMOS_SYNC_CHANNEL_INDEX: usize = 13;
const ATMOS_SYNC_SAMPLE_RATE: u32 = 48_000;
const ATMOS_SYNC_FEWEST_CHANNELS: u32 = 14;
const BYTES_PER_SAMPLE: usize = 3;
const SAMPLES_PER_BIT: usize = 4;
const PACKET_SYNC_WORD: [u8; 2] = [0x4d, 0x56];
const PACKET_CRC_POLYNOMIAL: u16 = 0x1021;
const CHECKED_BITS: usize = 64;
const PACKET_BITS: usize = 16 + CHECKED_BITS + 16 + 4;

struct SyncFrameLayout {
    edit_rate_code: u8,
    packets_per_frame: usize,
    padding_bits: usize,
}

fn sync_frame_layout(frames_per_second: u32) -> Result<SyncFrameLayout, String> {
    let (edit_rate_code, packets_per_frame, padding_bits) = match frames_per_second {
        24 => (0, 4, 25),
        25 => (1, 4, 20),
        30 => (2, 4, 0),
        48 => (3, 2, 25),
        50 => (4, 2, 20),
        60 => (5, 2, 0),
        96 => (6, 1, 25),
        100 => (7, 1, 20),
        120 => (8, 1, 0),
        _ => {
            return Err(format!(
                "the Atmos sync signal has no code for {frames_per_second} fps: it covers 24, 25, \
                 30, 48, 50, 60, 96, 100 and 120"
            ));
        }
    };
    Ok(SyncFrameLayout {
        edit_rate_code,
        packets_per_frame,
        padding_bits,
    })
}

pub fn check_atmos_sync_sound(
    frames_per_second: u32,
    sample_rate: u32,
    channels: u32,
) -> Result<(), String> {
    sync_frame_layout(frames_per_second)?;
    if sample_rate != ATMOS_SYNC_SAMPLE_RATE {
        return Err(format!(
            "the Atmos sync signal needs {ATMOS_SYNC_SAMPLE_RATE} Hz sound, and this sound is \
             {sample_rate} Hz"
        ));
    }
    if channels < ATMOS_SYNC_FEWEST_CHANNELS {
        return Err(format!(
            "the Atmos sync signal goes on channel {ATMOS_SYNC_FEWEST_CHANNELS}, and this sound \
             has {channels} channels"
        ));
    }
    Ok(())
}

pub(crate) fn crc16(bits: &[bool]) -> u16 {
    let mut remainder: u16 = 0;
    for &bit in bits {
        let divide = ((remainder & 0x8000) != 0) ^ bit;
        remainder <<= 1;
        if divide {
            remainder ^= PACKET_CRC_POLYNOMIAL;
        }
    }
    remainder
}

fn push_bits(bits: &mut Vec<bool>, value: u32, count: usize) {
    for shift in (0..count).rev() {
        bits.push((value >> shift) & 1 == 1);
    }
}

// the packets and the waveform match libdcp's, whose tests compare them with a reference Atmos sound track
pub struct AtmosSyncSignal {
    layout: SyncFrameLayout,
    sound_asset_uuid: [u8; 16],
    channels: usize,
    frame_index: u32,
    packet_counter: u8,
    positive_polarity: bool,
    previous_bit: bool,
}

impl AtmosSyncSignal {
    pub fn new(
        sound_asset_uuid: [u8; 16],
        frames_per_second: u32,
        sample_rate: u32,
        channels: u32,
    ) -> Result<Self, String> {
        check_atmos_sync_sound(frames_per_second, sample_rate, channels)?;
        Ok(Self {
            layout: sync_frame_layout(frames_per_second)?,
            sound_asset_uuid,
            channels: channels as usize,
            frame_index: 0,
            packet_counter: 0,
            positive_polarity: false,
            previous_bit: false,
        })
    }

    fn next_frame_bits(&mut self) -> Vec<bool> {
        let mut bits = Vec::new();
        for packet in 0..self.layout.packets_per_frame {
            for byte in PACKET_SYNC_WORD {
                push_bits(&mut bits, u32::from(byte), 8);
            }
            let checked_from = bits.len();
            push_bits(&mut bits, u32::from(self.layout.edit_rate_code), 4);
            push_bits(&mut bits, 0, 2);
            push_bits(&mut bits, u32::from(self.packet_counter), 2);
            // libdcp takes the asset id quarter from the packet's place in the frame, not its counter
            for &byte in &self.sound_asset_uuid[packet * 4..packet * 4 + 4] {
                push_bits(&mut bits, u32::from(byte), 8);
            }
            push_bits(&mut bits, self.frame_index, 24);
            let crc = crc16(&bits[checked_from..]);
            push_bits(&mut bits, u32::from(crc), 16);
            push_bits(&mut bits, 0, 4);
            bits.resize(bits.len() + self.layout.padding_bits, false);
            self.packet_counter = (self.packet_counter + 1) % 4;
        }
        self.frame_index += 1;
        bits
    }

    // frames go in order from the first frame of the track file
    pub fn write_frame(&mut self, frame: &mut [u8]) {
        let bits = self.next_frame_bits();
        let sample_stride = self.channels * BYTES_PER_SAMPLE;
        assert_eq!(
            frame.len(),
            bits.len() * SAMPLES_PER_BIT * sample_stride,
            "a sound frame holds one edit unit at the sync signal's sample rate"
        );
        let mut samples = frame.chunks_exact_mut(sample_stride).map(|sample| {
            &mut sample[ATMOS_SYNC_CHANNEL_INDEX * BYTES_PER_SAMPLE..][..BYTES_PER_SAMPLE]
        });
        for bit in bits {
            if !self.previous_bit {
                self.positive_polarity = !self.positive_polarity;
            }
            for position in 0..SAMPLES_PER_BIT {
                let magnitude = fsk_sample(position, bit);
                let value = if self.positive_polarity {
                    magnitude
                } else {
                    -magnitude
                };
                let out = samples
                    .next()
                    .expect("the length check above counts these samples");
                out.copy_from_slice(&value.to_le_bytes()[..BYTES_PER_SAMPLE]);
            }
            self.previous_bit = bit;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtmosSyncPacket {
    pub counter: u8,
    pub sound_asset_uuid_quarter: [u8; 4],
    pub frame_index: u32,
}

fn read_bits(bits: &mut &[bool], count: usize) -> u32 {
    let (head, tail) = bits.split_at(count);
    *bits = tail;
    head.iter()
        .fold(0, |value, &bit| (value << 1) | u32::from(bit))
}

pub fn decode_atmos_sync_frame(
    frame: &[u8],
    channels: usize,
    frames_per_second: u32,
) -> Result<Vec<AtmosSyncPacket>, String> {
    let layout = sync_frame_layout(frames_per_second)?;
    let sample_stride = channels * BYTES_PER_SAMPLE;
    let samples: Vec<i32> = frame
        .chunks_exact(sample_stride)
        .map(|sample| {
            let at = ATMOS_SYNC_CHANNEL_INDEX * BYTES_PER_SAMPLE;
            i32::from_le_bytes([0, sample[at], sample[at + 1], sample[at + 2]]) >> 8
        })
        .collect();
    let (bit_samples, rest) = samples.as_chunks::<SAMPLES_PER_BIT>();
    if !rest.is_empty() {
        return Err("channel 14 does not hold a whole number of sync bits".into());
    }
    let quietest = fsk_sample(0, false) / 2;
    let mut bits = Vec::with_capacity(bit_samples.len());
    for (index, bit) in bit_samples.iter().enumerate() {
        if bit.iter().any(|sample| sample.abs() < quietest) {
            return Err(format!("channel 14 carries no sync signal at bit {index}"));
        }
        bits.push((bit[0] > 0) != (bit[2] > 0));
    }

    let mut rest = bits.as_slice();
    let mut packets = Vec::with_capacity(layout.packets_per_frame);
    for packet in 0..layout.packets_per_frame {
        if rest.len() < PACKET_BITS + layout.padding_bits {
            return Err(format!("channel 14 ends inside packet {packet}"));
        }
        if read_bits(&mut rest, 16) != u32::from(u16::from_be_bytes(PACKET_SYNC_WORD)) {
            return Err(format!("packet {packet} does not start with the sync word"));
        }
        let checked = &rest[..CHECKED_BITS];
        let edit_rate_code = read_bits(&mut rest, 4);
        read_bits(&mut rest, 2);
        let counter = read_bits(&mut rest, 2) as u8;
        let sound_asset_uuid_quarter = read_bits(&mut rest, 32).to_be_bytes();
        let frame_index = read_bits(&mut rest, 24);
        if read_bits(&mut rest, 16) != u32::from(crc16(checked)) {
            return Err(format!("packet {packet} fails its CRC"));
        }
        if edit_rate_code != u32::from(layout.edit_rate_code) {
            return Err(format!(
                "packet {packet} carries edit rate code {edit_rate_code}, not the one for {frames_per_second} fps"
            ));
        }
        read_bits(&mut rest, 4 + layout.padding_bits);
        packets.push(AtmosSyncPacket {
            counter,
            sound_asset_uuid_quarter,
            frame_index,
        });
    }
    Ok(packets)
}

// a 0 is half a sine cycle over the bit's four samples, a 1 a whole one
fn fsk_sample(position: usize, bit: bool) -> i32 {
    const FULL_SCALE: f64 = 8_388_608.0;
    const ZERO: [f64; SAMPLES_PER_BIT] = [0.03827, 0.09239, 0.09239, 0.03827];
    const ONE: [f64; SAMPLES_PER_BIT] = [0.07071, 0.07071, -0.07071, -0.07071];
    let level = if bit { ONE[position] } else { ZERO[position] };
    (level * FULL_SCALE) as i32
}

#[cfg(test)]
mod tests;
