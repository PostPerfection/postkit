use crate::certificate::KdmMetadata;
use crate::cpl_xml::strip_urn_uuid;
use crate::package_library::Standard;
use crate::packaging::ns::{CPL_IMF, CPL_INTEROP, CPL_SMPTE};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use uuid::Uuid;

type Node<'a> = roxmltree::Node<'a, 'a>;

const ROOT_ELEMENT: &str = "CompositionPlaylist";
const ID_ELEMENT: &str = "Id";
const CONTENT_TITLE_ELEMENT: &str = "ContentTitleText";
const REEL_LIST_ELEMENT: &str = "ReelList";
const REEL_ELEMENT: &str = "Reel";
const ASSET_LIST_ELEMENT: &str = "AssetList";
const KEY_ID_ELEMENT: &str = "KeyId";
const PICTURE_KEY_TYPE: [u8; 4] = *b"MDIK";
const SOUND_KEY_TYPE: [u8; 4] = *b"MDAK";
const TEXT_KEY_TYPE: [u8; 4] = *b"MDSK";
const AUXILIARY_DATA_KEY_TYPE: [u8; 4] = *b"MDEK";
const INDENT: &str = "  ";
const NONE_TEXT: &str = "(none)";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedAsset {
    pub reel: usize,
    pub kind: String,
    pub asset_id: Uuid,
    pub key_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CplKeys {
    pub id: Uuid,
    pub content_title: String,
    pub standard: Standard,
    pub reel_count: usize,
    pub encrypted_assets: Vec<EncryptedAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingKey {
    pub reel: usize,
    pub kind: String,
    pub key_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnusedKey {
    pub key_type: Option<String>,
    pub key_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyTypeMismatch {
    pub reel: usize,
    pub kind: String,
    pub key_id: Uuid,
    pub expected_type: String,
    pub kdm_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdmCoverage {
    pub kdm_cpl_id: Uuid,
    pub cpl_id: Uuid,
    pub content_title: String,
    pub encrypted_asset_count: usize,
    pub kdm_key_count: usize,
    pub missing_keys: Vec<MissingKey>,
    pub unused_keys: Vec<UnusedKey>,
    pub key_type_mismatches: Vec<KeyTypeMismatch>,
}

impl KdmCoverage {
    pub fn cpl_matches(&self) -> bool {
        self.kdm_cpl_id == self.cpl_id
    }

    pub fn passed(&self) -> bool {
        self.cpl_matches() && self.missing_keys.is_empty()
    }
}

fn parse_xml(xml: &str) -> Result<roxmltree::Document<'_>, String> {
    roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            ..Default::default()
        },
    )
    .map_err(|error| format!("CPL is not valid XML: {error}"))
}

// extension assets such as msp-cpl:MainStereoscopicPicture sit in their own namespace
fn child<'a>(node: Node<'a>, name: &str) -> Option<Node<'a>> {
    children(node, name).next()
}

fn children<'a>(node: Node<'a>, name: &str) -> impl Iterator<Item = Node<'a>> {
    node.children()
        .filter(move |child| child.is_element() && child.tag_name().name() == name)
}

fn uuid_text(node: Node<'_>, label: &str) -> Result<Uuid, String> {
    let text = node.text().unwrap_or_default().trim();
    Uuid::parse_str(strip_urn_uuid(text))
        .map_err(|error| format!("{label} {text} is not a UUID: {error}"))
}

fn required_uuid(parent: Node<'_>, name: &str, label: &str) -> Result<Uuid, String> {
    let node = child(parent, name).ok_or_else(|| format!("{label} has no {name}"))?;
    uuid_text(node, &format!("{label} {name}"))
}

fn composition_playlist_root<'a>(
    document: &'a roxmltree::Document<'a>,
) -> Result<Node<'a>, String> {
    let root = document.root_element();
    if root.tag_name().name() != ROOT_ELEMENT {
        return Err(format!(
            "root element is <{}>, not <{ROOT_ELEMENT}>",
            root.tag_name().name()
        ));
    }
    Ok(root)
}

pub fn read_cpl_keys(cpl_xml: &str) -> Result<CplKeys, String> {
    let document = parse_xml(cpl_xml)?;
    let root = composition_playlist_root(&document)?;
    let standard = match root.tag_name().namespace() {
        Some(CPL_SMPTE) => Standard::Smpte,
        Some(CPL_INTEROP) => Standard::Interop,
        Some(CPL_IMF) => {
            return Err("an IMF CPL names no KeyIds, its track files carry them".into());
        }
        other => {
            return Err(format!(
                "CompositionPlaylist namespace {} is not a DCP CPL namespace",
                other.unwrap_or(NONE_TEXT)
            ));
        }
    };
    let id = required_uuid(root, ID_ELEMENT, ROOT_ELEMENT)?;
    let content_title = child(root, CONTENT_TITLE_ELEMENT)
        .and_then(|title| title.text())
        .unwrap_or_default()
        .trim()
        .to_string();
    let reel_list = child(root, REEL_LIST_ELEMENT)
        .ok_or_else(|| format!("{ROOT_ELEMENT} has no {REEL_LIST_ELEMENT}"))?;
    let mut reel_count = 0;
    let mut encrypted_assets = Vec::new();
    for (index, reel_node) in children(reel_list, REEL_ELEMENT).enumerate() {
        let reel = index + 1;
        reel_count = reel;
        let asset_list = child(reel_node, ASSET_LIST_ELEMENT)
            .ok_or_else(|| format!("reel {reel} has no {ASSET_LIST_ELEMENT}"))?;
        for asset in asset_list.children().filter(|node| node.is_element()) {
            let Some(key_id) = child(asset, KEY_ID_ELEMENT) else {
                continue;
            };
            let kind = asset.tag_name().name().to_string();
            let label = format!("reel {reel} {kind}");
            encrypted_assets.push(EncryptedAsset {
                reel,
                asset_id: required_uuid(asset, ID_ELEMENT, &label)?,
                key_id: uuid_text(key_id, &format!("{label} {KEY_ID_ELEMENT}"))?,
                kind,
            });
        }
    }
    Ok(CplKeys {
        id,
        content_title,
        standard,
        reel_count,
        encrypted_assets,
    })
}

fn composition_playlist_id(cpl_xml: &str) -> Result<Uuid, String> {
    let document = parse_xml(cpl_xml)?;
    let root = composition_playlist_root(&document)?;
    required_uuid(root, ID_ELEMENT, ROOT_ELEMENT)
}

pub fn find_cpl(directory: &Path, cpl_id: Uuid) -> Result<Option<PathBuf>, String> {
    for (path, text) in crate::package_library::composition_playlist_files(directory)? {
        let id = composition_playlist_id(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if id == cpl_id {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

// libdcp's key_type() for each reel asset class, where Interop text has none
fn expected_key_type(kind: &str, standard: Standard) -> Option<[u8; 4]> {
    match kind {
        "MainPicture" | "MainStereoscopicPicture" => Some(PICTURE_KEY_TYPE),
        "MainSound" => Some(SOUND_KEY_TYPE),
        "AuxData" => Some(AUXILIARY_DATA_KEY_TYPE),
        "MainSubtitle" | "MainCaption" | "ClosedSubtitle" | "ClosedCaption"
        | "MainClosedCaption"
            if standard == Standard::Smpte =>
        {
            Some(TEXT_KEY_TYPE)
        }
        _ => None,
    }
}

fn key_type_text(key_type: [u8; 4]) -> String {
    String::from_utf8_lossy(&key_type).into_owned()
}

pub fn check_kdm_coverage(kdm: &KdmMetadata, cpl: &CplKeys) -> KdmCoverage {
    let mut missing_keys = Vec::new();
    let mut key_type_mismatches = Vec::new();
    for asset in &cpl.encrypted_assets {
        let Some(kdm_key) = kdm.key_ids.iter().find(|key| key.key_id == asset.key_id) else {
            missing_keys.push(MissingKey {
                reel: asset.reel,
                kind: asset.kind.clone(),
                key_id: asset.key_id,
            });
            continue;
        };
        let (Some(kdm_type), Some(expected_type)) = (
            kdm_key.key_type,
            expected_key_type(&asset.kind, cpl.standard),
        ) else {
            continue;
        };
        if kdm_type != expected_type {
            key_type_mismatches.push(KeyTypeMismatch {
                reel: asset.reel,
                kind: asset.kind.clone(),
                key_id: asset.key_id,
                expected_type: key_type_text(expected_type),
                kdm_type: key_type_text(kdm_type),
            });
        }
    }
    let used_key_ids: HashSet<Uuid> = cpl
        .encrypted_assets
        .iter()
        .map(|asset| asset.key_id)
        .collect();
    let unused_keys = kdm
        .key_ids
        .iter()
        .filter(|key| !used_key_ids.contains(&key.key_id))
        .map(|key| UnusedKey {
            key_type: key.key_type.map(key_type_text),
            key_id: key.key_id,
        })
        .collect();
    KdmCoverage {
        kdm_cpl_id: kdm.cpl_id,
        cpl_id: cpl.id,
        content_title: cpl.content_title.clone(),
        encrypted_asset_count: cpl.encrypted_assets.len(),
        kdm_key_count: kdm.key_ids.len(),
        missing_keys,
        unused_keys,
        key_type_mismatches,
    }
}

impl Display for MissingKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "reel {} {} KeyId urn:uuid:{} has no key in the KDM",
            self.reel, self.kind, self.key_id
        )
    }
}

impl Display for UnusedKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KDM key {} urn:uuid:{} is used by no asset in the CPL",
            self.key_type.as_deref().unwrap_or(NONE_TEXT),
            self.key_id
        )
    }
}

impl Display for KeyTypeMismatch {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "reel {} {} KeyId urn:uuid:{} has key type {} in the KDM, {} expected",
            self.reel, self.kind, self.key_id, self.kdm_type, self.expected_type
        )
    }
}

impl Display for KdmCoverage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let verdict = if self.passed() { "passed" } else { "failed" };
        writeln!(f, "KDM key check: {verdict}")?;
        writeln!(
            f,
            "{INDENT}CPL: urn:uuid:{} {}",
            self.cpl_id, self.content_title
        )?;
        writeln!(
            f,
            "{INDENT}Encrypted assets: {}, KDM keys: {}",
            self.encrypted_asset_count, self.kdm_key_count
        )?;
        if !self.cpl_matches() {
            writeln!(
                f,
                "{INDENT}Error: the KDM is for CPL urn:uuid:{}",
                self.kdm_cpl_id
            )?;
        }
        for missing in &self.missing_keys {
            writeln!(f, "{INDENT}Error: {missing}")?;
        }
        for unused in &self.unused_keys {
            writeln!(f, "{INDENT}Warning: {unused}")?;
        }
        for mismatch in &self.key_type_mismatches {
            writeln!(f, "{INDENT}Warning: {mismatch}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certificate::parse_kdm;
    use crate::kdm_distribution::test_support::{CPL_ID, dkdm};
    use crate::packaging::{AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ns};

    // the test_support DKDM carries MDIK 0x1111 and MDAK 0x2222
    const PICTURE_KEY_ID: u128 = 0x1111;
    const SOUND_KEY_ID: u128 = 0x2222;
    const UNKNOWN_KEY_ID: u128 = 0x3333;
    const OTHER_CPL_ID: &str = "0c0c0000-0000-0000-0000-000000000000";
    const TITLE: &str = "Coverage Feature";

    fn key(value: u128) -> String {
        Uuid::from_u128(value).to_string()
    }

    fn reel(index: u128, picture_key: u128, sound_key: u128) -> DcpCplReel {
        DcpCplReel {
            reel_id: Uuid::from_u128(0xa000 + index).to_string(),
            picture_id: Uuid::from_u128(0xb000 + index).to_string(),
            picture_edit_rate_num: 24,
            picture_edit_rate_den: 1,
            picture_duration: 240,
            picture_width: 1998,
            picture_height: 1080,
            picture_key_id: Some(key(picture_key)),
            sound_id: Some(Uuid::from_u128(0xc000 + index).to_string()),
            sound_edit_rate_num: 24,
            sound_edit_rate_den: 1,
            sound_duration: 240,
            sound_key_id: Some(key(sound_key)),
            ..Default::default()
        }
    }

    fn cpl_xml(id: &str, namespace: &str, reels: Vec<DcpCplReel>) -> String {
        DcpCpl {
            uuid: id.to_string(),
            namespace: namespace.to_string(),
            title: TITLE.to_string(),
            reels,
            ..Default::default()
        }
        .to_xml()
    }

    fn kdm() -> KdmMetadata {
        parse_kdm(&dkdm(TITLE, 7)).unwrap()
    }

    fn coverage(cpl: &str) -> KdmCoverage {
        check_kdm_coverage(&kdm(), &read_cpl_keys(cpl).unwrap())
    }

    #[test]
    fn a_kdm_with_every_key_passes() {
        let cpl = cpl_xml(
            CPL_ID,
            ns::CPL_SMPTE,
            vec![
                reel(1, PICTURE_KEY_ID, SOUND_KEY_ID),
                reel(2, PICTURE_KEY_ID, SOUND_KEY_ID),
            ],
        );
        let result = coverage(&cpl);
        assert!(result.passed(), "{result}");
        assert_eq!(result.encrypted_asset_count, 4);
        assert!(result.unused_keys.is_empty());
        assert!(result.key_type_mismatches.is_empty());
        assert!(result.to_string().starts_with("KDM key check: passed"));
    }

    #[test]
    fn a_missing_key_is_named_with_its_reel_and_kind() {
        let cpl = cpl_xml(
            CPL_ID,
            ns::CPL_SMPTE,
            vec![
                reel(1, PICTURE_KEY_ID, SOUND_KEY_ID),
                reel(2, PICTURE_KEY_ID, UNKNOWN_KEY_ID),
            ],
        );
        let result = coverage(&cpl);
        assert!(!result.passed());
        assert_eq!(
            result.missing_keys,
            vec![MissingKey {
                reel: 2,
                kind: "MainSound".into(),
                key_id: Uuid::from_u128(UNKNOWN_KEY_ID),
            }]
        );
        assert!(result.to_string().contains(&format!(
            "Error: reel 2 MainSound KeyId urn:uuid:{} has no key in the KDM",
            key(UNKNOWN_KEY_ID)
        )));
    }

    #[test]
    fn a_kdm_for_another_cpl_fails() {
        let cpl = cpl_xml(
            OTHER_CPL_ID,
            ns::CPL_SMPTE,
            vec![reel(1, PICTURE_KEY_ID, SOUND_KEY_ID)],
        );
        let result = coverage(&cpl);
        assert!(!result.cpl_matches());
        assert!(!result.passed());
        assert!(
            result
                .to_string()
                .contains(&format!("Error: the KDM is for CPL urn:uuid:{CPL_ID}"))
        );
    }

    #[test]
    fn a_key_of_the_wrong_type_warns_and_an_unused_key_warns() {
        let cpl = cpl_xml(
            CPL_ID,
            ns::CPL_SMPTE,
            vec![reel(1, SOUND_KEY_ID, SOUND_KEY_ID)],
        );
        let result = coverage(&cpl);
        assert!(result.passed(), "{result}");
        assert_eq!(
            result.key_type_mismatches,
            vec![KeyTypeMismatch {
                reel: 1,
                kind: "MainPicture".into(),
                key_id: Uuid::from_u128(SOUND_KEY_ID),
                expected_type: "MDIK".into(),
                kdm_type: "MDAK".into(),
            }]
        );
        assert_eq!(
            result.unused_keys,
            vec![UnusedKey {
                key_type: Some("MDIK".into()),
                key_id: Uuid::from_u128(PICTURE_KEY_ID),
            }]
        );
    }

    #[test]
    fn an_interop_cpl_reads_its_key_ids() {
        let cpl = cpl_xml(
            CPL_ID,
            ns::CPL_INTEROP,
            vec![reel(1, PICTURE_KEY_ID, SOUND_KEY_ID)],
        );
        let keys = read_cpl_keys(&cpl).unwrap();
        assert_eq!(keys.standard, Standard::Interop);
        assert_eq!(keys.content_title, TITLE);
        let key_ids: Vec<(usize, &str, Uuid)> = keys
            .encrypted_assets
            .iter()
            .map(|asset| (asset.reel, asset.kind.as_str(), asset.key_id))
            .collect();
        assert_eq!(
            key_ids,
            vec![
                (1, "MainPicture", Uuid::from_u128(PICTURE_KEY_ID)),
                (1, "MainSound", Uuid::from_u128(SOUND_KEY_ID)),
            ]
        );
    }

    #[test]
    fn an_interop_subtitle_key_has_no_expected_type() {
        let cpl = format!(
            r#"<CompositionPlaylist xmlns="{}">
  <Id>urn:uuid:{CPL_ID}</Id>
  <ReelList><Reel><AssetList>
    <MainSubtitle><Id>urn:uuid:{}</Id><KeyId>urn:uuid:{}</KeyId></MainSubtitle>
  </AssetList></Reel></ReelList>
</CompositionPlaylist>"#,
            ns::CPL_INTEROP,
            Uuid::from_u128(0xd001),
            key(PICTURE_KEY_ID)
        );
        assert!(coverage(&cpl).key_type_mismatches.is_empty());
    }

    #[test]
    fn a_prefixed_cpl_reads_every_asset_kind() {
        let asset = |kind: &str, id: u128, key_id: u128| {
            format!(
                "<{kind}><cpl:Id>urn:uuid:{}</cpl:Id><cpl:KeyId>urn:uuid:{}</cpl:KeyId></{kind}>",
                Uuid::from_u128(id),
                key(key_id)
            )
        };
        let cpl = format!(
            r#"<?xml version="1.0"?>
<cpl:CompositionPlaylist xmlns:cpl="{}" xmlns:msp-cpl="http://www.smpte-ra.org/schemas/429-10/2008/Main-Stereo-Picture-CPL" xmlns:axd="{}">
  <cpl:Id>urn:uuid:{CPL_ID}</cpl:Id>
  <cpl:ContentTitleText>{TITLE}</cpl:ContentTitleText>
  <cpl:ReelList><cpl:Reel><cpl:Id>urn:uuid:{}</cpl:Id><cpl:AssetList>
    {}{}{}{}
    <cpl:MainMarkers><cpl:Id>urn:uuid:{}</cpl:Id></cpl:MainMarkers>
  </cpl:AssetList></cpl:Reel></cpl:ReelList>
</cpl:CompositionPlaylist>"#,
            ns::CPL_SMPTE,
            ns::AUX_DATA,
            Uuid::from_u128(0xa001),
            asset("msp-cpl:MainStereoscopicPicture", 0xb001, PICTURE_KEY_ID),
            asset("cpl:MainSound", 0xc001, SOUND_KEY_ID),
            asset("cpl:MainSubtitle", 0xd001, UNKNOWN_KEY_ID),
            asset("axd:AuxData", 0xe001, UNKNOWN_KEY_ID),
            Uuid::from_u128(0xf001),
        );
        let keys = read_cpl_keys(&cpl).unwrap();
        assert_eq!(keys.id, Uuid::parse_str(CPL_ID).unwrap());
        assert_eq!(keys.content_title, TITLE);
        assert_eq!(keys.reel_count, 1);
        let kinds: Vec<(&str, Uuid, Uuid)> = keys
            .encrypted_assets
            .iter()
            .map(|asset| (asset.kind.as_str(), asset.asset_id, asset.key_id))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (
                    "MainStereoscopicPicture",
                    Uuid::from_u128(0xb001),
                    Uuid::from_u128(PICTURE_KEY_ID)
                ),
                (
                    "MainSound",
                    Uuid::from_u128(0xc001),
                    Uuid::from_u128(SOUND_KEY_ID)
                ),
                (
                    "MainSubtitle",
                    Uuid::from_u128(0xd001),
                    Uuid::from_u128(UNKNOWN_KEY_ID)
                ),
                (
                    "AuxData",
                    Uuid::from_u128(0xe001),
                    Uuid::from_u128(UNKNOWN_KEY_ID)
                ),
            ]
        );
    }

    #[test]
    fn an_imf_cpl_is_refused() {
        let cpl = format!(
            r#"<CompositionPlaylist xmlns="{}"><Id>urn:uuid:{CPL_ID}</Id></CompositionPlaylist>"#,
            ns::CPL_IMF
        );
        let error = read_cpl_keys(&cpl).unwrap_err();
        assert!(error.contains("IMF"), "{error}");
    }

    #[test]
    fn the_cpl_is_found_by_its_id_through_the_assetmap() {
        let directory = tempfile::tempdir().unwrap();
        let mut assets = Vec::new();
        for id in [OTHER_CPL_ID, CPL_ID] {
            // the file name holds no id
            let file = format!("composition_{}.xml", assets.len());
            std::fs::write(
                directory.path().join(&file),
                cpl_xml(
                    id,
                    ns::CPL_SMPTE,
                    vec![reel(1, PICTURE_KEY_ID, SOUND_KEY_ID)],
                ),
            )
            .unwrap();
            assets.push(AssetMapAsset {
                id: id.to_string(),
                path: file,
                packing_list: false,
            });
        }
        let assetmap = AssetMap {
            uuid: Uuid::from_u128(0x0a0a).to_string(),
            namespace: ns::AM_SMPTE.into(),
            assets,
            ..Default::default()
        };
        std::fs::write(directory.path().join("ASSETMAP.xml"), assetmap.to_xml()).unwrap();

        let found = find_cpl(directory.path(), Uuid::parse_str(CPL_ID).unwrap()).unwrap();
        assert_eq!(found, Some(directory.path().join("composition_1.xml")));
        let absent = find_cpl(directory.path(), Uuid::from_u128(UNKNOWN_KEY_ID)).unwrap();
        assert_eq!(absent, None);
    }
}
