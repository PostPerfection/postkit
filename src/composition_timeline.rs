//! A package directory to the one mpv source that plays its whole composition.
//!
//! Picking a picture MXF by filename or size plays a single reel of a
//! multi-reel composition. The CPL is the only document that says which track
//! files belong to the composition and in what order, so resolution goes
//! ASSETMAP → CPL → picture track files and mpv gets them as one EDL timeline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The mpv source that plays every reel of the composition in `package_dir`.
///
/// None when the package has no ASSETMAP, no CPL, or a CPL naming no picture,
/// which leaves the caller on its own single-file fallback.
pub fn mpv_source(package_dir: &Path) -> Option<String> {
    let files = picture_files(package_dir);
    match files.as_slice() {
        [] => None,
        // one reel is the file it always was: an EDL wrapper would change the
        // demuxer and add a chapter for no gain
        [only] => Some(only.to_string_lossy().into_owned()),
        several => Some(edl_uri(several)),
    }
}

/// Every picture track file the composition names, in composition order.
fn picture_files(package_dir: &Path) -> Vec<PathBuf> {
    let Some(assetmap) = crate::assetmap::find(package_dir) else {
        return Vec::new();
    };
    let assets = crate::assetmap::parse_ordered(&assetmap);
    let Some(cpl) = first_cpl(package_dir, &assets) else {
        return Vec::new();
    };
    let path_by_id: HashMap<&str, &str> = assets
        .iter()
        .map(|(id, relative)| (id.as_str(), relative.as_str()))
        .collect();
    picture_asset_ids(&cpl)
        .iter()
        .filter_map(|id| path_by_id.get(id.as_str()))
        .map(|relative| package_dir.join(relative))
        .collect()
}

/// The text of the first CPL in ASSETMAP order. ASSETMAP order is the only
/// order a package states, so a package holding several CPLs resolves to the
/// same one on every run.
fn first_cpl(package_dir: &Path, assets: &[(String, String)]) -> Option<String> {
    // an OPL carries a <CompositionPlaylistId>, so the root element name has to
    // end at the match
    let root = regex::Regex::new(r"<(?:\w+:)?CompositionPlaylist[\s>]").ok()?;
    assets
        .iter()
        .map(|(_, relative)| package_dir.join(relative))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
        })
        .find_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            root.is_match(&text).then_some(text)
        })
}

/// Bare lowercased uuid of every picture track file the CPL names, in
/// composition order: a DCP CPL's reel MainPicture ids (ST 429-7), or an IMF
/// CPL's MainImageSequence resource TrackFileIds (ST 2067-3).
fn picture_asset_ids(cpl: &str) -> Vec<String> {
    let reel_pictures = main_picture_ids(cpl);
    if reel_pictures.is_empty() {
        return image_track_file_ids(cpl);
    }
    reel_pictures
}

/// One MainPicture per reel, so a single forward scan gives reel order.
fn main_picture_ids(cpl: &str) -> Vec<String> {
    let Ok(main_picture) = regex::Regex::new(
        r"(?s)<(?:\w+:)?MainPicture\b.*?<(?:\w+:)?Id>\s*(?:urn:uuid:)?([0-9a-fA-F-]{36})",
    ) else {
        return Vec::new();
    };
    main_picture
        .captures_iter(cpl)
        .map(|found| found[1].to_ascii_lowercase())
        .collect()
}

/// An image sequence may list several resources, so the sequence blocks come
/// first and the track file ids are read inside each one.
fn image_track_file_ids(cpl: &str) -> Vec<String> {
    let Ok(sequence) =
        regex::Regex::new(r"(?s)<(?:\w+:)?MainImageSequence\b.*?</(?:\w+:)?MainImageSequence>")
    else {
        return Vec::new();
    };
    let Ok(track_file) =
        regex::Regex::new(r"<(?:\w+:)?TrackFileId>\s*(?:urn:uuid:)?([0-9a-fA-F-]{36})")
    else {
        return Vec::new();
    };
    sequence
        .find_iter(cpl)
        .flat_map(|block| {
            track_file
                .captures_iter(block.as_str())
                .map(|found| found[1].to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// mpv's inline EDL URI (DOCS/edl-mpv.rst): one segment per file, separated by
/// `;`, played as a single virtual timeline. Every path is length-prefixed as
/// `%<bytes>%<path>` because a bare value may not hold `,`, `;`, newline or `!`.
fn edl_uri(files: &[PathBuf]) -> String {
    let segments: Vec<String> = files
        .iter()
        .map(|file| {
            let path = file.to_string_lossy();
            format!("%{}%{path}", path.len())
        })
        .collect();
    format!("edl://{}", segments.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REEL_UUIDS: [&str; 3] = [
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
        "33333333-3333-3333-3333-333333333333",
    ];

    /// An ASSETMAP listing the CPL first, then each named file in order.
    fn write_assetmap(dir: &Path, cpl_name: &str, assets: &[(&str, &str)]) {
        let mut xml = String::from("<AssetMap><AssetList>");
        xml.push_str(&format!(
            "<Asset><Id>urn:uuid:cc10cc10-0000-0000-0000-000000000000</Id>\
             <ChunkList><Chunk><Path>{cpl_name}</Path></Chunk></ChunkList></Asset>"
        ));
        for (id, path) in assets {
            xml.push_str(&format!(
                "<Asset><Id>urn:uuid:{id}</Id>\
                 <ChunkList><Chunk><Path>{path}</Path></Chunk></ChunkList></Asset>"
            ));
        }
        xml.push_str("</AssetList></AssetMap>");
        std::fs::write(dir.join("ASSETMAP.xml"), xml).unwrap();
    }

    fn dcp_cpl(picture_ids: &[&str]) -> String {
        let reels: String = picture_ids
            .iter()
            .map(|id| {
                format!(
                    "<Reel><AssetList><MainPicture><Id>urn:uuid:{id}</Id>\
                     <Duration>48</Duration></MainPicture></AssetList></Reel>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?>\n<CompositionPlaylist xmlns=\"x\">\
             <Id>urn:uuid:cc10cc10-0000-0000-0000-000000000000</Id>\
             <ReelList>{reels}</ReelList></CompositionPlaylist>"
        )
    }

    fn imf_cpl(track_file_ids: &[&str]) -> String {
        let resources: String = track_file_ids
            .iter()
            .map(|id| {
                format!(
                    "<Resource><Id>urn:uuid:{id}</Id>\
                     <TrackFileId>urn:uuid:{id}</TrackFileId></Resource>"
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\"?>\n<CompositionPlaylist xmlns=\"y\">\
             <SegmentList><Segment><SequenceList>\
             <cc:MainImageSequence xmlns:cc=\"z\"><ResourceList>{resources}</ResourceList>\
             </cc:MainImageSequence>\
             </SequenceList></Segment></SegmentList></CompositionPlaylist>"
        )
    }

    #[test]
    fn every_reel_plays_in_reel_order() {
        let dir = tempfile::tempdir().unwrap();
        // ASSETMAP order deliberately disagrees with reel order
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[
                (REEL_UUIDS[2], "tail.mxf"),
                (REEL_UUIDS[0], "head.mxf"),
                (REEL_UUIDS[1], "feature.mxf"),
            ],
        );
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS)).unwrap();

        assert_eq!(
            picture_files(dir.path()),
            vec![
                dir.path().join("head.mxf"),
                dir.path().join("feature.mxf"),
                dir.path().join("tail.mxf"),
            ]
        );
    }

    #[test]
    fn a_multi_reel_package_becomes_one_edl_timeline() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[(REEL_UUIDS[0], "head.mxf"), (REEL_UUIDS[1], "feature.mxf")],
        );
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS[..2])).unwrap();

        let head = dir.path().join("head.mxf").to_string_lossy().into_owned();
        let feature = dir
            .path()
            .join("feature.mxf")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            mpv_source(dir.path()),
            Some(format!(
                "edl://%{}%{head};%{}%{feature}",
                head.len(),
                feature.len()
            ))
        );
    }

    #[test]
    fn a_single_reel_package_stays_a_plain_path() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "only.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&REEL_UUIDS[..1])).unwrap();

        assert_eq!(
            mpv_source(dir.path()),
            Some(dir.path().join("only.mxf").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn separators_in_a_path_survive_the_length_prefix() {
        let files = [
            PathBuf::from("/dcp/reel,one;a.mxf"),
            PathBuf::from("/dcp/reel two!.mxf"),
        ];
        assert_eq!(
            edl_uri(&files),
            "edl://%19%/dcp/reel,one;a.mxf;%18%/dcp/reel two!.mxf"
        );
    }

    #[test]
    fn the_prefix_counts_bytes_not_characters() {
        let uri = edl_uri(&[PathBuf::from("/dcp/café.mxf")]);
        assert_eq!(uri, "edl://%14%/dcp/café.mxf");
        assert_eq!(uri.strip_prefix("edl://%14%").unwrap().len(), 14);
    }

    #[test]
    fn an_imf_composition_resolves_its_image_track_files() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(
            dir.path(),
            "CPL_a.xml",
            &[
                (REEL_UUIDS[0], "VIDEO_one.mxf"),
                (REEL_UUIDS[1], "VIDEO_two.mxf"),
            ],
        );
        std::fs::write(dir.path().join("CPL_a.xml"), imf_cpl(&REEL_UUIDS[..2])).unwrap();

        assert_eq!(
            picture_files(dir.path()),
            vec![
                dir.path().join("VIDEO_one.mxf"),
                dir.path().join("VIDEO_two.mxf"),
            ]
        );
    }

    #[test]
    fn several_cpls_resolve_to_the_first_in_assetmap_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut xml = String::from("<AssetMap><AssetList>");
        for (id, path) in [
            ("aaaa1111-0000-0000-0000-000000000000", "CPL_second.xml"),
            ("bbbb2222-0000-0000-0000-000000000000", "CPL_first.xml"),
            (REEL_UUIDS[0], "head.mxf"),
            (REEL_UUIDS[1], "feature.mxf"),
        ] {
            xml.push_str(&format!(
                "<Asset><Id>urn:uuid:{id}</Id>\
                 <ChunkList><Chunk><Path>{path}</Path></Chunk></ChunkList></Asset>"
            ));
        }
        xml.push_str("</AssetList></AssetMap>");
        std::fs::write(dir.path().join("ASSETMAP.xml"), xml).unwrap();
        std::fs::write(dir.path().join("CPL_second.xml"), dcp_cpl(&[REEL_UUIDS[1]])).unwrap();
        std::fs::write(dir.path().join("CPL_first.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();

        // CPL_second.xml is listed first, so it wins whatever the names suggest
        assert_eq!(
            picture_files(dir.path()),
            vec![dir.path().join("feature.mxf")]
        );
    }

    #[test]
    fn an_opl_is_not_mistaken_for_a_cpl() {
        let dir = tempfile::tempdir().unwrap();
        let mut xml = String::from("<AssetMap><AssetList>");
        for (id, path) in [
            ("aaaa1111-0000-0000-0000-000000000000", "OPL_a.xml"),
            ("bbbb2222-0000-0000-0000-000000000000", "CPL_a.xml"),
            (REEL_UUIDS[0], "head.mxf"),
        ] {
            xml.push_str(&format!(
                "<Asset><Id>urn:uuid:{id}</Id>\
                 <ChunkList><Chunk><Path>{path}</Path></Chunk></ChunkList></Asset>"
            ));
        }
        xml.push_str("</AssetList></AssetMap>");
        std::fs::write(dir.path().join("ASSETMAP.xml"), xml).unwrap();
        std::fs::write(
            dir.path().join("OPL_a.xml"),
            "<OutputProfileList><CompositionPlaylistId>urn:uuid:x</CompositionPlaylistId>\
             </OutputProfileList>",
        )
        .unwrap();
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();

        assert_eq!(picture_files(dir.path()), vec![dir.path().join("head.mxf")]);
    }

    #[test]
    fn no_assetmap_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("picture.mxf"), b"").unwrap();
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn no_cpl_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "missing.xml", &[(REEL_UUIDS[0], "head.mxf")]);
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn unreadable_cpl_xml_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "head.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), "<CompositionPlaylist").unwrap();
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn a_cpl_naming_no_picture_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[(REEL_UUIDS[0], "sound.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[])).unwrap();
        assert_eq!(mpv_source(dir.path()), None);
    }

    #[test]
    fn a_picture_missing_from_the_assetmap_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        write_assetmap(dir.path(), "CPL_a.xml", &[("dead-beef", "head.mxf")]);
        std::fs::write(dir.path().join("CPL_a.xml"), dcp_cpl(&[REEL_UUIDS[0]])).unwrap();
        assert_eq!(mpv_source(dir.path()), None);
    }
}
