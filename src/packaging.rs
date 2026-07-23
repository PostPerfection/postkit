//! Shared DCP/IMF packaging XML writers: CPL, PKL, ASSETMAP.
//!
//! Both wizards hand-rolled these. PKL and ASSETMAP are genuinely one concept
//! across DCP and IMF (same element shape, only the namespace and issuer differ),
//! so they are single writers parametrised by namespace. The CPL is not: a DCP
//! CPL (ST 429-7, reels of MainPicture/MainSound) and an IMF CPL (ST 2067-3,
//! segments of virtual-track sequences) are different documents, so each has its
//! own writer. Writers return the XML string; the caller does the file I/O.

use std::fmt::Write as _;

/// Standard namespace URIs, kept in one place so the apps don't re-hardcode them.
pub mod ns {
    // DCP CPL
    pub const CPL_SMPTE: &str = "http://www.smpte-ra.org/schemas/429-7/2006/CPL";
    pub const CPL_INTEROP: &str = "http://www.digicine.com/PROTO-ASDCP-CPL-20040511#";
    // DCP PKL
    pub const PKL_SMPTE: &str = "http://www.smpte-ra.org/schemas/429-8/2007/PKL";
    pub const PKL_INTEROP: &str = "http://www.digicine.com/PROTO-ASDCP-PKL-20040311#";
    // DCP ASSETMAP (IMF uses the same AM namespace)
    pub const AM_SMPTE: &str = "http://www.smpte-ra.org/schemas/429-9/2007/AM";
    pub const AM_INTEROP: &str = "http://www.digicine.com/PROTO-ASDCP-AM-20040311#";
    // IMF (ST 2067)
    pub const CPL_IMF: &str = "http://www.smpte-ra.org/schemas/2067-3/2016";
    pub const CPL_IMF_CC: &str = "http://www.smpte-ra.org/schemas/2067-2/2016";
    pub const PKL_IMF: &str = "http://www.smpte-ra.org/schemas/2067-2/2016/PKL";
    pub const APP2E: &str = "http://www.smpte-ra.org/schemas/2067-21/2016";
}

/// Escape XML special characters in element text or attribute values.
///
/// The single escaper the packaging writers and xmldsig share, replacing the
/// per-app private copies. Covers all five predefined entities.
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ─── PKL (shared DCP/IMF concept) ──────────────────────────────────────────

/// One asset in a packing list.
#[derive(Debug, Clone, Default)]
pub struct PklAsset {
    /// Bare UUID (no `urn:uuid:` prefix).
    pub id: String,
    /// Base64 SHA-1 digest of the file.
    pub hash: String,
    pub size: u64,
    /// MIME type, e.g. `text/xml` or `application/mxf`.
    pub asset_type: String,
}

/// A Packing List (ST 429-8 DCP or ST 2067-2 IMF, selected by `namespace`).
#[derive(Debug, Clone, Default)]
pub struct PackingList {
    /// Bare UUID.
    pub uuid: String,
    pub namespace: String,
    pub issuer: String,
    pub creator: String,
    pub issue_date: String,
    /// Optional AnnotationText, emitted right after Id (schema position in both
    /// the ST 429-8 DCP and ST 2067-2 IMF PKL). None keeps output byte-identical.
    pub annotation: Option<String>,
    pub assets: Vec<PklAsset>,
}

impl PackingList {
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(xml, "<PackingList xmlns=\"{}\">", self.namespace);
        let _ = writeln!(xml, "  <Id>urn:uuid:{}</Id>", self.uuid);
        if let Some(a) = &self.annotation {
            let _ = writeln!(xml, "  <AnnotationText>{}</AnnotationText>", escape_xml(a));
        }
        let _ = writeln!(xml, "  <IssueDate>{}</IssueDate>", self.issue_date);
        let _ = writeln!(xml, "  <Issuer>{}</Issuer>", escape_xml(&self.issuer));
        let _ = writeln!(xml, "  <Creator>{}</Creator>", escape_xml(&self.creator));
        xml.push_str("  <AssetList>\n");
        for a in &self.assets {
            xml.push_str("    <Asset>\n");
            let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", a.id);
            let _ = writeln!(xml, "      <Hash>{}</Hash>", a.hash);
            let _ = writeln!(xml, "      <Size>{}</Size>", a.size);
            let _ = writeln!(xml, "      <Type>{}</Type>", escape_xml(&a.asset_type));
            xml.push_str("    </Asset>\n");
        }
        xml.push_str("  </AssetList>\n");
        xml.push_str("</PackingList>\n");
        xml
    }
}

// ─── ASSETMAP (shared DCP/IMF concept) ─────────────────────────────────────

/// One asset in an ASSETMAP.
#[derive(Debug, Clone, Default)]
pub struct AssetMapAsset {
    /// Bare UUID.
    pub id: String,
    /// Chunk path relative to the package root.
    pub path: String,
    /// Marks the PKL entry (`<PackingList>true</PackingList>`).
    pub packing_list: bool,
}

/// An ASSETMAP (ST 429-9). VolumeCount is mandatory in both the SMPTE and
/// Interop AM schemas, so it is always emitted (single-volume: 1).
///
/// SMPTE 429-9 and Interop order the metadata block differently (SMPTE:
/// Creator, VolumeCount, IssueDate, Issuer; Interop: VolumeCount, IssueDate,
/// Issuer, Creator), so `to_xml` picks the order from the namespace.
#[derive(Debug, Clone, Default)]
pub struct AssetMap {
    /// Bare UUID.
    pub uuid: String,
    pub namespace: String,
    pub issuer: String,
    pub creator: String,
    pub issue_date: String,
    /// Optional AnnotationText, emitted right after Id (schema position in both
    /// the SMPTE 429-9 and Interop AM). None keeps output byte-identical.
    pub annotation: Option<String>,
    pub assets: Vec<AssetMapAsset>,
}

impl AssetMap {
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(xml, "<AssetMap xmlns=\"{}\">", self.namespace);
        let _ = writeln!(xml, "  <Id>urn:uuid:{}</Id>", self.uuid);
        if let Some(a) = &self.annotation {
            let _ = writeln!(xml, "  <AnnotationText>{}</AnnotationText>", escape_xml(a));
        }
        let creator = format!("  <Creator>{}</Creator>\n", escape_xml(&self.creator));
        let volume_count = "  <VolumeCount>1</VolumeCount>\n";
        let issue_date = format!("  <IssueDate>{}</IssueDate>\n", self.issue_date);
        let issuer = format!("  <Issuer>{}</Issuer>\n", escape_xml(&self.issuer));
        if self.namespace == ns::AM_INTEROP {
            xml.push_str(volume_count);
            xml.push_str(&issue_date);
            xml.push_str(&issuer);
            xml.push_str(&creator);
        } else {
            xml.push_str(&creator);
            xml.push_str(volume_count);
            xml.push_str(&issue_date);
            xml.push_str(&issuer);
        }
        xml.push_str("  <AssetList>\n");
        for a in &self.assets {
            xml.push_str("    <Asset>\n");
            let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", a.id);
            if a.packing_list {
                xml.push_str("      <PackingList>true</PackingList>\n");
            }
            xml.push_str("      <ChunkList>\n");
            xml.push_str("        <Chunk>\n");
            let _ = writeln!(xml, "          <Path>{}</Path>", escape_xml(&a.path));
            xml.push_str("        </Chunk>\n");
            xml.push_str("      </ChunkList>\n");
            xml.push_str("    </Asset>\n");
        }
        xml.push_str("  </AssetList>\n");
        xml.push_str("</AssetMap>\n");
        xml
    }
}

/// VOLINDEX document paired with a DCP ASSETMAP.
pub fn volindex_xml(namespace: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<VolumeIndex xmlns=\"{namespace}\">\n  <Index>1</Index>\n</VolumeIndex>\n"
    )
}

// ─── DCP CPL (ST 429-7) ────────────────────────────────────────────────────

/// One reel of a DCP CPL: a MainPicture and an optional MainSound.
#[derive(Debug, Clone, Default)]
pub struct DcpCplReel {
    /// Bare UUIDs.
    pub reel_id: String,
    pub picture_id: String,
    pub picture_edit_rate_num: u32,
    pub picture_edit_rate_den: u32,
    pub picture_duration: u64,
    pub picture_entry_point: u64,
    /// Picture container dimensions, e.g. 1998x1080 (flat) or 2048x858 (scope).
    /// SMPTE writes them as the Rational ScreenAspectRatio; Interop derives a
    /// decimal ratio from them.
    pub picture_width: u32,
    pub picture_height: u32,
    /// KeyId (bare UUID) when the picture essence is encrypted.
    pub picture_key_id: Option<String>,
    pub sound_id: Option<String>,
    pub sound_edit_rate_num: u32,
    pub sound_edit_rate_den: u32,
    pub sound_duration: u64,
    pub sound_entry_point: u64,
    /// KeyId (bare UUID) when the sound essence is encrypted.
    pub sound_key_id: Option<String>,
}

/// A DCP Composition Playlist (ST 429-7 SMPTE or Interop, by `namespace`).
#[derive(Debug, Clone, Default)]
pub struct DcpCpl {
    /// Bare UUID.
    pub uuid: String,
    pub namespace: String,
    pub title: String,
    pub content_kind: String,
    pub issuer: String,
    pub creator: String,
    pub issue_date: String,
    pub reels: Vec<DcpCplReel>,
}

/// Interop ScreenAspectRatio decimal from container dims. Rounding to two
/// places snaps to the canonical DCI ratios (1998x1080 -> 1.85, 2048x858 ->
/// 2.39) without a lookup table.
fn screen_aspect_decimal(width: u32, height: u32) -> String {
    assert!(height != 0, "picture_height must be set for an Interop CPL");
    format!("{:.2}", width as f64 / height as f64)
}

impl DcpCpl {
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(xml, "<CompositionPlaylist xmlns=\"{}\">", self.namespace);
        // ST 429-7 element order: Id, IssueDate, Issuer, Creator,
        // ContentTitleText, ContentKind, ContentVersion, RatingList.
        let _ = writeln!(xml, "  <Id>urn:uuid:{}</Id>", self.uuid);
        let _ = writeln!(xml, "  <IssueDate>{}</IssueDate>", self.issue_date);
        let _ = writeln!(xml, "  <Issuer>{}</Issuer>", escape_xml(&self.issuer));
        let _ = writeln!(xml, "  <Creator>{}</Creator>", escape_xml(&self.creator));
        let _ = writeln!(
            xml,
            "  <ContentTitleText>{}</ContentTitleText>",
            escape_xml(&self.title)
        );
        // ContentKind is required; default to "feature" when the caller omits it.
        let content_kind = if self.content_kind.is_empty() {
            "feature"
        } else {
            &self.content_kind
        };
        let _ = writeln!(
            xml,
            "  <ContentKind>{}</ContentKind>",
            escape_xml(content_kind)
        );
        // ContentVersion (required by SMPTE) and an empty RatingList (required by
        // both), synthesized from the CPL id and title.
        xml.push_str("  <ContentVersion>\n");
        let _ = writeln!(xml, "    <Id>urn:uuid:{}</Id>", self.uuid);
        let _ = writeln!(
            xml,
            "    <LabelText>{}</LabelText>",
            escape_xml(&self.title)
        );
        xml.push_str("  </ContentVersion>\n");
        xml.push_str("  <RatingList/>\n");
        xml.push_str("  <ReelList>\n");
        for (i, reel) in self.reels.iter().enumerate() {
            xml.push_str("    <Reel>\n");
            let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", reel.reel_id);
            let _ = writeln!(xml, "      <AnnotationText>Reel {}</AnnotationText>", i + 1);
            xml.push_str("      <AssetList>\n");
            xml.push_str("        <MainPicture>\n");
            let _ = writeln!(xml, "          <Id>urn:uuid:{}</Id>", reel.picture_id);
            let _ = writeln!(
                xml,
                "          <EditRate>{} {}</EditRate>",
                reel.picture_edit_rate_num, reel.picture_edit_rate_den
            );
            let _ = writeln!(
                xml,
                "          <IntrinsicDuration>{}</IntrinsicDuration>",
                reel.picture_duration
            );
            let _ = writeln!(
                xml,
                "          <EntryPoint>{}</EntryPoint>",
                reel.picture_entry_point
            );
            let _ = writeln!(
                xml,
                "          <Duration>{}</Duration>",
                reel.picture_duration
            );
            if let Some(ref key_id) = reel.picture_key_id {
                let _ = writeln!(xml, "          <KeyId>urn:uuid:{key_id}</KeyId>");
            }
            let _ = writeln!(
                xml,
                "          <FrameRate>{} {}</FrameRate>",
                reel.picture_edit_rate_num, reel.picture_edit_rate_den
            );
            // SMPTE ScreenAspectRatio is a Rational (container dims); Interop is
            // a decimal (e.g. 1.85, 2.39) derived from those dims.
            if self.namespace == ns::CPL_INTEROP {
                let _ = writeln!(
                    xml,
                    "          <ScreenAspectRatio>{}</ScreenAspectRatio>",
                    screen_aspect_decimal(reel.picture_width, reel.picture_height)
                );
            } else {
                let _ = writeln!(
                    xml,
                    "          <ScreenAspectRatio>{} {}</ScreenAspectRatio>",
                    reel.picture_width, reel.picture_height
                );
            }
            xml.push_str("        </MainPicture>\n");
            if let Some(ref sound_id) = reel.sound_id {
                xml.push_str("        <MainSound>\n");
                let _ = writeln!(xml, "          <Id>urn:uuid:{sound_id}</Id>");
                let _ = writeln!(
                    xml,
                    "          <EditRate>{} {}</EditRate>",
                    reel.sound_edit_rate_num, reel.sound_edit_rate_den
                );
                let _ = writeln!(
                    xml,
                    "          <IntrinsicDuration>{}</IntrinsicDuration>",
                    reel.sound_duration
                );
                let _ = writeln!(
                    xml,
                    "          <EntryPoint>{}</EntryPoint>",
                    reel.sound_entry_point
                );
                let _ = writeln!(
                    xml,
                    "          <Duration>{}</Duration>",
                    reel.sound_duration
                );
                if let Some(ref key_id) = reel.sound_key_id {
                    let _ = writeln!(xml, "          <KeyId>urn:uuid:{key_id}</KeyId>");
                }
                xml.push_str("        </MainSound>\n");
            }
            xml.push_str("      </AssetList>\n");
            xml.push_str("    </Reel>\n");
        }
        xml.push_str("  </ReelList>\n");
        xml.push_str("</CompositionPlaylist>\n");
        xml
    }
}

// ─── IMF CPL (ST 2067-3) ───────────────────────────────────────────────────

/// Essence kind of an IMF virtual-track resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImfTrackKind {
    Image,
    Audio,
    Subtitle,
}

impl ImfTrackKind {
    fn sequence_element(self) -> &'static str {
        match self {
            ImfTrackKind::Image => "MainImageSequence",
            ImfTrackKind::Audio => "MainAudioSequence",
            ImfTrackKind::Subtitle => "SubtitlesSequence",
        }
    }
}

/// One resource of an IMF CPL, pointing at a track file.
#[derive(Debug, Clone)]
pub struct ImfResource {
    /// Bare UUID of the referenced track file.
    pub track_file_uuid: String,
    pub duration: u64,
    pub kind: ImfTrackKind,
    /// Bare UUID linking this resource to an EssenceDescriptorList entry
    /// (ST 2067-2 SourceEncoding). None keeps the resource unchanged.
    pub source_encoding: Option<String>,
}

/// One entry of an IMF CPL EssenceDescriptorList (ST 2067-3): an Id matched by a
/// resource's SourceEncoding, plus the essence descriptor body. The body is the
/// MXF header-metadata serialisation in its own reg/335, reg/2003 and reg/395
/// namespaces (RGBADescriptor for image color/HDR-WCG, WAVEPCMDescriptor + MCA
/// subdescriptors for audio soundfield and per-track RFC 5646 spoken language).
/// postkit carries this verbatim; it does not synthesise the UL-coded descriptor
/// internals (that data comes from the wrapped MXF via asdcplib).
#[derive(Debug, Clone)]
pub struct ImfEssenceDescriptor {
    /// Bare UUID, referenced by an `ImfResource::source_encoding`.
    pub id: String,
    /// Descriptor body XML placed inside `<EssenceDescriptor>`, already namespaced.
    pub body: String,
}

/// An IMF Composition Playlist (ST 2067-3, App #2E).
#[derive(Debug, Clone, Default)]
pub struct ImfCpl {
    /// Bare UUID.
    pub uuid: String,
    pub title: String,
    /// Defaults to "feature" when empty.
    pub content_kind: String,
    pub issuer: String,
    pub creator: String,
    pub issue_date: String,
    pub fps_num: u32,
    pub fps_den: u32,
    pub resources: Vec<ImfResource>,
    /// Composition languages (RFC 5646) written as a ST 2067-3 LocaleList. Empty
    /// keeps the output unchanged.
    pub languages: Vec<String>,
    /// Essence descriptors carrying per-track audio MCA/soundfield/language
    /// labelling and image color/HDR-WCG metadata. Empty keeps output unchanged.
    pub essence_descriptors: Vec<ImfEssenceDescriptor>,
}

impl ImfCpl {
    pub fn to_xml(&self) -> String {
        let fps_num = self.fps_num;
        let fps_den = if self.fps_den == 0 { 1 } else { self.fps_den };
        let content_kind = if self.content_kind.is_empty() {
            "feature"
        } else {
            &self.content_kind
        };

        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(
            xml,
            "<CompositionPlaylist xmlns=\"{}\" xmlns:cc=\"{}\">",
            ns::CPL_IMF,
            ns::CPL_IMF_CC
        );
        let _ = writeln!(xml, "  <Id>urn:uuid:{}</Id>", self.uuid);
        let _ = writeln!(xml, "  <IssueDate>{}</IssueDate>", self.issue_date);
        let _ = writeln!(xml, "  <Issuer>{}</Issuer>", escape_xml(&self.issuer));
        let _ = writeln!(xml, "  <Creator>{}</Creator>", escape_xml(&self.creator));
        let _ = writeln!(
            xml,
            "  <ContentTitle>{}</ContentTitle>",
            escape_xml(&self.title)
        );
        let _ = writeln!(xml, "  <ContentKind>{content_kind}</ContentKind>");
        // EssenceDescriptorList precedes EditRate per ST 2067-3 element order.
        if !self.essence_descriptors.is_empty() {
            xml.push_str("  <EssenceDescriptorList>\n");
            for d in &self.essence_descriptors {
                xml.push_str("    <EssenceDescriptor>\n");
                let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", d.id);
                // body is caller-supplied, already-namespaced mxf descriptor xml
                xml.push_str(d.body.trim_end());
                xml.push('\n');
                xml.push_str("    </EssenceDescriptor>\n");
            }
            xml.push_str("  </EssenceDescriptorList>\n");
        }
        let _ = writeln!(xml, "  <EditRate>{fps_num} {fps_den}</EditRate>");
        // LocaleList follows EditRate and precedes ExtensionProperties.
        if !self.languages.is_empty() {
            xml.push_str("  <LocaleList>\n    <Locale>\n      <LanguageList>\n");
            for l in &self.languages {
                let _ = writeln!(xml, "        <Language>{}</Language>", escape_xml(l));
            }
            xml.push_str("      </LanguageList>\n    </Locale>\n  </LocaleList>\n");
        }
        xml.push_str("  <ExtensionProperties>\n");
        let _ = writeln!(
            xml,
            "    <cc:ApplicationIdentification>{}</cc:ApplicationIdentification>",
            ns::APP2E
        );
        xml.push_str("  </ExtensionProperties>\n");
        xml.push_str("  <SegmentList>\n");
        xml.push_str("    <Segment>\n");
        let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", uuid::Uuid::new_v4());
        xml.push_str("      <SequenceList>\n");

        // one virtual track per essence kind, image then audio then subtitle
        for kind in [
            ImfTrackKind::Image,
            ImfTrackKind::Audio,
            ImfTrackKind::Subtitle,
        ] {
            for r in self.resources.iter().filter(|r| r.kind == kind) {
                self.write_sequence(&mut xml, r, fps_num, fps_den);
            }
        }

        xml.push_str("      </SequenceList>\n");
        xml.push_str("    </Segment>\n");
        xml.push_str("  </SegmentList>\n");
        xml.push_str("</CompositionPlaylist>\n");
        xml
    }

    fn write_sequence(&self, xml: &mut String, r: &ImfResource, fps_num: u32, fps_den: u32) {
        let el = r.kind.sequence_element();
        let _ = writeln!(xml, "        <cc:{el} xmlns:cc=\"{}\">", ns::CPL_IMF_CC);
        let _ = writeln!(xml, "          <Id>urn:uuid:{}</Id>", uuid::Uuid::new_v4());
        let _ = writeln!(
            xml,
            "          <TrackId>urn:uuid:{}</TrackId>",
            uuid::Uuid::new_v4()
        );
        let _ = writeln!(xml, "          <EditRate>{fps_num} {fps_den}</EditRate>");
        xml.push_str("          <ResourceList>\n");
        xml.push_str("            <Resource>\n");
        let _ = writeln!(
            xml,
            "              <Id>urn:uuid:{}</Id>",
            uuid::Uuid::new_v4()
        );
        // SourceEncoding precedes TrackFileId per ST 2067-2 TrackFileResourceType.
        if let Some(se) = &r.source_encoding {
            let _ = writeln!(
                xml,
                "              <SourceEncoding>urn:uuid:{se}</SourceEncoding>"
            );
        }
        let _ = writeln!(
            xml,
            "              <TrackFileId>urn:uuid:{}</TrackFileId>",
            r.track_file_uuid
        );
        let _ = writeln!(
            xml,
            "              <EditRate>{fps_num} {fps_den}</EditRate>"
        );
        let _ = writeln!(
            xml,
            "              <IntrinsicDuration>{}</IntrinsicDuration>",
            r.duration
        );
        let _ = writeln!(
            xml,
            "              <SourceDuration>{}</SourceDuration>",
            r.duration
        );
        xml.push_str("            </Resource>\n");
        xml.push_str("          </ResourceList>\n");
        let _ = writeln!(xml, "        </cc:{el}>");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Validate the generated SMPTE DCP docs against the official XSDs. Gated on
    // POSTKIT_DCP_XSD_DIR (a dir holding the SMPTE 429-7/8/9 schemas plus a local
    // xmldsig-core-schema.xsd and xml.xsd) and xmllint; skips when absent.
    #[test]
    fn generated_dcp_docs_pass_xmllint_schema() {
        let Ok(xsd_dir) = std::env::var("POSTKIT_DCP_XSD_DIR") else {
            eprintln!("skipping: set POSTKIT_DCP_XSD_DIR to the SMPTE XSD directory");
            return;
        };
        if std::process::Command::new("xmllint")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: xmllint not installed");
            return;
        }
        let xsd = std::path::Path::new(&xsd_dir);
        let dir = tempfile::tempdir().unwrap();

        // xmllint resolves the CPLs' ds:Signature, xml.xsd, and (Interop only)
        // the 437-Y stereo-picture import (all declared by http URL) through this
        // catalog to the local copies.
        let catalog = dir.path().join("catalog.xml");
        let dsig = xsd.join("xmldsig-core-schema.xsd");
        let xml_xsd = xsd.join("xml.xsd");
        let stereo = xsd.join("437-Y-2007-Main-Stereo-Picture-CPL.xsd");
        let interop_cpl = xsd.join("PROTO-ASDCP-CPL-20040511.xsd");
        std::fs::write(
            &catalog,
            format!(
                r#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <system systemId="http://www.w3.org/TR/2002/REC-xmldsig-core-20020212/xmldsig-core-schema.xsd" uri="{dsig}"/>
  <system systemId="http://www.w3.org/2001/03/xml.xsd" uri="{xml_xsd}"/>
  <system systemId="http://www.digicine.com/schemas/437-Y/2007/Main-Stereo-Picture-CPL.xsd" uri="{stereo}"/>
  <public publicId="http://www.digicine.com/schemas/437-Y/2007/Main-Stereo-Picture-CPL" uri="{stereo}"/>
  <system systemId="http://www.digicine.com/PROTO-ASDCP-CPL-20040511.xsd" uri="{interop_cpl}"/>
  <public publicId="http://www.digicine.com/PROTO-ASDCP-CPL-20040511#" uri="{interop_cpl}"/>
</catalog>"#,
                dsig = dsig.display(),
                xml_xsd = xml_xsd.display(),
                stereo = stereo.display(),
                interop_cpl = interop_cpl.display(),
            ),
        )
        .unwrap();

        let uuid_a = "aaaaaaaa-7777-8888-9999-aaaaaaaaaaaa";
        let am = AssetMap {
            uuid: uuid_a.into(),
            namespace: ns::AM_SMPTE.into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: vec![AssetMapAsset {
                id: "99999999-7777-8888-9999-aaaaaaaaaaaa".into(),
                path: "PKL.xml".into(),
                packing_list: true,
            }],
        };
        // IMF ASSETMAP uses the same 429-9 AM schema and must also carry
        // VolumeCount (the old include_volume_count=false path was schema-invalid).
        let imf_am = AssetMap {
            uuid: "dddddddd-7777-8888-9999-aaaaaaaaaaaa".into(),
            namespace: ns::AM_SMPTE.into(),
            issuer: "IMF Wizard".into(),
            creator: "IMF Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: vec![AssetMapAsset {
                id: "eeeeeeee-7777-8888-9999-aaaaaaaaaaaa".into(),
                path: "PKL.xml".into(),
                packing_list: true,
            }],
        };
        let pkl = PackingList {
            uuid: "bbbbbbbb-7777-8888-9999-aaaaaaaaaaaa".into(),
            namespace: ns::PKL_SMPTE.into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: vec![PklAsset {
                id: "cccccccc-7777-8888-9999-aaaaaaaaaaaa".into(),
                hash: "kO0m3F3qX3qg3n3qg3n3qg3n3q0=".into(),
                size: 42,
                asset_type: "application/mxf".into(),
            }],
        };
        let cpl = DcpCpl {
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            namespace: ns::CPL_SMPTE.into(),
            title: "Test".into(),
            content_kind: "feature".into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            reels: vec![DcpCplReel {
                reel_id: "66666666-7777-8888-9999-aaaaaaaaaaaa".into(),
                picture_id: "77777777-7777-8888-9999-aaaaaaaaaaaa".into(),
                picture_edit_rate_num: 24,
                picture_edit_rate_den: 1,
                picture_duration: 240,
                picture_width: 1998,
                picture_height: 1080,
                sound_id: Some("88888888-7777-8888-9999-aaaaaaaaaaaa".into()),
                sound_edit_rate_num: 24,
                sound_edit_rate_den: 1,
                sound_duration: 240,
                ..Default::default()
            }],
        };
        // Interop CPL: scope container (2048x858) exercises the decimal
        // ScreenAspectRatio (-> 2.39) that the Interop schema requires.
        let interop_cpl_doc = DcpCpl {
            uuid: "22222222-2222-3333-4444-555555555555".into(),
            namespace: ns::CPL_INTEROP.into(),
            title: "Test".into(),
            content_kind: "feature".into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            reels: vec![DcpCplReel {
                reel_id: "66666666-7777-8888-9999-aaaaaaaaaaaa".into(),
                picture_id: "77777777-7777-8888-9999-aaaaaaaaaaaa".into(),
                picture_edit_rate_num: 24,
                picture_edit_rate_den: 1,
                picture_duration: 240,
                picture_width: 2048,
                picture_height: 858,
                sound_id: Some("88888888-7777-8888-9999-aaaaaaaaaaaa".into()),
                sound_edit_rate_num: 24,
                sound_edit_rate_den: 1,
                sound_duration: 240,
                ..Default::default()
            }],
        };
        assert!(
            interop_cpl_doc
                .to_xml()
                .contains("<ScreenAspectRatio>2.39</ScreenAspectRatio>")
        );

        // annotated PKL/ASSETMAP: AnnotationText must validate in its post-Id slot.
        let mut pkl_annotated = pkl.clone();
        pkl_annotated.annotation = Some("Feature reel 1 & 2".into());
        let mut am_annotated = am.clone();
        am_annotated.annotation = Some("Combined: A & B".into());

        for (doc, schema, xml) in [
            ("ASSETMAP", "SMPTE-429-9-2007-AM.xsd", am.to_xml()),
            (
                "ASSETMAP_ANNOTATED",
                "SMPTE-429-9-2007-AM.xsd",
                am_annotated.to_xml(),
            ),
            ("IMF_ASSETMAP", "SMPTE-429-9-2007-AM.xsd", imf_am.to_xml()),
            ("PKL", "SMPTE-429-8-2006-PKL.xsd", pkl.to_xml()),
            (
                "PKL_ANNOTATED",
                "SMPTE-429-8-2006-PKL.xsd",
                pkl_annotated.to_xml(),
            ),
            ("CPL", "SMPTE-429-7-2006-CPL.xsd", cpl.to_xml()),
            (
                "INTEROP_CPL",
                "PROTO-ASDCP-CPL-20040511.xsd",
                interop_cpl_doc.to_xml(),
            ),
        ] {
            let path = dir.path().join(format!("{doc}.xml"));
            std::fs::write(&path, &xml).unwrap();
            let out = std::process::Command::new("xmllint")
                .args(["--nonet", "--noout", "--schema"])
                .arg(xsd.join(schema))
                .arg(&path)
                .env("XML_CATALOG_FILES", &catalog)
                .output()
                .expect("run xmllint");
            assert!(
                out.status.success(),
                "{doc} must pass its SMPTE XSD:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn screen_aspect_decimal_snaps_to_dci_ratios() {
        assert_eq!(screen_aspect_decimal(1998, 1080), "1.85");
        assert_eq!(screen_aspect_decimal(2048, 858), "2.39");
        assert_eq!(screen_aspect_decimal(2048, 1080), "1.90");
    }

    #[test]
    fn escape_covers_all_five_entities() {
        assert_eq!(escape_xml("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
    }

    #[test]
    fn pkl_writes_assets_and_namespace() {
        let pkl = PackingList {
            uuid: "pkl".into(),
            namespace: ns::PKL_SMPTE.into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: vec![PklAsset {
                id: "asset".into(),
                hash: "base64hash".into(),
                size: 42,
                asset_type: "application/mxf".into(),
            }],
        };
        let xml = pkl.to_xml();
        assert!(xml.contains("429-8/2007/PKL"));
        assert!(xml.contains("<Id>urn:uuid:pkl</Id>"));
        assert!(xml.contains("<Hash>base64hash</Hash>"));
        assert!(xml.contains("<Size>42</Size>"));
        assert!(xml.contains("<Type>application/mxf</Type>"));
    }

    #[test]
    fn pkl_annotation_is_optional_and_positioned_after_id() {
        let base = PackingList {
            uuid: "pkl".into(),
            namespace: ns::PKL_SMPTE.into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: vec![],
        };
        // None keeps the pre-field output: no AnnotationText anywhere.
        let none = base.to_xml();
        assert!(!none.contains("<AnnotationText>"));

        // Some emits an escaped AnnotationText between Id and IssueDate; the rest
        // of the document is byte-identical to the None output (proof: dropping the
        // one inserted line reproduces `none`).
        let mut annotated = base.clone();
        annotated.annotation = Some("A & B <combo>".into());
        let some = annotated.to_xml();
        assert!(some.contains("<AnnotationText>A &amp; B &lt;combo&gt;</AnnotationText>"));
        assert!(some.find("</Id>").unwrap() < some.find("<AnnotationText>").unwrap());
        assert!(some.find("<AnnotationText>").unwrap() < some.find("<IssueDate>").unwrap());
        let stripped: String = some
            .lines()
            .filter(|l| !l.contains("<AnnotationText>"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(stripped, none, "None output must be byte-identical");
    }

    #[test]
    fn assetmap_annotation_is_optional_and_positioned_after_id() {
        let base = AssetMap {
            uuid: "am".into(),
            namespace: ns::AM_SMPTE.into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: vec![],
        };
        let none = base.to_xml();
        assert!(!none.contains("<AnnotationText>"));

        let mut annotated = base.clone();
        annotated.annotation = Some("Merged: X & Y".into());
        let some = annotated.to_xml();
        assert!(some.contains("<AnnotationText>Merged: X &amp; Y</AnnotationText>"));
        // after Id, before the Creator/VolumeCount metadata block
        assert!(some.find("</Id>").unwrap() < some.find("<AnnotationText>").unwrap());
        assert!(some.find("<AnnotationText>").unwrap() < some.find("<Creator>").unwrap());
        let stripped: String = some
            .lines()
            .filter(|l| !l.contains("<AnnotationText>"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(stripped, none, "None output must be byte-identical");
    }

    #[test]
    fn assetmap_always_emits_volume_count() {
        // ST 429-9 (and Interop AM) make VolumeCount mandatory, so every
        // ASSETMAP carries it regardless of DCP vs IMF.
        let assets = vec![AssetMapAsset {
            id: "pkl".into(),
            path: "PKL.xml".into(),
            packing_list: true,
        }];
        let smpte = AssetMap {
            uuid: "am".into(),
            namespace: ns::AM_SMPTE.into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets: assets.clone(),
        }
        .to_xml();
        assert!(smpte.contains("<VolumeCount>1</VolumeCount>"));
        assert!(smpte.contains("<IssueDate>2024-01-01T00:00:00+00:00</IssueDate>"));
        assert!(smpte.contains("<Issuer>DCP Wizard</Issuer>"));
        assert!(smpte.contains("<PackingList>true</PackingList>"));
        assert!(smpte.contains("<Path>PKL.xml</Path>"));

        let interop = AssetMap {
            uuid: "am".into(),
            namespace: ns::AM_INTEROP.into(),
            issuer: "IMF Wizard".into(),
            creator: "IMF Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            annotation: None,
            assets,
        }
        .to_xml();
        // Interop orders VolumeCount before IssueDate; it must still be present.
        assert!(interop.contains("<VolumeCount>1</VolumeCount>"));
        assert!(interop.find("<VolumeCount>").unwrap() < interop.find("<IssueDate>").unwrap());
    }

    #[test]
    fn volindex_carries_namespace() {
        let vi = volindex_xml(ns::AM_SMPTE);
        assert!(
            vi.contains("<VolumeIndex xmlns=\"http://www.smpte-ra.org/schemas/429-9/2007/AM\">")
        );
        assert!(vi.contains("<Index>1</Index>"));
    }

    #[test]
    fn dcp_cpl_writes_reel_with_picture_and_sound() {
        let cpl = DcpCpl {
            uuid: "cpl".into(),
            namespace: ns::CPL_SMPTE.into(),
            title: "A & B".into(),
            content_kind: "feature".into(),
            issuer: "DCP Wizard".into(),
            creator: "DCP Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            reels: vec![DcpCplReel {
                reel_id: "reel".into(),
                picture_id: "pic".into(),
                picture_edit_rate_num: 24,
                picture_edit_rate_den: 1,
                picture_duration: 240,
                picture_entry_point: 0,
                picture_width: 1998,
                picture_height: 1080,
                picture_key_id: Some("pic-key".into()),
                sound_id: Some("snd".into()),
                sound_edit_rate_num: 24,
                sound_edit_rate_den: 1,
                sound_duration: 240,
                sound_entry_point: 0,
                sound_key_id: None,
            }],
        };
        let xml = cpl.to_xml();
        assert!(xml.contains("429-7/2006/CPL"));
        // SMPTE keeps the Rational (container dims) form.
        assert!(xml.contains("<ScreenAspectRatio>1998 1080</ScreenAspectRatio>"));
        assert!(xml.contains("<ContentTitleText>A &amp; B</ContentTitleText>"));
        // ST 429-7 order: IssueDate precedes ContentTitleText, which precedes ContentKind.
        assert!(xml.find("<IssueDate>").unwrap() < xml.find("<ContentTitleText>").unwrap());
        assert!(xml.find("<ContentTitleText>").unwrap() < xml.find("<ContentKind>").unwrap());
        assert!(xml.contains("<ContentVersion>"));
        assert!(xml.contains("<RatingList/>"));
        assert!(xml.contains("<MainPicture>"));
        assert!(xml.contains("<Id>urn:uuid:pic</Id>"));
        assert!(xml.contains("<MainSound>"));
        assert!(xml.contains("<Id>urn:uuid:snd</Id>"));
        assert!(xml.contains("<FrameRate>24 1</FrameRate>"));
        // encrypted picture carries its KeyId; unencrypted sound does not
        assert!(xml.contains("<KeyId>urn:uuid:pic-key</KeyId>"));
        assert_eq!(xml.matches("<KeyId>").count(), 1);
    }

    #[test]
    fn dcp_cpl_omits_sound_when_absent() {
        let cpl = DcpCpl {
            uuid: "cpl".into(),
            namespace: ns::CPL_INTEROP.into(),
            reels: vec![DcpCplReel {
                reel_id: "reel".into(),
                picture_id: "pic".into(),
                picture_edit_rate_num: 24,
                picture_edit_rate_den: 1,
                picture_width: 1998,
                picture_height: 1080,
                sound_id: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let xml = cpl.to_xml();
        assert!(xml.contains("PROTO-ASDCP-CPL-20040511"));
        assert!(!xml.contains("<MainSound>"));
        // Interop emits the decimal ratio, not the Rational pair.
        assert!(xml.contains("<ScreenAspectRatio>1.85</ScreenAspectRatio>"));
    }

    #[test]
    fn imf_cpl_orders_sequences_and_identifies_app2e() {
        let cpl = ImfCpl {
            uuid: "cpl".into(),
            title: "Test".into(),
            content_kind: String::new(),
            issuer: "IMF Wizard".into(),
            creator: "IMF Wizard".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            fps_num: 24,
            fps_den: 1,
            resources: vec![
                ImfResource {
                    track_file_uuid: "aud".into(),
                    duration: 240,
                    kind: ImfTrackKind::Audio,
                    source_encoding: None,
                },
                ImfResource {
                    track_file_uuid: "vid".into(),
                    duration: 240,
                    kind: ImfTrackKind::Image,
                    source_encoding: None,
                },
            ],
            languages: vec![],
            essence_descriptors: vec![],
        };
        let xml = cpl.to_xml();
        assert!(xml.contains(ns::APP2E));
        assert!(xml.contains("<ContentKind>feature</ContentKind>"));
        assert!(xml.contains("MainImageSequence"));
        assert!(xml.contains("MainAudioSequence"));
        assert!(xml.contains("<TrackFileId>urn:uuid:vid</TrackFileId>"));
        // image sequence must precede audio sequence
        let img = xml.find("MainImageSequence").unwrap();
        let aud = xml.find("MainAudioSequence").unwrap();
        assert!(img < aud, "image track should be written before audio");
    }

    /// Compact but structurally real audio essence descriptor body: a
    /// WAVEPCMDescriptor with a SoundfieldGroupLabelSubDescriptor and an
    /// AudioChannelLabelSubDescriptor carrying an RFC 5646 spoken language.
    /// Namespaces mirror a real ST 2067-2 EssenceDescriptorList.
    fn sample_audio_descriptor_body(lang: &str) -> String {
        format!(
            r#"<r0:WAVEPCMDescriptor xmlns:r0="http://www.smpte-ra.org/reg/395/2014/13/1/aaf" xmlns:r1="http://www.smpte-ra.org/reg/335/2012">
        <r1:ChannelCount>2</r1:ChannelCount>
        <r1:SubDescriptors>
          <r0:SoundfieldGroupLabelSubDescriptor>
            <r1:MCATagSymbol>sg51</r1:MCATagSymbol>
            <r1:RFC5646SpokenLanguage>{lang}</r1:RFC5646SpokenLanguage>
          </r0:SoundfieldGroupLabelSubDescriptor>
          <r0:AudioChannelLabelSubDescriptor>
            <r1:MCAChannelID>1</r1:MCAChannelID>
            <r1:MCATagSymbol>chVIN</r1:MCATagSymbol>
            <r1:MCATagName>Visually Impaired</r1:MCATagName>
            <r1:RFC5646SpokenLanguage>{lang}</r1:RFC5646SpokenLanguage>
          </r0:AudioChannelLabelSubDescriptor>
        </r1:SubDescriptors>
      </r0:WAVEPCMDescriptor>"#
        )
    }

    #[test]
    fn imf_cpl_writes_locale_list() {
        let cpl = ImfCpl {
            uuid: "cpl".into(),
            fps_num: 24,
            fps_den: 1,
            languages: vec!["de-DE".into(), "en-US".into()],
            ..Default::default()
        };
        let xml = cpl.to_xml();
        assert!(xml.contains("<LocaleList>"));
        assert!(xml.contains("<Language>de-DE</Language>"));
        assert!(xml.contains("<Language>en-US</Language>"));
        // LocaleList follows EditRate and precedes ExtensionProperties.
        assert!(xml.find("<EditRate>").unwrap() < xml.find("<LocaleList>").unwrap());
        assert!(xml.find("<LocaleList>").unwrap() < xml.find("<ExtensionProperties>").unwrap());
        // empty languages keep the LocaleList out entirely
        let plain = ImfCpl {
            uuid: "cpl".into(),
            fps_num: 24,
            fps_den: 1,
            ..Default::default()
        };
        assert!(!plain.to_xml().contains("<LocaleList>"));
    }

    #[test]
    fn imf_cpl_writes_essence_descriptor_list_with_source_encoding() {
        let se = "12345678-1111-2222-3333-444444444444";
        let cpl = ImfCpl {
            uuid: "cpl".into(),
            fps_num: 24,
            fps_den: 1,
            resources: vec![ImfResource {
                track_file_uuid: "aud".into(),
                duration: 240,
                kind: ImfTrackKind::Audio,
                source_encoding: Some(se.into()),
            }],
            essence_descriptors: vec![ImfEssenceDescriptor {
                id: se.into(),
                body: sample_audio_descriptor_body("en-US"),
            }],
            ..Default::default()
        };
        let xml = cpl.to_xml();
        // descriptor list present, carries the MCA soundfield + per-track language
        assert!(xml.contains("<EssenceDescriptorList>"));
        assert!(xml.contains(&format!("<Id>urn:uuid:{se}</Id>")));
        assert!(xml.contains("SoundfieldGroupLabelSubDescriptor"));
        assert!(xml.contains("<r1:RFC5646SpokenLanguage>en-US</r1:RFC5646SpokenLanguage>"));
        assert!(xml.contains("<r1:MCATagSymbol>chVIN</r1:MCATagSymbol>"));
        // resource links to the descriptor via SourceEncoding, before TrackFileId
        assert!(xml.contains(&format!("<SourceEncoding>urn:uuid:{se}</SourceEncoding>")));
        assert!(xml.find("<SourceEncoding>").unwrap() < xml.find("<TrackFileId>").unwrap());
        // EssenceDescriptorList precedes EditRate per ST 2067-3 order
        assert!(xml.find("<EssenceDescriptorList>").unwrap() < xml.find("<EditRate>").unwrap());
        // empty descriptors keep it out
        let plain = ImfCpl {
            uuid: "cpl".into(),
            fps_num: 24,
            fps_den: 1,
            ..Default::default()
        };
        assert!(!plain.to_xml().contains("<EssenceDescriptorList>"));
    }

    /// Validate an IMF CPL carrying a LocaleList and an audio EssenceDescriptor
    /// (with MCA soundfield + RFC 5646 language) against the official ST
    /// 2067-3:2016 XSD. Gated on IMFWIZARD_IMF_XSD_DIR (a dir holding
    /// imf-cpl-20160411.xsd and xmldsig-core-schema.xsd anywhere below it) plus
    /// xmllint; skips when absent.
    #[test]
    fn imf_cpl_passes_st2067_3_xsd() {
        let Ok(xsd_dir) = std::env::var("IMFWIZARD_IMF_XSD_DIR") else {
            eprintln!("skipping: set IMFWIZARD_IMF_XSD_DIR to the ST 2067-3 XSD directory");
            return;
        };
        if std::process::Command::new("xmllint")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: xmllint not installed");
            return;
        }
        fn walk(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
            for e in std::fs::read_dir(dir).ok()?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if let Some(f) = walk(&p, name) {
                        return Some(f);
                    }
                } else if p.file_name().and_then(|f| f.to_str()) == Some(name) {
                    return Some(p);
                }
            }
            None
        }
        let root = std::path::Path::new(&xsd_dir);
        let (Some(cpl_xsd), Some(dsig_xsd)) = (
            walk(root, "imf-cpl-20160411.xsd"),
            walk(root, "xmldsig-core-schema.xsd"),
        ) else {
            panic!(
                "could not locate imf-cpl-20160411.xsd and xmldsig-core-schema.xsd under {xsd_dir}"
            );
        };

        let se = "12345678-1111-2222-3333-444444444444";
        let cpl = ImfCpl {
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            title: "Lang + MCA".into(),
            issuer: "postkit".into(),
            creator: "postkit".into(),
            issue_date: "2024-01-01T00:00:00+00:00".into(),
            fps_num: 24,
            fps_den: 1,
            resources: vec![ImfResource {
                track_file_uuid: "aaaaaaaa-1111-2222-3333-444444444444".into(),
                duration: 240,
                kind: ImfTrackKind::Audio,
                source_encoding: Some(se.into()),
            }],
            languages: vec!["de-DE".into(), "en-US".into()],
            essence_descriptors: vec![ImfEssenceDescriptor {
                id: se.into(),
                body: sample_audio_descriptor_body("de-DE"),
            }],
            ..Default::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let cpl_path = dir.path().join("CPL_lang_mca.xml");
        std::fs::write(&cpl_path, cpl.to_xml()).unwrap();

        let driver = dir.path().join("driver.xsd");
        std::fs::write(
            &driver,
            format!(
                r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:import namespace="http://www.smpte-ra.org/schemas/2067-3/2016" schemaLocation="{cpl}"/>
  <xs:import namespace="http://www.w3.org/2000/09/xmldsig#" schemaLocation="{dsig}"/>
</xs:schema>"#,
                cpl = cpl_xsd.display(),
                dsig = dsig_xsd.display(),
            ),
        )
        .unwrap();

        let out = std::process::Command::new("xmllint")
            .arg("--noout")
            .arg("--schema")
            .arg(&driver)
            .arg(&cpl_path)
            .output()
            .expect("run xmllint");
        assert!(
            out.status.success(),
            "CPL must pass ST 2067-3 XSD:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
