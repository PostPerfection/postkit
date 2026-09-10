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

// an OPL's <CompositionPlaylistId> must not match
pub fn is_composition_playlist(xml: &str) -> bool {
    regex::Regex::new(r"<(?:\w+:)?CompositionPlaylist[\s>]").is_ok_and(|root| root.is_match(xml))
}

pub fn read_prefixed_tag(xml: &str, name: &str) -> Option<String> {
    let pattern = format!(r"<(?:\w+:)?{name}(?:\s[^>]*)?>([^<]*)");
    let found = regex::Regex::new(&pattern).ok()?.captures(xml)?;
    let text = found[1].trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Replace the text content of the first `<tag ...>...</tag>` element, escaping
/// XML special characters in `new_value`. Returns the input unchanged when the
/// tag is not found.
pub fn write_tag(xml: &str, tag: &str, new_value: &str) -> String {
    replace_tag(xml, tag, new_value).unwrap_or_else(|| xml.to_string())
}

/// Same as [`write_tag`], but None when the tag is absent, so a caller can tell
/// a rewritten element from an untouched document.
pub fn replace_tag(xml: &str, tag: &str, new_value: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    let start = xml.find(&open)?;
    let after_open = xml[start..].find('>')?;
    let text_start = start + after_open + 1;
    let end = xml[text_start..].find(&close)?;
    Some(format!(
        "{}{}{}",
        &xml[..text_start],
        crate::packaging::escape_xml(new_value),
        &xml[text_start + end..]
    ))
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
    fn read_prefixed_tag_reads_past_the_prefix_and_the_xmlns() {
        let xml = "<app2e:MaxCLL xmlns:app2e=\"http://www.smpte-ra.org/ns/2067-21/2020\">993\
                   </app2e:MaxCLL><MaxFALL>362</MaxFALL><Empty></Empty>";
        assert_eq!(read_prefixed_tag(xml, "MaxCLL").as_deref(), Some("993"));
        assert_eq!(read_prefixed_tag(xml, "MaxFALL").as_deref(), Some("362"));
        assert_eq!(read_prefixed_tag(xml, "Empty"), None);
        assert_eq!(read_prefixed_tag(xml, "MaxCL"), None);
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
