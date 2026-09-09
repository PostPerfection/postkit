use std::path::Path;

use asdcplib::timed_text::MxfReader;

use crate::regxml;

// how much of the header partition to scan for the descriptor set; asdcplib sizes
// the timed-text header at 16 KiB plus a little per ancillary resource, so this
// covers it without reading the essence body
const HEADER_SCAN_BYTES: usize = 256 * 1024;

// DCTimedTextDescriptor set key, matched on the first four and last eight bytes so
// the registry/version bytes asdcplib stamps in between do not matter
const DESCRIPTOR_UL_PREFIX: [u8; 4] = [0x06, 0x0e, 0x2b, 0x34];
const DESCRIPTOR_UL_SUFFIX: [u8; 8] = [0x0d, 0x01, 0x01, 0x01, 0x01, 0x01, 0x64, 0x00];

// static local tags, no primer lookup needed
const INSTANCE_UID_TAG: u16 = 0x3c0a;
const LINKED_TRACK_ID_TAG: u16 = 0x3006;

/// The RegXML the CPL's EssenceDescriptorList carries for a wrapped AS-02
/// timed-text track file. asdcplib exposes the edit rate, container duration and
/// resource id; the descriptor's InstanceID and LinkedTrackID come off the MXF
/// header directly, the way the picture and sound descriptors read theirs, so
/// the CPL entry matches the MXF a validator reads back.
pub fn read_timed_text_descriptor_regxml(path: &Path) -> Result<String, String> {
    let mut reader = MxfReader::new();
    reader
        .open_read(&path.to_string_lossy())
        .map_err(|e| format!("cannot open timed text MXF {}: {e}", path.display()))?;
    let descriptor = reader.descriptor().map_err(|e| {
        format!(
            "cannot read timed text descriptor of {}: {e}",
            path.display()
        )
    })?;
    let _ = reader.close();

    let header = read_header_prefix(path)?;
    let set = timed_text_descriptor_set(&header).ok_or_else(|| {
        format!(
            "no timed text descriptor set found in the header of {}",
            path.display()
        )
    })?;
    let instance_id = local_item(set, INSTANCE_UID_TAG)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| {
            format!(
                "timed text descriptor of {} carries no InstanceID",
                path.display()
            )
        })?;
    let linked_track_id = local_item(set, LINKED_TRACK_ID_TAG)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| {
            format!(
                "timed text descriptor of {} carries no LinkedTrackID",
                path.display()
            )
        })?;

    Ok(regxml::timed_text_descriptor_regxml(
        &descriptor,
        &instance_id,
        linked_track_id,
    ))
}

fn read_header_prefix(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut buffer = vec![0u8; HEADER_SCAN_BYTES];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file
            .read(&mut buffer[filled..])
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

/// The value bytes of the first DCTimedTextDescriptor local set in the header.
fn timed_text_descriptor_set(header: &[u8]) -> Option<&[u8]> {
    let mut pos = 0;
    while pos + 16 <= header.len() {
        let key = &header[pos..pos + 16];
        let (length, length_bytes) = ber_length(&header[pos + 16..])?;
        let value_start = pos + 16 + length_bytes;
        let value_end = value_start.checked_add(length)?;
        if value_end > header.len() {
            return None;
        }
        if key[0..4] == DESCRIPTOR_UL_PREFIX && key[8..16] == DESCRIPTOR_UL_SUFFIX {
            return Some(&header[value_start..value_end]);
        }
        pos = value_end;
    }
    None
}

/// The value of a 2-byte-tag, 2-byte-length local set item.
fn local_item(set: &[u8], tag: u16) -> Option<&[u8]> {
    let mut pos = 0;
    while pos + 4 <= set.len() {
        let item_tag = u16::from_be_bytes([set[pos], set[pos + 1]]);
        let length = u16::from_be_bytes([set[pos + 2], set[pos + 3]]) as usize;
        let value_start = pos + 4;
        let value_end = value_start.checked_add(length)?;
        if value_end > set.len() {
            return None;
        }
        if item_tag == tag {
            return Some(&set[value_start..value_end]);
        }
        pos = value_end;
    }
    None
}

/// A KLV BER length: short form is one byte under 0x80, long form is 0x80 plus a
/// count of the bytes that follow. Returns the length and how many bytes it took.
fn ber_length(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    if first < 0x80 {
        return Some((first as usize, 1));
    }
    let count = (first & 0x7f) as usize;
    if count == 0 || count > 8 || bytes.len() < 1 + count {
        return None;
    }
    let mut length = 0usize;
    for &byte in &bytes[1..1 + count] {
        length = (length << 8) | byte as usize;
    }
    Some((length, 1 + count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mxf_wrap::{EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap};

    const IMSC1: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml"
    xmlns:ttp="http://www.w3.org/ns/ttml#parameter"
    xmlns:ittp="http://www.w3.org/ns/ttml/profile/imsc1#parameter"
    ttp:profile="http://www.w3.org/ns/ttml/profile/imsc1/text" xml:lang="en">
  <body><div>
    <p begin="00:00:00.100" end="00:00:00.400">hi</p>
  </div></body>
</tt>"#;

    #[test]
    fn descriptor_regxml_carries_the_mxf_instance_id_and_linked_track_id() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("sub.xml");
        std::fs::write(&xml, IMSC1).unwrap();
        let out = dir.path().join("SUBTITLE.mxf");

        let opts = MxfWrapOptions {
            input_files: vec![xml],
            output: out.clone(),
            essence_type: EssenceType::TimedText,
            standard: MxfStandard::As02,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: None,
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: Some(24),
        };

        let result = mxf_wrap(&opts);
        assert!(result.success, "timed text wrap failed: {}", result.error);

        let regxml = read_timed_text_descriptor_regxml(&out).expect("read descriptor");
        assert!(regxml.contains("<r0:DCTimedTextDescriptor"), "{regxml}");
        assert!(regxml.contains("<r1:InstanceID>urn:uuid:"), "{regxml}");
        assert!(regxml.contains("<r1:LinkedTrackID>"), "{regxml}");
        assert!(
            regxml.contains("<r1:EssenceLength>24</r1:EssenceLength>"),
            "{regxml}"
        );
        assert!(
            regxml.contains(
                "<r1:NamespaceURI>http://www.w3.org/ns/ttml/profile/imsc1/text</r1:NamespaceURI>"
            ),
            "{regxml}"
        );
        assert!(
            regxml.contains("<r1:UCSEncoding>UTF-8</r1:UCSEncoding>"),
            "{regxml}"
        );
        assert!(
            regxml.contains(
                "<r1:DataEssenceCoding>urn:smpte:ul:00000000.00000000.00000000.00000000</r1:DataEssenceCoding>"
            ),
            "{regxml}"
        );
        // the InstanceID has to be a real uuid, not the all-zero placeholder
        assert!(
            !regxml.contains("urn:uuid:00000000-0000-0000-0000-000000000000"),
            "{regxml}"
        );
        println!("{regxml}");
    }
}
