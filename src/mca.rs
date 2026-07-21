//! MCA (Multi-Channel Audio) label definitions and soundfield management
//! per SMPTE ST 377-4.

use std::fmt::Write as _;

use serde::Serialize;

/// SMPTE MCA channel tag symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum McaTagSymbol {
    L,
    R,
    C,
    Lfe,
    Ls,
    Rs,
    Lss,
    Rss,
    Lrs,
    Rrs,
    Lt,
    Rt,
    M1,
    M2,
    Ltf,
    Rtf,
    Ltr,
    Rtr,
    Vi,
    Hi,
}

impl McaTagSymbol {
    /// Human-readable name for this channel.
    pub fn tag_name(self) -> &'static str {
        match self {
            Self::L => "Left",
            Self::R => "Right",
            Self::C => "Center",
            Self::Lfe => "LFE",
            Self::Ls => "Left Surround",
            Self::Rs => "Right Surround",
            Self::Lss => "Left Side Surround",
            Self::Rss => "Right Side Surround",
            Self::Lrs => "Left Rear Surround",
            Self::Rrs => "Right Rear Surround",
            Self::Lt => "Left Total",
            Self::Rt => "Right Total",
            Self::M1 => "Mono One",
            Self::M2 => "Mono Two",
            Self::Ltf => "Left Top Front",
            Self::Rtf => "Right Top Front",
            Self::Ltr => "Left Top Rear",
            Self::Rtr => "Right Top Rear",
            Self::Vi => "Visually Impaired",
            Self::Hi => "Hearing Impaired",
        }
    }

    /// MCA tag symbol string (e.g. "chL", "chR").
    pub fn symbol_string(self) -> &'static str {
        match self {
            Self::L => "chL",
            Self::R => "chR",
            Self::C => "chC",
            Self::Lfe => "chLFE",
            Self::Ls => "chLs",
            Self::Rs => "chRs",
            Self::Lss => "chLss",
            Self::Rss => "chRss",
            Self::Lrs => "chLrs",
            Self::Rrs => "chRrs",
            Self::Lt => "chLt",
            Self::Rt => "chRt",
            Self::M1 => "chM1",
            Self::M2 => "chM2",
            Self::Ltf => "chLtf",
            Self::Rtf => "chRtf",
            Self::Ltr => "chLtr",
            Self::Rtr => "chRtr",
            Self::Vi => "chVIN",
            Self::Hi => "chHI",
        }
    }
}

/// An individual MCA channel label.
#[derive(Debug, Clone, Serialize)]
pub struct McaLabel {
    pub symbol: McaTagSymbol,
    pub tag_name: String,
    pub tag_symbol: String,
    pub channel_index: u32,
    pub spoken_language: String,
}

/// A soundfield group (collection of channels).
#[derive(Debug, Clone, Serialize)]
pub struct McaSoundfield {
    pub name: String,
    pub channels: Vec<McaLabel>,
}

fn label(symbol: McaTagSymbol, index: u32) -> McaLabel {
    McaLabel {
        symbol,
        tag_name: symbol.tag_name().to_string(),
        tag_symbol: symbol.symbol_string().to_string(),
        channel_index: index,
        spoken_language: String::new(),
    }
}

/// Standard stereo (2.0) soundfield.
pub fn soundfield_stereo() -> McaSoundfield {
    McaSoundfield {
        name: "20".to_string(),
        channels: vec![label(McaTagSymbol::L, 0), label(McaTagSymbol::R, 1)],
    }
}

/// Standard 5.1 surround soundfield.
pub fn soundfield_51() -> McaSoundfield {
    McaSoundfield {
        name: "51".to_string(),
        channels: vec![
            label(McaTagSymbol::L, 0),
            label(McaTagSymbol::R, 1),
            label(McaTagSymbol::C, 2),
            label(McaTagSymbol::Lfe, 3),
            label(McaTagSymbol::Ls, 4),
            label(McaTagSymbol::Rs, 5),
        ],
    }
}

/// Standard 7.1 surround soundfield.
pub fn soundfield_71() -> McaSoundfield {
    McaSoundfield {
        name: "71".to_string(),
        channels: vec![
            label(McaTagSymbol::L, 0),
            label(McaTagSymbol::R, 1),
            label(McaTagSymbol::C, 2),
            label(McaTagSymbol::Lfe, 3),
            label(McaTagSymbol::Ls, 4),
            label(McaTagSymbol::Rs, 5),
            label(McaTagSymbol::Lrs, 6),
            label(McaTagSymbol::Rrs, 7),
        ],
    }
}

/// 5.1 with Hearing Impaired and Visually Impaired tracks.
pub fn soundfield_51_with_hi_vi() -> McaSoundfield {
    let mut sf = soundfield_51();
    sf.name = "51+HI+VI".to_string();
    sf.channels.push(label(McaTagSymbol::Hi, 6));
    sf.channels.push(label(McaTagSymbol::Vi, 7));
    sf
}

/// Auto-detect soundfield from channel count.
pub fn detect_soundfield(channel_count: u32) -> McaSoundfield {
    match channel_count {
        1 => McaSoundfield {
            name: "10".to_string(),
            channels: vec![label(McaTagSymbol::M1, 0)],
        },
        2 => soundfield_stereo(),
        6 => soundfield_51(),
        8 => soundfield_51_with_hi_vi(),
        _ => soundfield_71(),
    }
}

impl McaTagSymbol {
    /// asdcplib `ASDCP_MCAConfigParser` channel token, or `None` when that parser
    /// has no label for this symbol (its DCP dictionary is L/R/C/LFE/Ls/Rs plus
    /// the rear-surround pair and HI/VI-N; the wider IMF set is AS-02 only).
    fn asdcp_token(self) -> Option<&'static str> {
        match self {
            Self::L => Some("L"),
            Self::R => Some("R"),
            Self::C => Some("C"),
            Self::Lfe => Some("LFE"),
            Self::Ls => Some("Ls"),
            Self::Rs => Some("Rs"),
            Self::Lrs => Some("Lrs"),
            Self::Rrs => Some("Rrs"),
            Self::Hi => Some("HI"),
            Self::Vi => Some("VIN"),
            _ => None,
        }
    }
}

/// Build an asdcp-wrap style MCA config string (e.g. `"51(L,R,C,LFE,Ls,Rs),HI,VIN"`)
/// for the PCM wrap path, or `None` if any channel has no asdcplib DCP label.
///
/// HI and VI-N are emitted as standalone channels after the soundfield group,
/// matching how the accessibility tracks sit outside the 5.1/7.1 group.
pub fn soundfield_to_mca_config(sf: &McaSoundfield) -> Option<String> {
    let is_accessibility = |s: McaTagSymbol| matches!(s, McaTagSymbol::Hi | McaTagSymbol::Vi);

    let mut group = Vec::new();
    let mut standalone = Vec::new();
    for ch in &sf.channels {
        let token = ch.symbol.asdcp_token()?;
        if is_accessibility(ch.symbol) {
            standalone.push(token);
        } else {
            group.push(token);
        }
    }
    if group.is_empty() {
        return None;
    }

    // Only 5.1/6.1/7.1 have a soundfield-group label in the DCP dictionary; other
    // layouts are wrapped as bare channels. Match on the layout prefix so a name
    // like "51+HI+VI" still uses the 51 group with HI/VI-N as standalone channels.
    let group_label = ["51", "61", "71"]
        .into_iter()
        .find(|g| sf.name.starts_with(g));
    let mut out = match group_label {
        Some(g) => format!("{}({})", g, group.join(",")),
        None => group.join(","),
    };
    for s in standalone {
        out.push(',');
        out.push_str(s);
    }
    Some(out)
}

/// Generate MCA subdescriptor XML for inclusion in a CPL.
pub fn generate_mca_xml(sf: &McaSoundfield) -> String {
    let mut xml = String::new();
    let _ = writeln!(xml, "  <r0:MCALabelSubDescriptors>");
    let _ = writeln!(xml, "    <r0:SoundfieldGroupLabelSubDescriptor>");
    let _ = writeln!(
        xml,
        "      <r0:MCATagSymbol>sg{}</r0:MCATagSymbol>",
        sf.name
    );
    let _ = writeln!(
        xml,
        "      <r0:MCATagName>Soundfield {}</r0:MCATagName>",
        sf.name
    );
    let _ = writeln!(xml, "    </r0:SoundfieldGroupLabelSubDescriptor>");

    for ch in &sf.channels {
        let _ = writeln!(xml, "    <r0:AudioChannelLabelSubDescriptor>");
        let _ = writeln!(
            xml,
            "      <r0:MCAChannelID>{}</r0:MCAChannelID>",
            ch.channel_index + 1
        );
        let _ = writeln!(
            xml,
            "      <r0:MCATagSymbol>{}</r0:MCATagSymbol>",
            ch.tag_symbol
        );
        let _ = writeln!(xml, "      <r0:MCATagName>{}</r0:MCATagName>", ch.tag_name);
        if !ch.spoken_language.is_empty() {
            let _ = writeln!(
                xml,
                "      <r0:RFC5646SpokenLanguage>{}</r0:RFC5646SpokenLanguage>",
                ch.spoken_language
            );
        }
        let _ = writeln!(xml, "    </r0:AudioChannelLabelSubDescriptor>");
    }

    let _ = writeln!(xml, "  </r0:MCALabelSubDescriptors>");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_names() {
        assert_eq!(McaTagSymbol::L.tag_name(), "Left");
        assert_eq!(McaTagSymbol::Lfe.tag_name(), "LFE");
        assert_eq!(McaTagSymbol::Hi.tag_name(), "Hearing Impaired");
        assert_eq!(McaTagSymbol::Vi.tag_name(), "Visually Impaired");
    }

    #[test]
    fn test_symbol_strings() {
        assert_eq!(McaTagSymbol::L.symbol_string(), "chL");
        assert_eq!(McaTagSymbol::Lfe.symbol_string(), "chLFE");
        assert_eq!(McaTagSymbol::Hi.symbol_string(), "chHI");
        assert_eq!(McaTagSymbol::Vi.symbol_string(), "chVIN");
    }

    #[test]
    fn test_soundfield_stereo() {
        let sf = soundfield_stereo();
        assert_eq!(sf.name, "20");
        assert_eq!(sf.channels.len(), 2);
        assert_eq!(sf.channels[0].symbol, McaTagSymbol::L);
        assert_eq!(sf.channels[1].symbol, McaTagSymbol::R);
    }

    #[test]
    fn test_soundfield_51() {
        let sf = soundfield_51();
        assert_eq!(sf.name, "51");
        assert_eq!(sf.channels.len(), 6);
        assert_eq!(sf.channels[3].symbol, McaTagSymbol::Lfe);
    }

    #[test]
    fn test_soundfield_71() {
        let sf = soundfield_71();
        assert_eq!(sf.name, "71");
        assert_eq!(sf.channels.len(), 8);
        assert_eq!(sf.channels[6].symbol, McaTagSymbol::Lrs);
        assert_eq!(sf.channels[7].symbol, McaTagSymbol::Rrs);
    }

    #[test]
    fn test_soundfield_51_with_hi_vi() {
        let sf = soundfield_51_with_hi_vi();
        assert_eq!(sf.name, "51+HI+VI");
        assert_eq!(sf.channels.len(), 8);
        assert_eq!(sf.channels[6].symbol, McaTagSymbol::Hi);
        assert_eq!(sf.channels[7].symbol, McaTagSymbol::Vi);
    }

    #[test]
    fn test_detect_soundfield() {
        assert_eq!(detect_soundfield(1).name, "10");
        assert_eq!(detect_soundfield(1).channels.len(), 1);
        assert_eq!(detect_soundfield(2).name, "20");
        assert_eq!(detect_soundfield(6).name, "51");
        assert_eq!(detect_soundfield(8).name, "51+HI+VI");
        assert_eq!(detect_soundfield(4).name, "71");
    }

    #[test]
    fn test_generate_mca_xml_stereo() {
        let sf = soundfield_stereo();
        let xml = generate_mca_xml(&sf);
        assert!(xml.contains("sg20"));
        assert!(xml.contains("Soundfield 20"));
        assert!(xml.contains("<r0:MCAChannelID>1</r0:MCAChannelID>"));
        assert!(xml.contains("<r0:MCAChannelID>2</r0:MCAChannelID>"));
        assert!(xml.contains("<r0:MCATagSymbol>chL</r0:MCATagSymbol>"));
        assert!(xml.contains("<r0:MCATagSymbol>chR</r0:MCATagSymbol>"));
    }

    #[test]
    fn test_generate_mca_xml_with_language() {
        let mut sf = soundfield_stereo();
        sf.channels[0].spoken_language = "en".to_string();
        let xml = generate_mca_xml(&sf);
        assert!(xml.contains("<r0:RFC5646SpokenLanguage>en</r0:RFC5646SpokenLanguage>"));
    }

    #[test]
    fn test_soundfield_to_mca_config() {
        assert_eq!(
            soundfield_to_mca_config(&soundfield_51()).as_deref(),
            Some("51(L,R,C,LFE,Ls,Rs)")
        );
        // HI and VI-N sit outside the soundfield group as standalone channels.
        assert_eq!(
            soundfield_to_mca_config(&soundfield_51_with_hi_vi()).as_deref(),
            Some("51(L,R,C,LFE,Ls,Rs),HI,VIN")
        );
        // stereo has no group label in the DCP dictionary
        assert_eq!(
            soundfield_to_mca_config(&soundfield_stereo()).as_deref(),
            Some("L,R")
        );
    }

    #[test]
    fn test_soundfield_to_mca_config_rejects_unsupported() {
        // Lt/Rt have no ASDCP DCP label, so the whole soundfield is unmappable.
        let sf = McaSoundfield {
            name: "Lt/Rt".to_string(),
            channels: vec![label(McaTagSymbol::Lt, 0), label(McaTagSymbol::Rt, 1)],
        };
        assert_eq!(soundfield_to_mca_config(&sf), None);
    }

    #[test]
    fn test_channel_indices() {
        let sf = soundfield_51();
        for (i, ch) in sf.channels.iter().enumerate() {
            assert_eq!(ch.channel_index, i as u32);
        }
    }
}
