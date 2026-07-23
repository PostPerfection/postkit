//! PAC binary subtitle parser (dom#1719).
//!
//! Format and Latin codepage byte mapping ported from Subtitle Edit's reference
//! implementation: src/libse/SubtitleFormats/Pac.cs
//! (github.com/SubtitleEdit/subtitleedit). Latin codepage only: timecodes,
//! text blocks, per-line italics. Non-Latin codepages fail loud. Timecodes use
//! the 25 fps PAC/EBU frame rate.

use super::{HAlign, StyledCue, StyledRun, SubtitleError, VAlign};

/// PAC codepage id; only Latin (0) is implemented.
pub const CODEPAGE_LATIN: u8 = 0;

/// Parse PAC bytes using the given codepage. Only Latin (0) is supported.
pub fn parse_pac(buffer: &[u8], codepage: u8) -> Result<Vec<StyledCue>, SubtitleError> {
    if codepage != CODEPAGE_LATIN {
        return Err(SubtitleError::UnsupportedCodepage(codepage));
    }
    if buffer.len() < 24 || buffer[0] != 1 {
        return Err(SubtitleError::Parse("not a PAC file (bad header)".into()));
    }

    let mut cues = Vec::new();
    let mut index = 0usize;
    while index < buffer.len() {
        match next_paragraph(buffer, &mut index) {
            Some(cue) => cues.push(cue),
            None => break,
        }
    }
    if cues.is_empty() {
        return Err(SubtitleError::Parse("no PAC subtitles found".into()));
    }
    Ok(cues)
}

fn next_paragraph(buffer: &[u8], index: &mut usize) -> Option<StyledCue> {
    // locate the next 0xFE record marker, preceded by a 0x60..0x67 spot byte
    loop {
        *index += 1;
        if *index + 20 >= buffer.len() {
            return None;
        }
        if buffer[*index] == 0xFE {
            let m15 = buffer[*index - 15];
            let m12 = buffer[*index - 12];
            if (0x60..=0x67).contains(&m15) || (0x60..=0x67).contains(&m12) {
                break;
            }
        }
    }
    let fe = *index;
    let raw_align = buffer[fe + 1];
    let align_bits = raw_align & 0x03; // 0=right, 1=left, 2=center
    let first_italic = (raw_align & 0x04) != 0;

    // resolve timecode block position (spot byte at ts, or ts+3)
    let mut ts = fe - 15;
    if buffer[ts] == 0x60 || (0x61..=0x67).contains(&buffer[ts]) {
        // ts is correct
    } else if buffer[ts + 3] == 0x60 || (0x61..=0x67).contains(&buffer[ts + 3]) {
        ts += 3;
    } else {
        return None;
    }
    let start_ms = get_tc_ms(buffer, ts + 1);
    let end_ms = get_tc_ms(buffer, ts + 5);

    let text_length = buffer[ts + 9] as usize + buffer[ts + 10] as usize * 256;
    if text_length > 500 {
        return None;
    }
    let max_index = (ts + 10 + text_length).min(buffer.len().saturating_sub(1));
    let vertical_alignment = buffer[ts + 11];

    // extract Latin text; 0xFE breaks lines, its next byte carries line italic
    let mut lines: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut cur_italic = first_italic;
    let mut i = fe + 3; // skip FE, alignment byte, 0x03 constant
    while i <= max_index && i < buffer.len() {
        let b = buffer[i];
        if b == 0xFE {
            lines.push((std::mem::take(&mut cur), cur_italic));
            let ra = buffer.get(i + 1).copied().unwrap_or(0);
            cur_italic = (ra & 0x04) != 0;
            i += 3;
            continue;
        }
        if b == 0xFF {
            cur.push(' ');
            i += 1;
            continue;
        }
        // colour switch and other control bytes are ignored
        if (0x01..0x08).contains(&b) || matches!(b, 0x00 | 0x0b | 0x0d | 0x17 | 0x1d) {
            i += 1;
            continue;
        }
        let (s, consumed) = latin_char(buffer, i);
        cur.push_str(&s);
        i += consumed;
    }
    lines.push((cur, cur_italic));

    *index = max_index;

    let mut runs = Vec::new();
    for (li, (text, ital)) in lines.iter().enumerate() {
        let mut t = text.clone();
        if li + 1 < lines.len() {
            t.push('\n');
        }
        runs.push(StyledRun {
            text: t,
            italic: *ital,
            bold: false,
            underline: false,
            color: None,
        });
    }

    let align = match align_bits {
        1 => HAlign::Left,
        0 => HAlign::Right,
        _ => HAlign::Center,
    };
    let valign = if vertical_alignment < 5 {
        VAlign::Top
    } else if vertical_alignment < 9 {
        VAlign::Middle
    } else {
        VAlign::Bottom
    };
    let vposition = if vertical_alignment >= 2 {
        Some(vertical_alignment as f32 * 100.0 / 12.0)
    } else {
        None
    };

    Some(StyledCue {
        start_ms,
        end_ms,
        runs,
        align: Some(align),
        valign: Some(valign),
        vposition,
        image: None,
    })
}

/// Decode one PAC timecode (4 bytes) to milliseconds. Layout: HHMM then SSFF,
/// each a little-endian decimal u16; frames are 25 fps.
fn get_tc_ms(buffer: &[u8], i: usize) -> u64 {
    let high = buffer[i] as u64 + buffer[i + 1] as u64 * 256;
    let low = buffer[i + 2] as u64 + buffer[i + 3] as u64 * 256;
    let hours = high / 100;
    let minutes = high % 100;
    let seconds = low / 100;
    let frames = low % 100;
    let frame_ms = (frames * 1000 / 25).min(999);
    (hours * 3600 + minutes * 60 + seconds) * 1000 + frame_ms
}

/// Decode one Latin character at `i`, returning (string, bytes consumed).
///
/// Mirrors Subtitle Edit's TryGetMappedCharacter: a two-byte mapping wins when
/// present and the following two-byte window is absent or lower priority;
/// otherwise the single-byte mapping, else ISO-8859-1, else skip.
fn latin_char(buffer: &[u8], i: usize) -> (String, usize) {
    let b = buffer[i] as u32;
    if i + 2 < buffer.len() {
        let code = b * 256 + buffer[i + 1] as u32;
        let next = buffer[i + 1] as u32 * 256 + buffer[i + 2] as u32;
        if let Some((ch, prio)) = latin_lookup(code) {
            let next_prio = latin_lookup(next).map(|(_, p)| p);
            if next_prio.is_none() || prio > next_prio.unwrap() {
                return (ch.to_string(), 2);
            }
        }
    }
    if let Some((ch, _)) = latin_lookup(b) {
        return (ch.to_string(), 1);
    }
    if b > 13 {
        // ISO-8859-1: byte value is the Unicode code point
        return (char::from(b as u8).to_string(), 1);
    }
    (String::new(), 1)
}

fn latin_lookup(code: u32) -> Option<(&'static str, i32)> {
    LATIN_CODES
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, s, p)| (*s, *p))
}

/// (code, char, priority) from Subtitle Edit's LatinCodes dictionary.
static LATIN_CODES: &[(u32, &str, i32)] = &[
    (0xE041, "Ã", 2),
    (0xE04E, "Ñ", 2),
    (0xE04F, "Õ", 2),
    (0xE061, "ã", 2),
    (0xE06E, "ñ", 2),
    (0xE06F, "õ", 2),
    (0xE161, "å", 2),
    (0xE141, "Å", 2),
    (0x618A, "ā", 2),
    (0x418A, "Ā", 2),
    (0x458A, "Ē", 2),
    (0x658A, "ē", 2),
    (0x498A, "Ī", 2),
    (0x698A, "ī", 2),
    (0x4F8A, "Ō", 2),
    (0x6F8A, "ō", 2),
    (0x558A, "Ū", 2),
    (0x758A, "ū", 2),
    (0x0020, " ", 2),
    (0x0009, " ", 2),
    (0x0023, "£", 2),
    (0x007C, "æ", 2),
    (0x007D, "ø", 2),
    (0x007E, "§", 2),
    (0x0080, "#", 2),
    (0x005C, "Æ", 2),
    (0x005D, "Ø", 2),
    (0x005E, "÷", 2),
    (0x002D, "-", 2),
    (0x005F, "–", 2),
    (0xE54F, "Ö", 2),
    (0xE56F, "ö", 2),
    (0xE541, "Ä", 2),
    (0xE561, "ä", 2),
    (0xE555, "Ü", 2),
    (0xE575, "ü", 2),
    (0x0081, "ß", 2),
    (0x0082, "²", 2),
    (0xE241, "Á", 2),
    (0xE249, "Í", 2),
    (0xE255, "Ú", 2),
    (0xE259, "Ý", 2),
    (0xE261, "á", 2),
    (0xE265, "é", 2),
    (0xE269, "í", 2),
    (0xE245, "É", 2),
    (0xE275, "ú", 2),
    (0xE279, "ý", 2),
    (0xE361, "à", 2),
    (0xE365, "è", 2),
    (0xE36F, "ò", 2),
    (0xE345, "È", 2),
    (0xE349, "Ì", 2),
    (0xE34F, "Ò", 2),
    (0xE369, "ì", 2),
    (0xE443, "Ĉ", 2),
    (0xE447, "Ĝ", 2),
    (0xE448, "Ĥ", 2),
    (0xE44A, "Ĵ", 2),
    (0xE453, "Ŝ", 2),
    (0xEA55, "Ǔ", 2),
    (0xE463, "ĉ", 2),
    (0xE467, "ĝ", 2),
    (0xE468, "ĥ", 2),
    (0xE46A, "ĵ", 2),
    (0xE473, "ŝ", 2),
    (0xEA75, "ǔ", 2),
    (0xE341, "À", 2),
    (0xE441, "Â", 2),
    (0xE461, "â", 2),
    (0xE643, "Ç", 2),
    (0xE663, "ç", 2),
    (0xE445, "Ê", 2),
    (0xE465, "ê", 2),
    (0xE545, "Ë", 2),
    (0xE565, "ë", 2),
    (0xE449, "Î", 2),
    (0xE469, "î", 2),
    (0xE549, "Ï", 2),
    (0xE569, "ï", 2),
    (0xE44F, "Ô", 2),
    (0xE46F, "ô", 2),
    (0xE355, "Ù", 2),
    (0xE375, "ù", 2),
    (0xE455, "Û", 2),
    (0xE475, "û", 2),
    (0xE559, "Ÿ", 2),
    (0xE579, "ÿ", 2),
    (0xEB41, "Ą", 2),
    (0xEB61, "ą", 2),
    (0xE243, "Ć", 2),
    (0xE263, "ć", 2),
    (0xEB45, "Ę", 2),
    (0xEB65, "ę", 2),
    (0x009C, "Ł", 2),
    (0x00BC, "ł", 2),
    (0xE24E, "Ń", 2),
    (0xE26E, "ń", 2),
    (0xE24F, "Ó", 2),
    (0xE26F, "ó", 2),
    (0xE253, "Ś", 2),
    (0xE273, "ś", 2),
    (0xE25A, "Ź", 2),
    (0xE27A, "ź", 2),
    (0xE85A, "Ż", 2),
    (0xE87A, "ż", 2),
    (0x0087, "þ", 2),
    (0x0089, "ð", 2),
    (0x0088, "Þ", 2),
    (0x008C, "Đ", 2),
    (0xE653, "Ş", 2),
    (0xE673, "ş", 2),
    (0x007B, "ı", 2),
    (0xEA67, "ǧ", 2),
    (0xEA47, "Ǧ", 2),
    (0xE849, "İ", 2),
    (0xE75A, "Ž", 2),
    (0xE753, "Š", 2),
    (0xE743, "Č", 2),
    (0xE77A, "ž", 2),
    (0xE773, "š", 2),
    (0xE763, "č", 2),
    (0x00AE, "đ", 2),
    (0x00A8, "¿", 2),
    (0x00AD, "¡", 2),
    (0x00A6, "ª", 2),
    (0x00A7, "º", 2),
    (0x00AB, "½", 2),
    (0x00BB, "»", 2),
    (0x00B3, "³", 2),
    (0x001C, "“", 2),
    (0x001D, "”", 2),
    (0x0018, "‘", 2),
    (0x0019, "’", 2),
    (0x0013, "–", 2),
    (0x0014, "—", 2),
    (0x0083, "³", 2),
    (0x005B, "¤", 2),
    (0x0085, "ŧ", 2),
    (0x0086, "Ŧ", 2),
    (0x008A, "«", 2),
    (0x008B, "»", 2),
    (0x008E, "Ŋ", 2),
    (0x008D, "ŋ", 2),
    (0x0095, "Ħ", 2),
    (0x0096, "ħ", 2),
    (0x009A, "Œ", 2),
    (0x009B, "¢", 2),
    (0x009D, "¥", 2),
    (0x009E, "€", 2),
    (0x00A9, "°", 2),
    (0x00AA, "¾", 2),
    (0x00AC, "¼", 2),
    (0x00B1, "±", 2),
    (0x00B5, "µ", 2),
    (0x00BA, "œ", 2),
    (0x00BF, "ǧ", 2),
    (0x00D2, "®", 2),
    (0x00D4, "©", 2),
    (0x4CE2, "Ľ", 1),
    (0x74E2, "ť", 1),
    (0x64E2, "ď", 1),
    (0x6CE2, "ľ", 1),
    (0xE020, "˜", 0),
    (0xE045, "Ẽ", 2),
    (0xE049, "Ĩ", 2),
    (0xE055, "Ũ", 2),
    (0xE056, "Ṽ", 2),
    (0xE059, "Ỹ", 2),
    (0xE065, "ẽ", 2),
    (0xE069, "ĩ", 2),
    (0xE075, "ũ", 2),
    (0xE076, "ṽ", 2),
    (0xE079, "ỹ", 2),
    (0xE120, "˚", 0),
    (0xE155, "Ů", 2),
    (0xE175, "ů", 2),
    (0xE177, "ẘ", 2),
    (0xE179, "ẙ", 2),
    (0xE220, "´", 0),
    (0xE247, "Ǵ", 2),
    (0xE24B, "Ḱ", 2),
    (0xE24C, "Ĺ", 2),
    (0xE24D, "Ḿ", 2),
    (0xE250, "Ṕ", 2),
    (0xE252, "Ŕ", 2),
    (0xE257, "Ẃ", 2),
    (0xE25C, "Ǽ", 2),
    (0xE25D, "Ǿ", 2),
    (0xE267, "ǵ", 2),
    (0xE26B, "ḱ", 2),
    (0xE26C, "ĺ", 2),
    (0xE26D, "ḿ", 2),
    (0xE270, "ṕ", 2),
    (0xE272, "ŕ", 2),
    (0xE277, "ẃ", 2),
    (0xE27C, "ǽ", 2),
    (0xE27D, "ǿ", 2),
    (0xE320, "`", 0),
    (0xE34E, "Ǹ", 2),
    (0xE357, "Ẁ", 2),
    (0xE359, "Ỳ", 2),
    (0xE36E, "ǹ", 2),
    (0xE377, "ẁ", 2),
    (0xE379, "ỳ", 2),
    (0xE420, "^", 0),
    (0xE457, "Ŵ", 2),
    (0xE459, "Ŷ", 2),
    (0xE45A, "Ẑ", 2),
    (0xE477, "ŵ", 2),
    (0xE479, "ŷ", 2),
    (0xE47A, "ẑ", 2),
    (0xE520, "¨", 0),
    (0xE548, "Ḧ", 2),
    (0xE557, "Ẅ", 2),
    (0xE558, "Ẍ", 2),
    (0xE568, "ḧ", 2),
    (0xE574, "ẗ", 2),
    (0xE577, "ẅ", 2),
    (0xE578, "ẍ", 2),
    (0xE620, "¸", 0),
    (0xE644, "Ḑ", 2),
    (0xE645, "Ȩ", 2),
    (0xE647, "Ģ", 2),
    (0xE648, "Ḩ", 2),
    (0xE64B, "Ķ", 2),
    (0xE64C, "Ļ", 2),
    (0xE64E, "Ņ", 2),
    (0xE652, "Ŗ", 2),
    (0xE654, "Ţ", 2),
    (0xE664, "ḑ", 2),
    (0xE665, "ȩ", 2),
    (0xE667, "ģ", 2),
    (0xE668, "ḩ", 2),
    (0xE66B, "ķ", 2),
    (0xE66C, "ļ", 2),
    (0xE66E, "ņ", 2),
    (0xE672, "ŗ", 2),
    (0xE674, "ţ", 2),
    (0xE720, "ˇ", 0),
    (0xE741, "Ă", 2),
    (0xE744, "Ď", 2),
    (0xE745, "Ě", 2),
    (0xE747, "Ǧ", 2),
    (0xE748, "Ȟ", 2),
    (0xE749, "Ǐ", 2),
    (0xE74B, "Ǩ", 2),
    (0xE74E, "Ň", 2),
    (0xE74F, "Ǒ", 2),
    (0xE752, "Ř", 2),
    (0xE754, "Ť", 2),
    (0xE755, "Ǔ", 2),
    (0xE761, "ă", 2),
    (0xE765, "ě", 2),
    (0xE768, "ȟ", 2),
    (0xE769, "ǐ", 2),
    (0xE76A, "ǰ", 2),
    (0xE76B, "ǩ", 2),
    (0xE76E, "ň", 2),
    (0xE76F, "ǒ", 2),
    (0xE772, "ř", 2),
    (0xE775, "ǔ", 2),
    (0xE820, "˙", 0),
    (0xE841, "Ȧ", 2),
    (0xE842, "Ḃ", 2),
    (0xE843, "Ċ", 2),
    (0xE844, "Ḋ", 2),
    (0xE845, "Ė", 2),
    (0xE846, "Ḟ", 2),
    (0xE847, "Ġ", 2),
    (0xE848, "Ḣ", 2),
    (0xE84D, "Ṁ", 2),
    (0xE84E, "Ṅ", 2),
    (0xE84F, "Ȯ", 2),
    (0xE850, "Ṗ", 2),
    (0xE852, "Ṙ", 2),
    (0xE853, "Ṡ", 2),
    (0xE854, "Ṫ", 2),
    (0xE857, "Ẇ", 2),
    (0xE858, "Ẋ", 2),
    (0xE859, "Ẏ", 2),
    (0xE861, "ȧ", 2),
    (0xE862, "ḃ", 2),
    (0xE863, "ċ", 2),
    (0xE864, "ḋ", 2),
    (0xE865, "ė", 2),
    (0xE866, "ḟ", 2),
    (0xE867, "ġ", 2),
    (0xE868, "ḣ", 2),
    (0xE86D, "ṁ", 2),
    (0xE86E, "ṅ", 2),
    (0xE86F, "ȯ", 2),
    (0xE870, "ṗ", 2),
    (0xE872, "ṙ", 2),
    (0xE873, "ṡ", 2),
    (0xE874, "ṫ", 2),
    (0xE877, "ẇ", 2),
    (0xE878, "ẋ", 2),
    (0xE879, "ẏ", 2),
    (0xE920, "¯", 0),
    (0xE941, "Ā", 2),
    (0xE945, "Ē", 2),
    (0xE947, "Ḡ", 2),
    (0xE949, "Ī", 2),
    (0xE94F, "Ō", 2),
    (0xE955, "Ū", 2),
    (0xE959, "Ȳ", 2),
    (0xE95C, "Ǣ", 2),
    (0xE961, "ā", 2),
    (0xE965, "ē", 2),
    (0xE967, "ḡ", 2),
    (0xE969, "ī", 2),
    (0xE96F, "ō", 2),
    (0xE975, "ū", 2),
    (0xE979, "ȳ", 2),
    (0xE97C, "ǣ", 2),
    (0xEA20, "ˇ", 0),
    (0xEA41, "Ă", 2),
    (0xEA45, "Ě", 2),
    (0xEA49, "Ǐ", 2),
    (0xEA4F, "Ǒ", 2),
    (0xEA61, "ă", 2),
    (0xEA65, "ě", 2),
    (0xEA69, "ǐ", 2),
    (0xEA6F, "ǒ", 2),
    (0xEB20, "˛", 0),
    (0xEB49, "Į", 2),
    (0xEB4F, "Ǫ", 2),
    (0xEB55, "Ų", 2),
    (0xEB69, "į", 2),
    (0xEB6F, "ǫ", 2),
    (0xEB75, "ų", 2),
    (0xEC20, "˝", 0),
    (0xEC4F, "Ő", 2),
    (0xEC55, "Ű", 2),
    (0xEC6F, "ő", 2),
    (0xEC75, "ű", 2),
    (0x1C00, " ", 2),
    (0x00C0, "[", 2),
    (0x00C1, "]", 2),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a one-record PAC buffer. `align`: raw alignment byte (bits 0-1
    /// align, bit 2 italic). `text` is raw Latin bytes; 0xFE separates lines.
    fn build_pac(
        start_ms_parts: (u16, u16),
        end_ms_parts: (u16, u16),
        valign: u8,
        align: u8,
        text: &[u8],
    ) -> Vec<u8> {
        let mut b = vec![0u8; 41];
        b[0] = 1;
        let ts = 23;
        b[ts] = 0x60; // spot byte
        let put16 = |b: &mut [u8], at: usize, v: u16| {
            b[at] = (v & 0xff) as u8;
            b[at + 1] = (v >> 8) as u8;
        };
        // start tc: high (HHMM), low (SSFF)
        put16(&mut b, ts + 1, start_ms_parts.0);
        put16(&mut b, ts + 3, start_ms_parts.1);
        put16(&mut b, ts + 5, end_ms_parts.0);
        put16(&mut b, ts + 7, end_ms_parts.1);
        // text length: large enough to cover the text plus slack
        b[ts + 9] = (text.len() as u8).saturating_add(20);
        b[ts + 10] = 0;
        b[ts + 11] = valign;
        // fe record at ts+15
        b[ts + 15] = 0xFE;
        b[ts + 16] = align;
        b[ts + 17] = 0x03;
        b.extend_from_slice(text);
        // trailing padding so the record is found and the scan then ends
        b.extend(std::iter::repeat_n(0u8, 30));
        b
    }

    #[test]
    fn pac_parses_latin_text_and_timing() {
        // start 00:00:01:00 -> high 0, low 100 ; end 00:00:05:00 -> high 0, low 500
        let buf = build_pac((0, 100), (0, 500), 10, 0x02, b"AB");
        let cues = parse_pac(&buf, CODEPAGE_LATIN).unwrap();
        assert_eq!(cues.len(), 1);
        let c = &cues[0];
        assert_eq!(c.start_ms, 1000);
        assert_eq!(c.end_ms, 5000);
        assert_eq!(c.plain_text(), "AB");
        assert_eq!(c.align, Some(HAlign::Center));
    }

    #[test]
    fn pac_decodes_diacritic_and_italic_lines() {
        // line 1 italic (align bit 0x04): "A", 0xFE next-align non-italic, then
        // accented "á" via 0xE2 0x61
        let text = [b'A', 0xFE, 0x00, 0x03, 0xE2, 0x61];
        let buf = build_pac((0, 100), (0, 300), 10, 0x04, &text);
        let cues = parse_pac(&buf, CODEPAGE_LATIN).unwrap();
        let c = &cues[0];
        assert_eq!(c.plain_text(), "A\ná");
        assert!(c.runs[0].italic);
        assert!(!c.runs[1].italic);
    }

    #[test]
    fn pac_rejects_unsupported_codepage() {
        let buf = build_pac((0, 100), (0, 500), 10, 0x02, b"AB");
        assert!(matches!(
            parse_pac(&buf, 3),
            Err(SubtitleError::UnsupportedCodepage(3))
        ));
    }

    #[test]
    fn pac_rejects_bad_header() {
        assert!(matches!(
            parse_pac(&[0u8; 40], CODEPAGE_LATIN),
            Err(SubtitleError::Parse(_))
        ));
    }
}
