//! Edit a written package's CPL metadata without re-wrapping essence: one path
//! for a ST 429-7 DCP and a ST 2067-3 IMF composition.
//!
//! Rewrites the CPL's title / content kind / issuer / annotation and gives it a
//! NEW composition id (a changed CPL is a different composition), then updates
//! the CPL's PKL and ASSETMAP entries (new id, new hash/size). Essence files are
//! never touched: their asset ids and bytes stay identical. Encrypted packages
//! are refused because a KDM names the composition id it authorises. Every
//! document rewritten here loses its signature, which the rewrite invalidates.
//!
//! Reel and segment surgery is out of scope: this covers the metadata fields.

use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::cpl_xml::{read_tag, replace_tag, strip_urn_uuid};
use crate::packaging::escape_xml;

/// What to change on the package's CPL. `None` fields are left as-is.
#[derive(Debug, Clone, Default)]
pub struct PackageEdit {
    /// The package directory: a DCP or an IMP.
    pub input: PathBuf,
    /// Write the edited package here (copied first). None edits in place.
    pub output: Option<PathBuf>,
    pub title: Option<String>,
    pub annotation: Option<String>,
    /// An ISDCF abbreviation (FTR, TLR, ...) or a kind string; see
    /// [`normalized_content_kind`].
    pub content_kind: Option<String>,
    pub issuer: Option<String>,
}

/// What an edit left behind.
#[derive(Debug, Clone)]
pub struct EditedPackage {
    /// The package carrying the edits: `output` when one was given.
    pub package_dir: PathBuf,
    /// The rewritten CPL, named after its new composition id.
    pub cpl_path: PathBuf,
    /// The composition id the rewrite minted.
    pub composition_id: String,
    /// Documents written unsigned because the rewrite invalidated the signature
    /// they carried, by file name.
    pub unsigned_documents: Vec<String>,
}

/// ISDCF abbreviations and the CPL content kind each names.
const CONTENT_KIND_ABBREVIATIONS: [(&str, &str); 11] = [
    ("FTR", "feature"),
    ("SHR", "short"),
    ("TLR", "trailer"),
    ("TST", "test"),
    ("XSN", "transitional"),
    ("RTG", "rating"),
    ("TSR", "teaser"),
    ("POL", "policy"),
    ("PSA", "psa"),
    ("ADV", "advertisement"),
    ("EPS", "episode"),
];

/// The CPL content kind an ISDCF abbreviation names, or the value unchanged when
/// it already is a kind string. An IMF ContentKind defaults to the ST 429-7
/// vocabulary scope, so both formats share this.
pub fn normalized_content_kind(value: &str) -> String {
    CONTENT_KIND_ABBREVIATIONS
        .iter()
        .find(|(abbreviation, _)| abbreviation.eq_ignore_ascii_case(value))
        .map_or_else(|| value.to_string(), |(_, kind)| (*kind).to_string())
}

/// The element names one composition playlist standard uses for the fields this
/// module edits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CplVocabulary {
    content_title: &'static str,
    /// The composition-level annotation, which both standards place between the
    /// composition Id and IssueDate.
    pub(crate) annotation: &'static str,
    /// The ST 429-16 metadata title, kept consistent with the content title when
    /// the CPL carries one. IMF has no counterpart.
    metadata_content_title: Option<&'static str>,
}

pub(crate) const DCP_VOCABULARY: CplVocabulary = CplVocabulary {
    content_title: "ContentTitleText",
    annotation: "AnnotationText",
    metadata_content_title: Some("meta:FullContentTitleText"),
};

const IMF_VOCABULARY: CplVocabulary = CplVocabulary {
    content_title: "ContentTitle",
    annotation: "Annotation",
    metadata_content_title: None,
};

/// Which standard wrote this CPL, told by the element holding its title. Checked
/// in this order because `<ContentTitle` is also a prefix of `<ContentTitleText`.
pub(crate) fn vocabulary_for(cpl: &str) -> Option<CplVocabulary> {
    if cpl.contains("<ContentTitleText") {
        Some(DCP_VOCABULARY)
    } else if cpl.contains("<ContentTitle") {
        Some(IMF_VOCABULARY)
    } else {
        None
    }
}

/// Apply the edits, in place or into `output`.
pub fn edit_package(edit: &PackageEdit) -> Result<EditedPackage, String> {
    if !edit.input.is_dir() {
        return Err(format!("{} is not a directory", edit.input.display()));
    }
    if edit.title.is_none()
        && edit.annotation.is_none()
        && edit.content_kind.is_none()
        && edit.issuer.is_none()
    {
        return Err(
            "nothing to edit: pass at least one of title, annotation, content kind, issuer".into(),
        );
    }

    let package_dir = match edit.output.as_ref() {
        Some(output) => {
            copy_tree(&edit.input, output)
                .map_err(|e| format!("cannot copy the package to {}: {e}", output.display()))?;
            output.clone()
        }
        None => edit.input.clone(),
    };

    let documents = xml_documents(&package_dir);
    let cpl_path = single_document(&documents, "CompositionPlaylist", &package_dir)?;
    let cpl = std::fs::read_to_string(&cpl_path)
        .map_err(|e| format!("cannot read {}: {e}", cpl_path.display()))?;

    let vocabulary = vocabulary_for(&cpl).ok_or_else(|| {
        format!(
            "{} holds no content title, so it is neither a ST 429-7 nor a ST 2067-3 composition",
            cpl_path.display()
        )
    })?;
    if cpl.contains("<KeyId>") {
        return Err(
            "the package is encrypted; a KDM authorises its keys for one named composition id, \
             so a metadata edit (which mints a new one) would invalidate every KDM"
                .into(),
        );
    }

    let old_id = read_tag(&cpl, "Id")
        .map(|id| strip_urn_uuid(&id).to_string())
        .ok_or_else(|| format!("{} has no Id", cpl_path.display()))?;
    let new_id = uuid::Uuid::new_v4().to_string();

    // the composition id appears once in the CPL (nothing there references it),
    // so a plain replace is safe
    let mut xml = cpl.replace(&old_id, &new_id);
    if let Some(title) = edit.title.as_deref() {
        xml = set_element(&xml, vocabulary.content_title, title, &cpl_path)?;
        if let Some(metadata_title) = vocabulary.metadata_content_title
            && let Some(updated) = replace_tag(&xml, metadata_title, title)
        {
            xml = updated;
        }
    }
    if let Some(kind) = edit.content_kind.as_deref() {
        xml = set_element(
            &xml,
            "ContentKind",
            &normalized_content_kind(kind),
            &cpl_path,
        )?;
    }
    if let Some(issuer) = edit.issuer.as_deref() {
        xml = set_element(&xml, "Issuer", issuer, &cpl_path)?;
    }
    if let Some(annotation) = edit.annotation.as_deref() {
        set_annotation(&mut xml, vocabulary.annotation, annotation);
    }

    // ── write the CPL under its new id, drop the old file ──
    let new_cpl_name = format!("CPL_{new_id}.xml");
    let new_cpl_path = cpl_path.with_file_name(&new_cpl_name);
    // strip before the write, so the hash below covers what actually lands
    let mut unsigned_documents = Vec::new();
    if crate::xmldsig::strip_signature(&mut xml) {
        unsigned_documents.push(new_cpl_name.clone());
    }
    crate::fs::write_atomic(&new_cpl_path, xml.as_bytes())?;
    let old_cpl_name = file_name(&cpl_path);
    if old_cpl_name != new_cpl_name {
        std::fs::remove_file(&cpl_path)
            .map_err(|e| format!("cannot remove the old CPL {}: {e}", cpl_path.display()))?;
    }

    let hash = crate::hash::hash_file(&new_cpl_path, crate::hash::HashAlgorithm::Sha1)
        .map(|digest| digest.base64)
        .map_err(|e| format!("cannot hash {}: {e}", new_cpl_path.display()))?;
    let size = std::fs::metadata(&new_cpl_path)
        .map_err(|e| format!("cannot stat {}: {e}", new_cpl_path.display()))?
        .len();

    // ── update the PKL(s): the CPL asset's id, hash, size ──
    let mut patched_a_pkl = false;
    for pkl in paths_of(&documents, "PackingList") {
        let content = std::fs::read_to_string(&pkl)
            .map_err(|e| format!("cannot read {}: {e}", pkl.display()))?;
        if !content.contains(&old_id) {
            continue;
        }
        let mut updated = patch_pkl_cpl_asset(&content, &old_id, &new_id, &hash, size);
        if crate::xmldsig::strip_signature(&mut updated) {
            unsigned_documents.push(file_name(&pkl));
        }
        crate::fs::write_atomic(&pkl, updated.as_bytes())?;
        patched_a_pkl = true;
    }
    if !patched_a_pkl {
        return Err(format!(
            "no PKL in {} lists the composition id, so the package is inconsistent",
            package_dir.display()
        ));
    }

    // ── update the ASSETMAP: the CPL id and its <Path> ──
    let assetmap_path = single_document(&documents, "AssetMap", &package_dir)?;
    let assetmap = std::fs::read_to_string(&assetmap_path)
        .map_err(|e| format!("cannot read {}: {e}", assetmap_path.display()))?;
    if !assetmap.contains(&old_id) {
        return Err(format!(
            "{} does not list the composition id, so the package is inconsistent",
            assetmap_path.display()
        ));
    }
    let mut updated = assetmap
        .replace(&old_id, &new_id)
        .replace(&old_cpl_name, &new_cpl_name);
    if crate::xmldsig::strip_signature(&mut updated) {
        unsigned_documents.push(file_name(&assetmap_path));
    }
    crate::fs::write_atomic(&assetmap_path, updated.as_bytes())?;

    Ok(EditedPackage {
        package_dir,
        cpl_path: new_cpl_path,
        composition_id: new_id,
        unsigned_documents,
    })
}

/// Replace an element's text, refusing when the CPL has no such element: the
/// caller asked for a change that would otherwise go silently missing.
fn set_element(xml: &str, element: &str, value: &str, cpl_path: &Path) -> Result<String, String> {
    replace_tag(xml, element, value)
        .ok_or_else(|| format!("{} has no <{element}> to set", cpl_path.display()))
}

/// Set the composition-level annotation: replace it when the CPL carries one,
/// else insert it after the composition Id, where both standards place it.
fn set_annotation(xml: &mut String, element: &str, value: &str) {
    // a reel, segment, resource or locale carries an annotation of its own, so
    // only the region ahead of IssueDate can hold the composition's
    let boundary = xml.find("<IssueDate").unwrap_or(xml.len());
    if let Some(updated) = replace_tag(&xml[..boundary], element, value) {
        xml.replace_range(..boundary, &updated);
        return;
    }
    let Some(id_end) = xml.find("</Id>") else {
        return;
    };
    let insert_at = xml[id_end..]
        .find('\n')
        .map_or(xml.len(), |newline| id_end + newline + 1);
    let line = format!("  <{element}>{}</{element}>\n", escape_xml(value));
    xml.insert_str(insert_at, &line);
}

/// Rewrite the CPL asset's block in a PKL: new id, hash, size. Its
/// HashAlgorithm, where the IMF PKL carries one, still names SHA-1.
fn patch_pkl_cpl_asset(
    content: &str,
    old_id: &str,
    new_id: &str,
    new_hash: &str,
    new_size: u64,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(block_start) = rest.find("<Asset>") {
        let block_end = rest[block_start..]
            .find("</Asset>")
            .map_or(rest.len(), |end| block_start + end + "</Asset>".len());
        out.push_str(&rest[..block_start]);
        let block = &rest[block_start..block_end];
        if block.contains(old_id) {
            let patched = block.replace(old_id, new_id);
            let patched = replace_tag(&patched, "Hash", new_hash).unwrap_or(patched);
            let patched = replace_tag(&patched, "Size", &new_size.to_string()).unwrap_or(patched);
            out.push_str(&patched);
        } else {
            out.push_str(block);
        }
        rest = &rest[block_end..];
    }
    out.push_str(rest);
    out
}

/// The package's XML documents as (root element, path). Only `.xml` files and a
/// bare `ASSETMAP` are opened, so the essence is never read.
fn xml_documents(dir: &Path) -> Vec<(String, PathBuf)> {
    const EXTENSIONLESS_ASSETMAP: &str = "ASSETMAP";
    let mut documents = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let is_xml = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"));
        if !path.is_file() || !(is_xml || file_name(&path) == EXTENSIONLESS_ASSETMAP) {
            continue;
        }
        if let Some(root) = root_element(&path) {
            documents.push((root, path));
        }
    }
    documents.sort();
    documents
}

/// The document's root element, without any namespace prefix.
fn root_element(path: &Path) -> Option<String> {
    let xml = std::fs::read_to_string(path).ok()?;
    let mut reader = Reader::from_str(&xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                return Some(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn paths_of(documents: &[(String, PathBuf)], root: &str) -> Vec<PathBuf> {
    documents
        .iter()
        .filter(|(element, _)| element == root)
        .map(|(_, path)| path.clone())
        .collect()
}

/// The one document with this root element. More than one CPL means a
/// multi-composition package, which the caller has to name a composition in.
fn single_document(
    documents: &[(String, PathBuf)],
    root: &str,
    dir: &Path,
) -> Result<PathBuf, String> {
    let mut found = paths_of(documents, root);
    match found.len() {
        0 => Err(format!("no {root} found in {}", dir.display())),
        1 => Ok(found.remove(0)),
        n => Err(format!(
            "{} holds {n} {root} documents; this edit operates on a single-composition package",
            dir.display()
        )),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Write `bytes` to `path` atomically: a temp file in the same dir, then rename.
/// Copy `src` into `dst`, subdirectories and all.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packaging::{
        AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ImfCpl, ImfResource, ImfTrackKind,
        PackingList, PklAsset, ns,
    };

    const CPL_ID: &str = "11111111-1111-1111-1111-111111111111";
    const PKL_ID: &str = "33333333-3333-3333-3333-333333333333";
    const PICTURE_ID: &str = "22222222-2222-2222-2222-222222222222";
    const OLD_TITLE: &str = "OLD-TITLE_FTR_S_EN-XX_51_2K";
    const ESSENCE: &[u8] = b"picture essence";
    const ESSENCE_NAME: &str = "picture.mxf";
    /// A stale hash in the PKL, so a test can tell a rewritten entry apart.
    const STALE_HASH: &str = "c3RhbGU=";

    fn dcp_cpl_xml(key_id: Option<&str>) -> String {
        DcpCpl {
            uuid: CPL_ID.into(),
            namespace: ns::CPL_SMPTE.into(),
            title: OLD_TITLE.into(),
            content_kind: "feature".into(),
            issuer: "PostPerfection".into(),
            creator: "postkit".into(),
            issue_date: "2026-08-17T09:00:00+00:00".into(),
            reels: vec![DcpCplReel {
                reel_id: "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
                picture_id: PICTURE_ID.into(),
                picture_edit_rate_num: 24,
                picture_edit_rate_den: 1,
                picture_duration: 480,
                picture_width: 1998,
                picture_height: 1080,
                picture_key_id: key_id.map(str::to_string),
                ..Default::default()
            }],
            ..Default::default()
        }
        .to_xml()
    }

    fn imf_cpl_xml() -> String {
        ImfCpl {
            uuid: CPL_ID.into(),
            title: OLD_TITLE.into(),
            content_kind: "feature".into(),
            issuer: "PostPerfection".into(),
            creator: "postkit".into(),
            issue_date: "2026-08-17T09:00:00+00:00".into(),
            fps_num: 24,
            fps_den: 1,
            resources: vec![ImfResource {
                track_file_uuid: PICTURE_ID.into(),
                duration: 480,
                kind: ImfTrackKind::Image,
                source_encoding: None,
            }],
            ..Default::default()
        }
        .to_xml()
    }

    /// A package holding `cpl_xml`, a PKL and an ASSETMAP that both name the
    /// composition, plus an essence file the edit must leave alone.
    fn write_package(dir: &Path, cpl_xml: &str, pkl_namespace: &str) {
        let cpl_name = format!("CPL_{CPL_ID}.xml");
        std::fs::write(dir.join(&cpl_name), cpl_xml).unwrap();
        let pkl = PackingList {
            uuid: PKL_ID.into(),
            namespace: pkl_namespace.into(),
            issuer: "PostPerfection".into(),
            creator: "postkit".into(),
            issue_date: "2026-08-17T09:00:00+00:00".into(),
            assets: vec![PklAsset {
                id: CPL_ID.into(),
                hash: STALE_HASH.into(),
                size: 1,
                asset_type: "text/xml".into(),
            }],
            annotation: None,
        };
        std::fs::write(dir.join(format!("PKL_{PKL_ID}.xml")), pkl.to_xml()).unwrap();
        let assetmap = AssetMap {
            uuid: "44444444-4444-4444-4444-444444444444".into(),
            namespace: ns::AM_SMPTE.into(),
            issuer: "PostPerfection".into(),
            creator: "postkit".into(),
            issue_date: "2026-08-17T09:00:00+00:00".into(),
            assets: vec![
                AssetMapAsset {
                    id: PKL_ID.into(),
                    path: format!("PKL_{PKL_ID}.xml"),
                    packing_list: true,
                },
                AssetMapAsset {
                    id: CPL_ID.into(),
                    path: cpl_name,
                    packing_list: false,
                },
            ],
            annotation: None,
        };
        std::fs::write(dir.join("ASSETMAP.xml"), assetmap.to_xml()).unwrap();
        std::fs::write(dir.join(ESSENCE_NAME), ESSENCE).unwrap();
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    /// The PKL asset block for `id`, so a test can read the hash and size it states.
    fn pkl_asset_of(pkl: &str, id: &str) -> String {
        pkl.split("<Asset>")
            .find(|block| block.contains(id))
            .unwrap_or_default()
            .to_string()
    }

    /// Every claim a retitle has to satisfy, whichever standard wrote the CPL.
    fn assert_retitled(dir: &Path, edited: &EditedPackage, title_element: &str, new_title: &str) {
        let cpl = std::fs::read_to_string(&edited.cpl_path).unwrap();
        assert!(cpl.contains(&format!("<{title_element}>{new_title}<")));
        assert!(!cpl.contains(CPL_ID), "the composition id must change");
        assert!(
            cpl.contains(PICTURE_ID),
            "the essence keeps its own asset id"
        );
        assert_eq!(
            edited.cpl_path,
            dir.join(format!("CPL_{}.xml", edited.composition_id))
        );
        assert!(
            !dir.join(format!("CPL_{CPL_ID}.xml")).exists(),
            "the CPL under the old id must be gone"
        );

        let expected_hash =
            crate::hash::hash_file(&edited.cpl_path, crate::hash::HashAlgorithm::Sha1)
                .unwrap()
                .base64;
        let expected_size = std::fs::metadata(&edited.cpl_path).unwrap().len();
        let pkl = read(dir, &format!("PKL_{PKL_ID}.xml"));
        let asset = pkl_asset_of(&pkl, &edited.composition_id);
        assert!(
            asset.contains(&format!("<Hash>{expected_hash}</Hash>")),
            "the PKL must state the rewritten CPL's hash, got {asset}"
        );
        assert!(
            asset.contains(&format!("<Size>{expected_size}</Size>")),
            "the PKL must state the rewritten CPL's size, got {asset}"
        );
        assert!(!pkl.contains(STALE_HASH));
        assert!(!pkl.contains(CPL_ID));

        let assetmap = read(dir, "ASSETMAP.xml");
        assert!(assetmap.contains(&edited.composition_id));
        assert!(assetmap.contains(&format!("CPL_{}.xml", edited.composition_id)));
        assert!(!assetmap.contains(CPL_ID));

        assert_eq!(
            std::fs::read(dir.join(ESSENCE_NAME)).unwrap(),
            ESSENCE,
            "essence must be untouched"
        );
    }

    #[test]
    fn a_dcp_retitle_mints_a_new_composition_id_and_repoints_the_package() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &dcp_cpl_xml(None), ns::PKL_SMPTE);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("NEW-TITLE_FTR_S_EN-XX_51_2K".into()),
            ..Default::default()
        })
        .unwrap();

        assert_retitled(
            dir.path(),
            &edited,
            "ContentTitleText",
            "NEW-TITLE_FTR_S_EN-XX_51_2K",
        );
    }

    #[test]
    fn an_imf_retitle_rewrites_content_title_and_repoints_the_package() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &imf_cpl_xml(), ns::PKL_IMF);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("Nuovo Titolo".into()),
            ..Default::default()
        })
        .unwrap();

        assert_retitled(dir.path(), &edited, "ContentTitle", "Nuovo Titolo");
        // the ST 2067-2 digest identifier still describes the SHA-1 written above
        let pkl = read(dir.path(), &format!("PKL_{PKL_ID}.xml"));
        assert!(pkl.contains("xmldsig#sha1"));
    }

    #[test]
    fn a_title_with_xml_syntax_is_escaped() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &imf_cpl_xml(), ns::PKL_IMF);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("Ben & Cléo <2>".into()),
            ..Default::default()
        })
        .unwrap();

        let cpl = std::fs::read_to_string(&edited.cpl_path).unwrap();
        assert!(cpl.contains("<ContentTitle>Ben &amp; Cléo &lt;2&gt;</ContentTitle>"));
    }

    #[test]
    fn an_encrypted_package_is_refused_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        write_package(
            dir.path(),
            &dcp_cpl_xml(Some("55555555-5555-5555-5555-555555555555")),
            ns::PKL_SMPTE,
        );

        let error = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("NEW-TITLE".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(error.contains("encrypted"), "got {error}");

        let cpl = read(dir.path(), &format!("CPL_{CPL_ID}.xml"));
        assert!(cpl.contains(OLD_TITLE), "nothing may change");
        assert!(read(dir.path(), &format!("PKL_{PKL_ID}.xml")).contains(STALE_HASH));
    }

    #[test]
    fn an_edit_with_nothing_to_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &imf_cpl_xml(), ns::PKL_IMF);

        let error = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(error.contains("nothing to edit"), "got {error}");
        assert!(dir.path().join(format!("CPL_{CPL_ID}.xml")).exists());
    }

    #[test]
    fn a_package_with_two_cpls_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &imf_cpl_xml(), ns::PKL_IMF);
        std::fs::write(
            dir.path()
                .join("CPL_66666666-6666-6666-6666-666666666666.xml"),
            imf_cpl_xml().replace(CPL_ID, "66666666-6666-6666-6666-666666666666"),
        )
        .unwrap();

        let error = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("Nuovo Titolo".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(error.contains("single-composition"), "got {error}");
    }

    #[test]
    fn an_output_copy_leaves_the_source_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("ov");
        std::fs::create_dir(&source).unwrap();
        write_package(&source, &imf_cpl_xml(), ns::PKL_IMF);
        let copy = dir.path().join("retitled");

        let edited = edit_package(&PackageEdit {
            input: source.clone(),
            output: Some(copy.clone()),
            title: Some("Nuovo Titolo".into()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(edited.package_dir, copy);
        assert!(read(&source, &format!("CPL_{CPL_ID}.xml")).contains(OLD_TITLE));
        assert_retitled(&copy, &edited, "ContentTitle", "Nuovo Titolo");
    }

    #[test]
    fn an_annotation_is_inserted_when_the_cpl_carries_none() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &imf_cpl_xml(), ns::PKL_IMF);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            annotation: Some("first pass".into()),
            ..Default::default()
        })
        .unwrap();

        let cpl = std::fs::read_to_string(&edited.cpl_path).unwrap();
        assert!(cpl.contains("<Annotation>first pass</Annotation>"));
        assert!(
            cpl.find("<Annotation>").unwrap() < cpl.find("<IssueDate>").unwrap(),
            "ST 2067-3 puts the annotation ahead of IssueDate"
        );
        assert!(
            cpl.contains(OLD_TITLE),
            "the title is not part of this edit"
        );
    }

    #[test]
    fn a_dcp_annotation_replaces_the_one_the_cpl_already_carries() {
        let dir = tempfile::tempdir().unwrap();
        let cpl_xml = dcp_cpl_xml(None).replace(
            "<IssueDate>",
            "<AnnotationText>first pass</AnnotationText>\n  <IssueDate>",
        );
        write_package(dir.path(), &cpl_xml, ns::PKL_SMPTE);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            annotation: Some("second pass".into()),
            ..Default::default()
        })
        .unwrap();

        let cpl = std::fs::read_to_string(&edited.cpl_path).unwrap();
        assert!(cpl.contains("<AnnotationText>second pass</AnnotationText>"));
        assert!(!cpl.contains("first pass"));
        assert!(
            cpl.contains("<AnnotationText>Reel 1</AnnotationText>"),
            "a reel's own annotation is not the composition's"
        );
    }

    #[test]
    fn a_content_kind_abbreviation_lands_as_its_cpl_kind() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &imf_cpl_xml(), ns::PKL_IMF);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            content_kind: Some("tlr".into()),
            ..Default::default()
        })
        .unwrap();

        let cpl = std::fs::read_to_string(&edited.cpl_path).unwrap();
        assert!(cpl.contains("<ContentKind>trailer</ContentKind>"));
        assert_eq!(normalized_content_kind("episode"), "episode");
    }

    #[test]
    fn a_signed_cpl_is_rewritten_unsigned_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let signed = imf_cpl_xml().replace(
            "</CompositionPlaylist>",
            "  <Signer>key info</Signer>\n  <ds:Signature xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\">\
             <ds:SignatureValue>x</ds:SignatureValue></ds:Signature>\n</CompositionPlaylist>",
        );
        write_package(dir.path(), &signed, ns::PKL_IMF);

        let edited = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("Nuovo Titolo".into()),
            ..Default::default()
        })
        .unwrap();

        let cpl = std::fs::read_to_string(&edited.cpl_path).unwrap();
        assert!(
            !cpl.contains("Signature"),
            "the signature no longer matches"
        );
        assert!(!cpl.contains("<Signer>"));
        assert_eq!(
            edited.unsigned_documents,
            vec![format!("CPL_{}.xml", edited.composition_id)]
        );
        assert_retitled(dir.path(), &edited, "ContentTitle", "Nuovo Titolo");
    }

    #[test]
    fn a_package_without_a_cpl_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ESSENCE_NAME), ESSENCE).unwrap();

        let error = edit_package(&PackageEdit {
            input: dir.path().to_path_buf(),
            title: Some("Nuovo Titolo".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(
            error.contains("no CompositionPlaylist found"),
            "got {error}"
        );
    }
}
