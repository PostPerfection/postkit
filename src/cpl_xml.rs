//! Shared CPL/OPL XML field helpers used by cpl_annotation and metadata_edit.

/// Strip a leading "urn:uuid:" from a CPL/asset id, returning the bare id.
pub fn strip_urn_uuid(id: &str) -> &str {
    id.strip_prefix("urn:uuid:").unwrap_or(id)
}

/// Read the text content of the first `<tag ...>...</tag>` element.
///
/// Attribute-tolerant (matches `<tag` then the next `>`). Returns None when the
/// tag is absent or its trimmed content is empty.
pub fn read_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let after_open = xml[start..].find('>')?;
    let text_start = start + after_open + 1;
    let end = xml[text_start..].find(&close)?;
    let text = xml[text_start..text_start + end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Replace the text content of the first `<tag ...>...</tag>` element, escaping
/// XML special characters in `new_value`. Returns the input unchanged when the
/// tag is not found.
pub fn write_tag(xml: &str, tag: &str, new_value: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    if let Some(start) = xml.find(&open)
        && let Some(after_open) = xml[start..].find('>')
    {
        let text_start = start + after_open + 1;
        if let Some(end) = xml[text_start..].find(&close) {
            let escaped = new_value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            return format!(
                "{}{}{}",
                &xml[..text_start],
                escaped,
                &xml[text_start + end..]
            );
        }
    }
    xml.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_urn_uuid() {
        assert_eq!(strip_urn_uuid("urn:uuid:abc-123"), "abc-123");
        assert_eq!(strip_urn_uuid("abc-123"), "abc-123");
    }

    #[test]
    fn test_read_tag() {
        let xml =
            r#"<Root><Id>urn:uuid:x</Id><Title lang="en">Hello</Title><Empty></Empty></Root>"#;
        assert_eq!(read_tag(xml, "Id").as_deref(), Some("urn:uuid:x"));
        assert_eq!(read_tag(xml, "Title").as_deref(), Some("Hello"));
        assert_eq!(read_tag(xml, "Empty"), None);
        assert_eq!(read_tag(xml, "Missing"), None);
    }

    #[test]
    fn test_write_tag() {
        let xml = "<Root><Title>Old</Title></Root>";
        assert_eq!(
            write_tag(xml, "Title", "New"),
            "<Root><Title>New</Title></Root>"
        );
        assert_eq!(
            write_tag(xml, "Title", "a<b&c"),
            "<Root><Title>a&lt;b&amp;c</Title></Root>"
        );
        // unknown tag leaves input unchanged
        assert_eq!(write_tag(xml, "Missing", "x"), xml);
    }
}
