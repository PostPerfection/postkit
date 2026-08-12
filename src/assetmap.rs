//! ASSETMAP lookup: an asset uuid to the file that holds it.
//!
//! A package names its track files however it likes, so the ASSETMAP is the only
//! way to get from a CPL asset id to a path. Reading the id out of a filename
//! works on packages one tool built and fails on everything else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The ASSETMAP in this directory, under either name ST 429-9 allows.
pub fn find(dir: &Path) -> Option<PathBuf> {
    ["ASSETMAP.xml", "ASSETMAP"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// Bare lowercased asset uuid to its path relative to the package directory.
/// Empty when the document holds no asset this could read.
pub fn parse(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(asset) = regex::Regex::new(
        r"(?s)<(?:\w+:)?Asset\b.*?<(?:\w+:)?Id>\s*(?:urn:uuid:)?([0-9a-fA-F-]{36})\s*</(?:\w+:)?Id>.*?<(?:\w+:)?Path>\s*([^<]+?)\s*</(?:\w+:)?Path>",
    ) else {
        return HashMap::new();
    };
    asset
        .captures_iter(&text)
        .map(|c| (c[1].to_ascii_lowercase(), c[2].to_string()))
        .collect()
}

/// The file an asset id names, resolved against the package directory.
pub fn resolve(package_dir: &Path, asset_id: &str) -> Option<PathBuf> {
    let relative = parse(&find(package_dir)?).remove(&asset_id.to_ascii_lowercase())?;
    Some(package_dir.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(dir: &Path, assetmap: &str) {
        std::fs::write(dir.join("ASSETMAP.xml"), assetmap).unwrap();
    }

    #[test]
    fn resolves_an_id_to_a_file_named_after_something_else() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"<AssetMap><AssetList>
              <Asset><Id>urn:uuid:11111111-2222-3333-4444-555555555555</Id>
                <ChunkList><Chunk><Path>audio_track.mxf</Path></Chunk></ChunkList></Asset>
            </AssetList></AssetMap>"#,
        );
        std::fs::write(dir.path().join("audio_track.mxf"), b"").unwrap();

        let resolved = resolve(dir.path(), "11111111-2222-3333-4444-555555555555");
        assert_eq!(resolved, Some(dir.path().join("audio_track.mxf")));
    }

    #[test]
    fn id_case_does_not_matter() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"<AssetMap><AssetList>
              <Asset><Id>urn:uuid:AABBCCDD-2222-3333-4444-555555555555</Id>
                <ChunkList><Chunk><Path>sound.mxf</Path></Chunk></ChunkList></Asset>
            </AssetList></AssetMap>"#,
        );
        assert_eq!(
            resolve(dir.path(), "aabbccdd-2222-3333-4444-555555555555"),
            Some(dir.path().join("sound.mxf"))
        );
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        package(
            dir.path(),
            r#"<AssetMap><AssetList>
              <Asset><Id>urn:uuid:11111111-2222-3333-4444-555555555555</Id>
                <ChunkList><Chunk><Path>sound.mxf</Path></Chunk></ChunkList></Asset>
            </AssetList></AssetMap>"#,
        );
        assert_eq!(
            resolve(dir.path(), "99999999-2222-3333-4444-555555555555"),
            None
        );
    }

    #[test]
    fn no_assetmap_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve(dir.path(), "11111111-2222-3333-4444-555555555555"),
            None
        );
    }
}
