//! xmllint reads an XSD `schemaLocation`, a catalog `uri` and `XML_CATALOG_FILES`
//! as URIs, so a windows path with a drive letter and backslashes never resolves.

use std::path::Path;

/// `path` as a `file:///` URI, with any `..` resolved.
pub fn file_uri(path: &Path) -> String {
    let absolute = path
        .canonicalize()
        .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()));
    let text = absolute.to_string_lossy().replace('\\', "/");
    // windows canonicalize gives back a verbatim \\?\D:\... path
    let without_verbatim_prefix = text.strip_prefix("//?/").unwrap_or(text.as_str());
    format!(
        "file:///{}",
        without_verbatim_prefix.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_becomes_an_absolute_file_uri() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("schemas");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("thing.xsd");
        std::fs::write(&file, "<xs:schema/>").unwrap();

        let uri = file_uri(&nested.join("..").join("schemas").join("thing.xsd"));
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(uri.ends_with("/schemas/thing.xsd"), "{uri}");
        assert!(!uri.contains(".."), "{uri}");
        assert!(!uri.contains('\\'), "{uri}");
    }
}
