use std::fmt::Write;

use asdcplib::Rational;
use asdcplib::as02::jp2k::{Jpeg2000PictureSubDescriptor, RgbaEssenceDescriptor};
use asdcplib::as02::pcm::WaveAudioDescriptor;
use asdcplib::pcm::{McaLabelKind, McaLabelSubDescriptor};

use crate::packaging::escape_xml;

// the three registry namespaces a picture essence descriptor is spelled in
const AAF: &str = "http://www.smpte-ra.org/reg/395/2014/13/1/aaf";
const ITEMS: &str = "http://www.smpte-ra.org/reg/335/2012";
const TYPES: &str = "http://www.smpte-ra.org/reg/2003/2012";

const FRAME_LAYOUT_SYMBOLS: [&str; 5] = [
    "FullFrame",
    "SeparateFields",
    "OneField",
    "MixedFields",
    "SegmentedFrame",
];
const SCANNING_DIRECTION_SYMBOLS: [&str; 8] = [
    "ScanningDirection_LeftToRightTopToBottom",
    "ScanningDirection_RightToLeftTopToBottom",
    "ScanningDirection_LeftToRightBottomToTop",
    "ScanningDirection_RightToLeftBottomToTop",
    "ScanningDirection_TopToBottomLeftToRight",
    "ScanningDirection_TopToBottomRightToLeft",
    "ScanningDirection_BottomToTopLeftToRight",
    "ScanningDirection_BottomToTopRightToLeft",
];

// the batch header the MXF puts before the SIZ triples
const BATCH_HEADER_BYTES: usize = 8;
const COMPONENT_SIZING_BYTES: usize = 3;

// asdcplib writes SoundEssenceCoding without ever setting it, so all zeros means the MXF has no value
const UNSET_UL: [u8; 16] = [0; 16];

/// The RGBA picture essence descriptor of a wrapped AS-02 JPEG 2000 track file,
/// as the RegXML an IMF CPL's EssenceDescriptorList carries. Every item comes
/// off the MXF, so a validator comparing the two sees one descriptor twice.
pub fn picture_descriptor_regxml(
    descriptor: &RgbaEssenceDescriptor,
    jpeg2000: &Jpeg2000PictureSubDescriptor,
) -> String {
    let mut xml = String::new();
    let _ = writeln!(
        xml,
        r#"      <r0:RGBADescriptor xmlns:r0="{AAF}" xmlns:r1="{ITEMS}" xmlns:r2="{TYPES}">"#
    );
    const INDENT: &str = "        ";
    item(
        &mut xml,
        INDENT,
        "InstanceID",
        &urn_uuid(&descriptor.instance_id),
    );
    if let Some(duration) = descriptor.container_duration {
        item(&mut xml, INDENT, "EssenceLength", &duration.to_string());
    }
    if let Some(track) = descriptor.linked_track_id {
        item(&mut xml, INDENT, "LinkedTrackID", &track.to_string());
    }
    item(
        &mut xml,
        INDENT,
        "SampleRate",
        &rational(&descriptor.sample_rate),
    );
    item(
        &mut xml,
        INDENT,
        "ContainerFormat",
        &urn_ul(&descriptor.essence_container),
    );
    item(
        &mut xml,
        INDENT,
        "FrameLayout",
        frame_layout_symbol(descriptor.frame_layout),
    );
    item(
        &mut xml,
        INDENT,
        "StoredWidth",
        &descriptor.stored_width.to_string(),
    );
    item(
        &mut xml,
        INDENT,
        "StoredHeight",
        &descriptor.stored_height.to_string(),
    );
    item(
        &mut xml,
        INDENT,
        "ImageAspectRatio",
        &rational(&descriptor.aspect_ratio),
    );
    item(
        &mut xml,
        INDENT,
        "PictureCompression",
        &urn_ul(&descriptor.picture_essence_coding),
    );
    if let Some(transfer) = descriptor.hdr.transfer_characteristic {
        item(
            &mut xml,
            INDENT,
            "TransferCharacteristic",
            &urn_ul(&transfer),
        );
    }
    if let Some(primaries) = descriptor.hdr.color_primaries {
        item(&mut xml, INDENT, "ColorPrimaries", &urn_ul(&primaries));
    }
    if let Some(max) = descriptor.component_max_ref {
        item(&mut xml, INDENT, "ComponentMaxRef", &max.to_string());
    }
    if let Some(min) = descriptor.component_min_ref {
        item(&mut xml, INDENT, "ComponentMinRef", &min.to_string());
    }
    if let Some(direction) = descriptor.scanning_direction {
        item(
            &mut xml,
            INDENT,
            "ScanningDirection",
            scanning_direction_symbol(direction),
        );
    }
    if let Some(primaries) = &descriptor.hdr.mastering_display_primaries {
        xml.push_str("        <r1:MasteringDisplayPrimaries>\n");
        for [x, y] in primaries {
            let _ = writeln!(
                xml,
                "          <r2:ColorPrimary><r2:X>{x}</r2:X><r2:Y>{y}</r2:Y></r2:ColorPrimary>"
            );
        }
        xml.push_str("        </r1:MasteringDisplayPrimaries>\n");
    }
    if let Some([x, y]) = descriptor.hdr.mastering_display_white_point {
        let _ = writeln!(
            xml,
            "        <r1:MasteringDisplayWhitePointChromaticity><r2:X>{x}</r2:X><r2:Y>{y}</r2:Y></r1:MasteringDisplayWhitePointChromaticity>"
        );
    }
    if let Some(max) = descriptor.hdr.mastering_display_max_luminance {
        item(
            &mut xml,
            INDENT,
            "MasteringDisplayMaximumLuminance",
            &max.to_string(),
        );
    }
    if let Some(min) = descriptor.hdr.mastering_display_min_luminance {
        item(
            &mut xml,
            INDENT,
            "MasteringDisplayMinimumLuminance",
            &min.to_string(),
        );
    }
    if let Some([first, second]) = descriptor.video_line_map {
        let _ = writeln!(
            xml,
            "        <r1:VideoLineMap><r2:Int32>{first}</r2:Int32><r2:Int32>{second}</r2:Int32></r1:VideoLineMap>"
        );
    }
    xml.push_str(&rgba_layout(
        INDENT,
        "PixelLayout",
        &descriptor.pixel_layout,
    ));

    xml.push_str("        <r1:SubDescriptors>\n");
    xml.push_str(&jpeg2000_sub_descriptor_regxml(jpeg2000));
    xml.push_str("        </r1:SubDescriptors>\n");
    xml.push_str("      </r0:RGBADescriptor>");
    xml
}

fn item(xml: &mut String, indent: &str, name: &str, value: &str) {
    let _ = writeln!(xml, "{indent}<r1:{name}>{value}</r1:{name}>");
}

/// The WAVE PCM essence descriptor of a wrapped AS-02 sound track file, as the
/// RegXML an IMF CPL's EssenceDescriptorList carries. Every item comes off the
/// MXF, the MCA label InstanceIDs included, so a validator comparing the two
/// sees one descriptor twice.
pub fn sound_descriptor_regxml(
    descriptor: &WaveAudioDescriptor,
    labels: &[McaLabelSubDescriptor],
) -> String {
    let mut xml = String::new();
    let _ = writeln!(
        xml,
        r#"      <r0:WAVEPCMDescriptor xmlns:r0="{AAF}" xmlns:r1="{ITEMS}">"#
    );
    const INDENT: &str = "        ";
    item(
        &mut xml,
        INDENT,
        "InstanceID",
        &urn_uuid(&descriptor.instance_id),
    );
    if let Some(generation) = &descriptor.generation_id {
        item(
            &mut xml,
            INDENT,
            "LinkedGenerationID",
            &urn_uuid(generation),
        );
    }
    item(
        &mut xml,
        INDENT,
        "SampleRate",
        &rational(&descriptor.sample_rate),
    );
    if let Some(duration) = descriptor.container_duration {
        item(&mut xml, INDENT, "EssenceLength", &duration.to_string());
    }
    item(
        &mut xml,
        INDENT,
        "ContainerFormat",
        &urn_ul(&descriptor.essence_container),
    );
    if let Some(codec) = &descriptor.codec {
        item(&mut xml, INDENT, "Codec", &urn_ul(codec));
    }
    if let Some(track) = descriptor.linked_track_id {
        item(&mut xml, INDENT, "LinkedTrackID", &track.to_string());
    }
    item(
        &mut xml,
        INDENT,
        "AudioSampleRate",
        &rational(&descriptor.audio_sampling_rate),
    );
    item(
        &mut xml,
        INDENT,
        "Locked",
        // the smpte boolean type spells its values True and False
        if descriptor.locked { "True" } else { "False" },
    );
    if let Some(level) = descriptor.audio_ref_level {
        item(
            &mut xml,
            INDENT,
            "AudioReferenceLevel",
            &(level as i8).to_string(),
        );
    }
    item(
        &mut xml,
        INDENT,
        "ChannelCount",
        &descriptor.channel_count.to_string(),
    );
    item(
        &mut xml,
        INDENT,
        "QuantizationBits",
        &descriptor.quantization_bits.to_string(),
    );
    if let Some(dial_norm) = descriptor.dial_norm {
        item(&mut xml, INDENT, "DialNorm", &(dial_norm as i8).to_string());
    }
    if descriptor.sound_essence_coding != UNSET_UL {
        item(
            &mut xml,
            INDENT,
            "SoundCompression",
            &urn_ul(&descriptor.sound_essence_coding),
        );
    }
    if let Some(level) = descriptor.reference_audio_alignment_level {
        item(
            &mut xml,
            INDENT,
            "ReferenceAudioAlignmentLevel",
            &(level as i8).to_string(),
        );
    }
    if let Some(edit_rate) = &descriptor.reference_image_edit_rate {
        item(
            &mut xml,
            INDENT,
            "ReferenceImageEditRate",
            &rational(edit_rate),
        );
    }
    item(
        &mut xml,
        INDENT,
        "BlockAlign",
        &descriptor.block_align.to_string(),
    );
    if let Some(offset) = descriptor.sequence_offset {
        item(&mut xml, INDENT, "SequenceOffset", &offset.to_string());
    }
    item(
        &mut xml,
        INDENT,
        "AverageBytesPerSecond",
        &descriptor.avg_bps.to_string(),
    );
    if let Some(assignment) = &descriptor.channel_assignment {
        item(&mut xml, INDENT, "ChannelAssignment", &urn_ul(assignment));
    }

    if !labels.is_empty() {
        xml.push_str("        <r1:SubDescriptors>\n");
        for label in labels {
            xml.push_str(&mca_label_sub_descriptor_regxml(label));
        }
        xml.push_str("        </r1:SubDescriptors>\n");
    }
    xml.push_str("      </r0:WAVEPCMDescriptor>");
    xml
}

fn mca_label_sub_descriptor_regxml(label: &McaLabelSubDescriptor) -> String {
    const INDENT: &str = "            ";
    let element = match label.kind {
        McaLabelKind::AudioChannel => "AudioChannelLabelSubDescriptor",
        McaLabelKind::SoundfieldGroup => "SoundfieldGroupLabelSubDescriptor",
        McaLabelKind::GroupOfSoundfieldGroups => "GroupOfSoundfieldGroupsLabelSubDescriptor",
    };
    let mut xml = String::new();
    let _ = writeln!(xml, "          <r0:{element}>");
    item(
        &mut xml,
        INDENT,
        "InstanceID",
        &urn_uuid(&label.instance_id),
    );
    item(
        &mut xml,
        INDENT,
        "MCALabelDictionaryID",
        &urn_ul(&label.label_dictionary_id),
    );
    item(&mut xml, INDENT, "MCALinkID", &urn_uuid(&label.link_id));
    item(
        &mut xml,
        INDENT,
        "MCATagSymbol",
        &escape_xml(&label.tag_symbol),
    );
    if let Some(tag_name) = &label.tag_name {
        item(&mut xml, INDENT, "MCATagName", &escape_xml(tag_name));
    }
    if let Some(channel) = label.channel_id {
        item(&mut xml, INDENT, "MCAChannelID", &channel.to_string());
    }
    if let Some(language) = &label.spoken_language {
        item(
            &mut xml,
            INDENT,
            "RFC5646SpokenLanguage",
            &escape_xml(language),
        );
    }
    if let Some(title) = &label.title {
        item(&mut xml, INDENT, "MCATitle", &escape_xml(title));
    }
    if let Some(title_version) = &label.title_version {
        item(
            &mut xml,
            INDENT,
            "MCATitleVersion",
            &escape_xml(title_version),
        );
    }
    if let Some(content_kind) = &label.audio_content_kind {
        item(
            &mut xml,
            INDENT,
            "MCAAudioContentKind",
            &escape_xml(content_kind),
        );
    }
    if let Some(element_kind) = &label.audio_element_kind {
        item(
            &mut xml,
            INDENT,
            "MCAAudioElementKind",
            &escape_xml(element_kind),
        );
    }
    if let Some(group) = &label.soundfield_group_link_id {
        item(&mut xml, INDENT, "SoundfieldGroupLinkID", &urn_uuid(group));
    }
    let _ = writeln!(xml, "          </r0:{element}>");
    xml
}

fn jpeg2000_sub_descriptor_regxml(sub: &Jpeg2000PictureSubDescriptor) -> String {
    const INDENT: &str = "            ";
    let mut xml = String::new();
    xml.push_str("          <r0:JPEG2000SubDescriptor>\n");
    item(&mut xml, INDENT, "InstanceID", &urn_uuid(&sub.instance_id));
    for (name, value) in [
        ("Rsiz", u64::from(sub.rsize)),
        ("Xsiz", u64::from(sub.xsize)),
        ("Ysiz", u64::from(sub.ysize)),
        ("XOsiz", u64::from(sub.x_osize)),
        ("YOsiz", u64::from(sub.y_osize)),
        ("XTsiz", u64::from(sub.xt_size)),
        ("YTsiz", u64::from(sub.yt_size)),
        ("XTOsiz", u64::from(sub.xt_osize)),
        ("YTOsiz", u64::from(sub.yt_osize)),
        ("Csiz", u64::from(sub.csize)),
    ] {
        item(&mut xml, INDENT, name, &value.to_string());
    }
    if let Some(coding_style) = &sub.coding_style_default {
        item(&mut xml, INDENT, "CodingStyleDefault", &hex(coding_style));
    }
    if let Some(quantization) = &sub.quantization_default {
        item(&mut xml, INDENT, "QuantizationDefault", &hex(quantization));
    }
    if let Some(sizing) = &sub.picture_component_sizing {
        xml.push_str("            <r1:PictureComponentSizing>\n");
        let triples = sizing.get(BATCH_HEADER_BYTES..).unwrap_or_default();
        for component in triples.as_chunks::<COMPONENT_SIZING_BYTES>().0 {
            let _ = writeln!(
                xml,
                "              <r2:J2KComponentSizing><r2:Ssiz>{}</r2:Ssiz><r2:XRSiz>{}</r2:XRSiz><r2:YRSiz>{}</r2:YRSiz></r2:J2KComponentSizing>",
                component[0], component[1], component[2]
            );
        }
        xml.push_str("            </r1:PictureComponentSizing>\n");
    }
    if let Some(layout) = &sub.j2c_layout {
        xml.push_str(&rgba_layout(INDENT, "J2CLayout", layout));
    }
    xml.push_str("          </r0:JPEG2000SubDescriptor>\n");
    xml
}

/// An RGBALayout item: eight component code and size pairs, the unused ones
/// carrying the null code, which is how the MXF stores them.
fn rgba_layout(indent: &str, name: &str, layout: &[u8; 16]) -> String {
    let mut xml = String::new();
    let _ = writeln!(xml, "{indent}<r1:{name}>");
    for pair in layout.as_chunks::<2>().0 {
        let _ = writeln!(
            xml,
            "{indent}  <r2:RGBAComponent><r2:Code>{}</r2:Code><r2:ComponentSize>{}</r2:ComponentSize></r2:RGBAComponent>",
            component_code_symbol(pair[0]),
            pair[1]
        );
    }
    let _ = writeln!(xml, "{indent}</r1:{name}>");
    xml
}

/// The registry symbol for an RGBA component code, as ST 377-1 spells the byte.
fn component_code_symbol(code: u8) -> String {
    match code {
        b'R' => "CompRed".into(),
        b'G' => "CompGreen".into(),
        b'B' => "CompBlue".into(),
        b'A' => "CompAlpha".into(),
        b'F' => "CompFill".into(),
        b'P' => "CompPalette".into(),
        b'L' => "CompLuma".into(),
        b'C' => "CompChroma".into(),
        b'D' => "CompDepth".into(),
        0 => "CompNull".into(),
        other => format!("{other}"),
    }
}

fn frame_layout_symbol(layout: u8) -> &'static str {
    FRAME_LAYOUT_SYMBOLS
        .get(layout as usize)
        .copied()
        .unwrap_or("FullFrame")
}

fn scanning_direction_symbol(direction: u8) -> &'static str {
    SCANNING_DIRECTION_SYMBOLS
        .get(direction as usize)
        .copied()
        .unwrap_or(SCANNING_DIRECTION_SYMBOLS[0])
}

fn rational(value: &Rational) -> String {
    format!("{}/{}", value.numerator, value.denominator)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A 16-byte SMPTE UL as its `urn:smpte:ul:` form, four dot-separated groups.
pub fn urn_ul(ul: &[u8; 16]) -> String {
    let group = |offset: usize| {
        format!(
            "{:02x}{:02x}{:02x}{:02x}",
            ul[offset],
            ul[offset + 1],
            ul[offset + 2],
            ul[offset + 3]
        )
    };
    format!(
        "urn:smpte:ul:{}.{}.{}.{}",
        group(0),
        group(4),
        group(8),
        group(12)
    )
}

pub fn urn_uuid(bytes: &[u8; 16]) -> String {
    format!("urn:uuid:{}", uuid::Uuid::from_bytes(*bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ul_reads_as_the_urn_a_cpl_carries() {
        let ul = [
            0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0d, 0x04, 0x01, 0x01, 0x01, 0x01, 0x0b,
            0x00, 0x00,
        ];
        assert_eq!(
            urn_ul(&ul),
            "urn:smpte:ul:060e2b34.0401010d.04010101.010b0000"
        );
    }
}
