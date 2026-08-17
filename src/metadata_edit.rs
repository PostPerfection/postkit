use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::cpl_xml::{read_tag, strip_urn_uuid, write_tag};
use crate::package_edit::{DCP_VOCABULARY, vocabulary_for};

/// Editable metadata field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataField {
    pub key: String,
    pub value: String,
    /// "string", "uuid", "datetime", "integer", "rational"
    pub field_type: String,
    pub readonly: bool,
}

/// Metadata for a CPL or OPL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompositionMetadata {
    pub uuid: String,
    pub title: String,
    pub annotation: String,
    pub issuer: String,
    pub creator: String,
    pub issue_date: String,
    /// "feature", "trailer", "advertisement", etc.
    pub content_kind: String,
    pub rating: String,
    pub custom_fields: Vec<MetadataField>,
}

/// Read metadata from a CPL/OPL XML file.
pub fn read_metadata(cpl_path: &Path) -> CompositionMetadata {
    let content = match std::fs::read_to_string(cpl_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to read CPL: {e}");
            return CompositionMetadata::default();
        }
    };

    let uuid = read_tag(&content, "Id")
        .map(|s| strip_urn_uuid(&s).to_string())
        .unwrap_or_default();
    let title = read_tag(&content, "ContentTitle")
        .or_else(|| read_tag(&content, "AnnotationText"))
        .unwrap_or_default();

    CompositionMetadata {
        uuid,
        title,
        annotation: read_tag(&content, "AnnotationText").unwrap_or_default(),
        issuer: read_tag(&content, "Issuer").unwrap_or_default(),
        creator: read_tag(&content, "Creator").unwrap_or_default(),
        issue_date: read_tag(&content, "IssueDate").unwrap_or_default(),
        content_kind: read_tag(&content, "ContentKind").unwrap_or_default(),
        rating: read_tag(&content, "Rating").unwrap_or_default(),
        custom_fields: Vec::new(),
    }
}

/// Write updated metadata back to CPL/OPL XML.
///
/// Replaces specific element text content while preserving the rest of the XML.
pub fn write_metadata(cpl_path: &Path, meta: &CompositionMetadata) -> i32 {
    let content = match std::fs::read_to_string(cpl_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to read CPL: {e}");
            return -1;
        }
    };

    // ST 429-7 and ST 2067-3 spell the annotation differently, so the element to
    // write depends on which standard wrote this CPL
    let vocabulary = vocabulary_for(&content).unwrap_or(DCP_VOCABULARY);
    let mut updated = content;

    if !meta.title.is_empty() {
        updated = write_tag(&updated, "ContentTitle", &meta.title);
    }
    if !meta.annotation.is_empty() {
        updated = write_tag(&updated, vocabulary.annotation, &meta.annotation);
    }
    if !meta.issuer.is_empty() {
        updated = write_tag(&updated, "Issuer", &meta.issuer);
    }
    if !meta.creator.is_empty() {
        updated = write_tag(&updated, "Creator", &meta.creator);
    }
    if !meta.content_kind.is_empty() {
        updated = write_tag(&updated, "ContentKind", &meta.content_kind);
    }

    match std::fs::write(cpl_path, updated) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("Failed to write CPL: {e}");
            -1
        }
    }
}

/// Batch update a field across multiple CPLs.
pub fn batch_update_field(cpls: &[PathBuf], field_key: &str, new_value: &str) -> i32 {
    let mut failures = 0;
    for cpl in cpls {
        let mut meta = read_metadata(cpl);
        match field_key {
            "title" | "ContentTitle" => meta.title = new_value.to_string(),
            "annotation" | "AnnotationText" => meta.annotation = new_value.to_string(),
            "issuer" | "Issuer" => meta.issuer = new_value.to_string(),
            "creator" | "Creator" => meta.creator = new_value.to_string(),
            "content_kind" | "ContentKind" => meta.content_kind = new_value.to_string(),
            _ => {
                tracing::warn!("Unknown field key: {field_key}");
                continue;
            }
        }
        if write_metadata(cpl, &meta) != 0 {
            failures += 1;
        }
    }
    if failures > 0 { -1 } else { 0 }
}

/// List all editable fields in a CPL.
pub fn list_fields(cpl_path: &Path) -> Vec<MetadataField> {
    let meta = read_metadata(cpl_path);
    vec![
        MetadataField {
            key: "ContentTitle".into(),
            value: meta.title,
            field_type: "string".into(),
            readonly: false,
        },
        MetadataField {
            key: "AnnotationText".into(),
            value: meta.annotation,
            field_type: "string".into(),
            readonly: false,
        },
        MetadataField {
            key: "Issuer".into(),
            value: meta.issuer,
            field_type: "string".into(),
            readonly: false,
        },
        MetadataField {
            key: "Creator".into(),
            value: meta.creator,
            field_type: "string".into(),
            readonly: false,
        },
        MetadataField {
            key: "IssueDate".into(),
            value: meta.issue_date,
            field_type: "datetime".into(),
            readonly: true,
        },
        MetadataField {
            key: "ContentKind".into(),
            value: meta.content_kind,
            field_type: "string".into(),
            readonly: false,
        },
        MetadataField {
            key: "Id".into(),
            value: meta.uuid,
            field_type: "uuid".into(),
            readonly: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let cpl = dir.path().join("CPL.xml");
        std::fs::write(
            &cpl,
            r#"<?xml version="1.0"?>
<CompositionPlaylist>
  <Id>urn:uuid:abc-123</Id>
  <ContentTitle>Test Film</ContentTitle>
  <Issuer>Studio</Issuer>
  <Creator>IMF Wizard</Creator>
  <ContentKind>feature</ContentKind>
</CompositionPlaylist>"#,
        )
        .unwrap();

        let meta = read_metadata(&cpl);
        assert_eq!(meta.uuid, "abc-123");
        assert_eq!(meta.title, "Test Film");
        assert_eq!(meta.issuer, "Studio");
        assert_eq!(meta.content_kind, "feature");
    }

    #[test]
    fn test_write_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let cpl = dir.path().join("CPL.xml");
        std::fs::write(
            &cpl,
            r#"<CompositionPlaylist><ContentTitle>Old</ContentTitle></CompositionPlaylist>"#,
        )
        .unwrap();

        let meta = CompositionMetadata {
            title: "New Title".into(),
            ..Default::default()
        };
        assert_eq!(write_metadata(&cpl, &meta), 0);

        let content = std::fs::read_to_string(&cpl).unwrap();
        assert!(content.contains("New Title"));
        assert!(!content.contains("Old"));
    }

    /// ST 2067-3 names the composition annotation `Annotation`, not the
    /// `AnnotationText` ST 429-7 uses.
    #[test]
    fn an_imf_annotation_lands_in_the_element_its_standard_names() {
        let dir = tempfile::tempdir().unwrap();
        let cpl = dir.path().join("CPL.xml");
        std::fs::write(
            &cpl,
            r#"<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/2067-3/2016">
  <Id>urn:uuid:abc-123</Id>
  <Annotation>first pass</Annotation>
  <ContentTitle>Film</ContentTitle>
  <IssueDate>2026-08-17T09:00:00+00:00</IssueDate>
</CompositionPlaylist>"#,
        )
        .unwrap();

        let meta = CompositionMetadata {
            annotation: "second pass".into(),
            ..Default::default()
        };
        assert_eq!(write_metadata(&cpl, &meta), 0);

        let content = std::fs::read_to_string(&cpl).unwrap();
        assert!(content.contains("<Annotation>second pass</Annotation>"));
        assert!(!content.contains("first pass"));
        assert!(
            !content.contains("AnnotationText"),
            "an IMF CPL has no AnnotationText: {content}"
        );
    }

    #[test]
    fn a_dcp_annotation_lands_in_the_element_its_standard_names() {
        let dir = tempfile::tempdir().unwrap();
        let cpl = dir.path().join("CPL.xml");
        std::fs::write(
            &cpl,
            r#"<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/429-7/2006/CPL">
  <Id>urn:uuid:abc-123</Id>
  <AnnotationText>first pass</AnnotationText>
  <ContentTitleText>OLD-TITLE_FTR_S_EN-XX_51_2K</ContentTitleText>
</CompositionPlaylist>"#,
        )
        .unwrap();

        let meta = CompositionMetadata {
            annotation: "second pass".into(),
            ..Default::default()
        };
        assert_eq!(write_metadata(&cpl, &meta), 0);

        let content = std::fs::read_to_string(&cpl).unwrap();
        assert!(content.contains("<AnnotationText>second pass</AnnotationText>"));
        assert!(!content.contains("first pass"));
    }

    #[test]
    fn test_list_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cpl = dir.path().join("CPL.xml");
        std::fs::write(&cpl, r#"<CompositionPlaylist><ContentTitle>Film</ContentTitle><Id>urn:uuid:x</Id></CompositionPlaylist>"#).unwrap();

        let fields = list_fields(&cpl);
        assert!(fields.len() >= 5);
        assert!(
            fields
                .iter()
                .any(|f| f.key == "ContentTitle" && f.value == "Film")
        );
    }
}
