//! CPL annotation management — add/read annotations and revision markers.

use std::fs;
use std::path::Path;

use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use crate::cpl_xml::{read_tag, strip_urn_uuid};

#[derive(Debug, Error)]
pub enum AnnotationError {
    #[error("Cannot open file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Not a valid CPL (missing </CompositionPlaylist> closing tag)")]
    InvalidCpl,
}

#[derive(Debug, Clone, Serialize)]
pub struct CplAnnotation {
    pub author: String,
    pub timestamp: String,
    pub text: String,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CplVersionInfo {
    pub cpl_uuid: String,
    pub title: String,
    pub annotations: Vec<CplAnnotation>,
    pub current_revision: String,
}

fn iso_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Add an annotation to a CPL file.
pub fn annotate_cpl(cpl_path: &Path, annotation: &CplAnnotation) -> Result<(), AnnotationError> {
    let content = fs::read_to_string(cpl_path)?;

    let close_pos = content
        .find("</CompositionPlaylist>")
        .ok_or(AnnotationError::InvalidCpl)?;

    let timestamp = if annotation.timestamp.is_empty() {
        iso_timestamp()
    } else {
        annotation.timestamp.clone()
    };

    let mut ann_xml = format!(
        "  <Annotation>\n\
         \x20   <Text>{}</Text>\n\
         \x20   <Author>{}</Author>\n\
         \x20   <Timestamp>{}</Timestamp>\n",
        annotation.text, annotation.author, timestamp
    );
    if !annotation.revision.is_empty() {
        ann_xml.push_str(&format!(
            "    <Revision>{}</Revision>\n",
            annotation.revision
        ));
    }
    ann_xml.push_str("  </Annotation>\n");

    let mut output = String::with_capacity(content.len() + ann_xml.len());
    output.push_str(&content[..close_pos]);
    output.push_str(&ann_xml);
    output.push_str(&content[close_pos..]);

    fs::write(cpl_path, output)?;
    Ok(())
}

/// Read all annotations from a CPL file.
pub fn read_cpl_annotations(cpl_path: &Path) -> Result<CplVersionInfo, AnnotationError> {
    let content = fs::read_to_string(cpl_path)?;

    let cpl_uuid = read_tag(&content, "Id")
        .map(|s| strip_urn_uuid(&s).to_string())
        .unwrap_or_default();
    let title = read_tag(&content, "ContentTitleText")
        .or_else(|| read_tag(&content, "ContentTitle"))
        .unwrap_or_default();

    let mut annotations = Vec::new();
    let mut remaining = content.as_str();

    while let Some(start) = remaining.find("<Annotation>") {
        let after_open = start + "<Annotation>".len();
        if let Some(end) = remaining[after_open..].find("</Annotation>") {
            let block = &remaining[after_open..after_open + end];
            let text = read_tag(block, "Text").unwrap_or_default();
            let author = read_tag(block, "Author").unwrap_or_default();
            let timestamp = read_tag(block, "Timestamp").unwrap_or_default();
            let revision = read_tag(block, "Revision").unwrap_or_default();

            annotations.push(CplAnnotation {
                author,
                timestamp,
                text,
                revision,
            });
            remaining = &remaining[after_open + end + "</Annotation>".len()..];
        } else {
            break;
        }
    }

    let current_revision = annotations
        .last()
        .map(|a| a.revision.clone())
        .unwrap_or_default();

    Ok(CplVersionInfo {
        cpl_uuid,
        title,
        annotations,
        current_revision,
    })
}

/// Set a revision marker on a CPL.
pub fn set_cpl_revision(
    cpl_path: &Path,
    revision: &str,
    author: &str,
) -> Result<(), AnnotationError> {
    let ann = CplAnnotation {
        text: format!("Revision set to {revision}"),
        author: author.to_string(),
        timestamp: String::new(),
        revision: revision.to_string(),
    };
    annotate_cpl(cpl_path, &ann)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_cpl() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/429-7/2006/CPL">
  <Id>urn:uuid:abc-123-def</Id>
  <ContentTitleText>Test Film</ContentTitleText>
  <EditRate>24 1</EditRate>
</CompositionPlaylist>"#
    }

    #[test]
    fn test_annotate_cpl() {
        let tmp = TempDir::new().unwrap();
        let cpl = tmp.path().join("CPL.xml");
        fs::write(&cpl, sample_cpl()).unwrap();

        let ann = CplAnnotation {
            text: "Initial version".to_string(),
            author: "Test User".to_string(),
            timestamp: "2024-01-15T10:00:00Z".to_string(),
            revision: "v1.0".to_string(),
        };

        annotate_cpl(&cpl, &ann).unwrap();

        let content = fs::read_to_string(&cpl).unwrap();
        assert!(content.contains("<Text>Initial version</Text>"));
        assert!(content.contains("<Author>Test User</Author>"));
        assert!(content.contains("<Timestamp>2024-01-15T10:00:00Z</Timestamp>"));
        assert!(content.contains("<Revision>v1.0</Revision>"));
        assert!(content.contains("</CompositionPlaylist>"));
    }

    #[test]
    fn test_read_annotations() {
        let tmp = TempDir::new().unwrap();
        let cpl = tmp.path().join("CPL.xml");
        fs::write(&cpl, sample_cpl()).unwrap();

        let ann1 = CplAnnotation {
            text: "First edit".to_string(),
            author: "Editor A".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            revision: "v1.0".to_string(),
        };
        let ann2 = CplAnnotation {
            text: "Color correction".to_string(),
            author: "Editor B".to_string(),
            timestamp: "2024-02-01T00:00:00Z".to_string(),
            revision: "v1.1".to_string(),
        };
        annotate_cpl(&cpl, &ann1).unwrap();
        annotate_cpl(&cpl, &ann2).unwrap();

        let info = read_cpl_annotations(&cpl).unwrap();
        assert_eq!(info.cpl_uuid, "abc-123-def");
        assert_eq!(info.title, "Test Film");
        assert_eq!(info.annotations.len(), 2);
        assert_eq!(info.annotations[0].text, "First edit");
        assert_eq!(info.annotations[1].author, "Editor B");
        assert_eq!(info.current_revision, "v1.1");
    }

    #[test]
    fn test_set_revision() {
        let tmp = TempDir::new().unwrap();
        let cpl = tmp.path().join("CPL.xml");
        fs::write(&cpl, sample_cpl()).unwrap();

        set_cpl_revision(&cpl, "v2.0", "postkit").unwrap();

        let info = read_cpl_annotations(&cpl).unwrap();
        assert_eq!(info.current_revision, "v2.0");
        assert!(info.annotations[0].text.contains("Revision set to v2.0"));
        assert_eq!(info.annotations[0].author, "postkit");
    }

    #[test]
    fn test_annotate_invalid_cpl() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("bad.xml");
        fs::write(&file, "<NotACpl></NotACpl>").unwrap();

        let ann = CplAnnotation {
            text: "test".to_string(),
            author: "test".to_string(),
            timestamp: String::new(),
            revision: String::new(),
        };

        assert!(annotate_cpl(&file, &ann).is_err());
    }

    #[test]
    fn test_annotation_without_revision() {
        let tmp = TempDir::new().unwrap();
        let cpl = tmp.path().join("CPL.xml");
        fs::write(&cpl, sample_cpl()).unwrap();

        let ann = CplAnnotation {
            text: "A note".to_string(),
            author: "Someone".to_string(),
            timestamp: "2024-06-01T12:00:00Z".to_string(),
            revision: String::new(),
        };
        annotate_cpl(&cpl, &ann).unwrap();

        let content = fs::read_to_string(&cpl).unwrap();
        assert!(!content.contains("<Revision>"));
        assert!(content.contains("<Text>A note</Text>"));
    }

    #[test]
    fn test_read_no_annotations() {
        let tmp = TempDir::new().unwrap();
        let cpl = tmp.path().join("CPL.xml");
        fs::write(&cpl, sample_cpl()).unwrap();

        let info = read_cpl_annotations(&cpl).unwrap();
        assert_eq!(info.cpl_uuid, "abc-123-def");
        assert_eq!(info.title, "Test Film");
        assert!(info.annotations.is_empty());
        assert_eq!(info.current_revision, "");
    }
}
