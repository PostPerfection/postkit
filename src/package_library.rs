use crate::cpl_xml::{is_composition_playlist, read_prefixed_tag, strip_urn_uuid};
use crate::packaging::ns::{CPL_INTEROP, CPL_SMPTE};
use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SCAN_DEPTH_LIMIT: usize = 4;
const CPL_EXTENSION: &str = "xml";
const ID_ELEMENT: &str = "Id";
const CONTENT_TITLE_ELEMENT: &str = "ContentTitleText";
const DURATION_ELEMENT: &str = "Duration";
const INTRINSIC_DURATION_ELEMENT: &str = "IntrinsicDuration";
const ENTRY_POINT_ELEMENT: &str = "EntryPoint";
const EDIT_RATE_ELEMENT: &str = "EditRate";
// a 3D reel holds its picture in MainStereoscopicPicture
const PICTURE_ASSET_PATTERN: &str = r"(?s)<(?:[\w-]+:)?Main(?:Stereoscopic)?Picture[\s>].*?</(?:[\w-]+:)?Main(?:Stereoscopic)?Picture>";
const KEY_ID_PATTERN: &str = r"<(?:[\w-]+:)?KeyId[\s>/]";

/// The DCP standard a CPL is written to, told apart by its root element's namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Standard {
    Interop,
    Smpte,
}

/// One composition a package holds, read from its CPL alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionEntry {
    pub id: uuid::Uuid,
    pub title: String,
    pub duration_frames: u64,
    pub edit_rate: (u32, u32),
    pub encrypted: bool,
}

/// A package directory and the compositions its CPLs describe, titled after the first one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageEntry {
    pub directory: PathBuf,
    pub title: String,
    pub standard: Standard,
    pub compositions: Vec<CompositionEntry>,
}

/// Where verification of a package stands. The app runs dcpdoctor and stores the outcome here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VerdictState {
    #[default]
    Unverified,
    Verifying,
    Valid,
    Failed,
}

/// The last verification result for a package, with the error count and when it ran.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Verdict {
    pub state: VerdictState,
    pub error_count: u32,
    pub verified_at: Option<String>,
}

/// A package in the library and its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub package: PackageEntry,
    pub verdict: Verdict,
}

/// The packages found under the watched folders, kept in one JSON file and sorted by directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(skip)]
    path: PathBuf,
    entries: Vec<LibraryEntry>,
}

/// What a refresh changed, and every package directory it could not read with the reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub added: usize,
    pub kept: usize,
    pub removed: usize,
    pub unreadable: Vec<(PathBuf, String)>,
}

/// Every directory under each root, the root included, that holds an asset map, at most
/// four levels down and never inside a package already found. Sorted and deduplicated.
pub fn scan(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut packages = Vec::new();
    for root in roots {
        collect_packages(root, 0, &mut packages);
    }
    packages.sort();
    packages.dedup();
    packages
}

fn collect_packages(directory: &Path, depth: usize, packages: &mut Vec<PathBuf>) {
    if crate::assetmap::find(directory).is_some() {
        packages.push(directory.to_path_buf());
        return;
    }
    if depth == SCAN_DEPTH_LIMIT {
        return;
    }
    let Ok(children) = std::fs::read_dir(directory) else {
        return;
    };
    for child in children.flatten() {
        let path = child.path();
        if path.is_dir() {
            collect_packages(&path, depth + 1, packages);
        }
    }
}

/// The package in `directory` as its asset map and CPLs describe it, in asset map order.
/// No track file is opened. A directory with no CPL is an error naming the directory.
pub fn read_package(directory: &Path) -> Result<PackageEntry, String> {
    let assetmap = crate::assetmap::find(directory)
        .ok_or_else(|| format!("{}: no asset map", directory.display()))?;
    let mut compositions = Vec::new();
    let mut standard = None;
    for (_, relative) in crate::assetmap::parse_ordered(&assetmap) {
        let path = directory.join(relative);
        let is_xml = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(CPL_EXTENSION));
        if !is_xml {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !is_composition_playlist(&text) {
            continue;
        }
        let (cpl_standard, composition) =
            read_composition(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        standard.get_or_insert(cpl_standard);
        compositions.push(composition);
    }
    let (Some(standard), Some(first)) = (standard, compositions.first()) else {
        return Err(format!("{}: no composition playlist", directory.display()));
    };
    Ok(PackageEntry {
        directory: directory.to_path_buf(),
        title: first.title.clone(),
        standard,
        compositions,
    })
}

fn read_composition(cpl: &str) -> Result<(Standard, CompositionEntry), String> {
    let standard = cpl_standard(cpl)?;
    let id_text = read_prefixed_tag(cpl, ID_ELEMENT).ok_or("no Id")?;
    let id = uuid::Uuid::parse_str(strip_urn_uuid(&id_text))
        .map_err(|error| format!("Id {id_text}: {error}"))?;
    let stated_title =
        read_prefixed_tag(cpl, CONTENT_TITLE_ELEMENT).ok_or("no ContentTitleText")?;
    let title = quick_xml::escape::unescape(&stated_title)
        .map_err(|error| format!("ContentTitleText: {error}"))?
        .into_owned();
    let pictures = regex::Regex::new(PICTURE_ASSET_PATTERN).map_err(|error| error.to_string())?;
    let picture_assets: Vec<&str> = pictures
        .find_iter(cpl)
        .map(|found| found.as_str())
        .collect();
    let first_picture = picture_assets.first().ok_or("no MainPicture")?;
    let edit_rate = read_prefixed_tag(first_picture, EDIT_RATE_ELEMENT)
        .ok_or("MainPicture has no EditRate")
        .and_then(|text| parse_edit_rate(&text).ok_or("MainPicture EditRate is not two numbers"))?;
    let mut duration_frames = 0;
    for (index, picture) in picture_assets.iter().enumerate() {
        duration_frames += picture_duration(picture)
            .ok_or_else(|| format!("reel {} MainPicture has no Duration", index + 1))?;
    }
    let encrypted = regex::Regex::new(KEY_ID_PATTERN)
        .map_err(|error| error.to_string())?
        .is_match(cpl);
    Ok((
        standard,
        CompositionEntry {
            id,
            title,
            duration_frames,
            edit_rate,
            encrypted,
        },
    ))
}

fn cpl_standard(cpl: &str) -> Result<Standard, String> {
    let namespace = root_namespace(cpl).ok_or("CompositionPlaylist has no namespace")?;
    match namespace.as_str() {
        CPL_INTEROP => Ok(Standard::Interop),
        CPL_SMPTE => Ok(Standard::Smpte),
        other => Err(format!(
            "CompositionPlaylist namespace {other} is not a DCP CPL"
        )),
    }
}

fn root_namespace(xml: &str) -> Option<String> {
    let mut reader = NsReader::from_str(xml);
    loop {
        match reader.read_resolved_event().ok()? {
            (ResolveResult::Bound(namespace), Event::Start(_) | Event::Empty(_)) => {
                return Some(String::from_utf8_lossy(namespace.as_ref()).into_owned());
            }
            (_, Event::Start(_) | Event::Empty(_) | Event::Eof) => return None,
            _ => {}
        }
    }
}

// ST 429-7 makes Duration optional
fn picture_duration(picture: &str) -> Option<u64> {
    if let Some(duration) = element_number(picture, DURATION_ELEMENT) {
        return Some(duration);
    }
    let intrinsic = element_number(picture, INTRINSIC_DURATION_ELEMENT)?;
    let entry_point = element_number(picture, ENTRY_POINT_ELEMENT).unwrap_or(0);
    intrinsic.checked_sub(entry_point)
}

fn element_number(xml: &str, element: &str) -> Option<u64> {
    read_prefixed_tag(xml, element)?.parse().ok()
}

fn parse_edit_rate(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split_whitespace();
    let numerator = parts.next()?.parse().ok()?;
    let denominator = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((numerator, denominator))
}

impl Library {
    /// The library kept in `path`, empty when the file does not exist. A verdict left
    /// Verifying loads as Unverified because the app that was verifying has stopped.
    pub fn load(path: &Path) -> Result<Library, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Library {
                    path: path.to_path_buf(),
                    entries: Vec::new(),
                });
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let mut library: Library =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        library.path = path.to_path_buf();
        library
            .entries
            .sort_by(|left, right| left.package.directory.cmp(&right.package.directory));
        for entry in &mut library.entries {
            if entry.verdict.state == VerdictState::Verifying {
                entry.verdict = Verdict::default();
            }
        }
        Ok(library)
    }

    /// Write the library to the file it was loaded from.
    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        crate::fs::write_atomic(&self.path, &json)
    }

    /// Add or replace the entry for the package's directory. The verdict stays when the
    /// directory was listed with the same composition ids, otherwise it is Unverified.
    pub fn upsert(&mut self, package: PackageEntry) {
        match self.position(&package.directory) {
            Ok(index) => {
                let entry = &mut self.entries[index];
                let same_compositions = entry
                    .package
                    .compositions
                    .iter()
                    .map(|composition| composition.id)
                    .eq(package
                        .compositions
                        .iter()
                        .map(|composition| composition.id));
                if !same_compositions {
                    entry.verdict = Verdict::default();
                }
                entry.package = package;
            }
            Err(index) => self.entries.insert(
                index,
                LibraryEntry {
                    package,
                    verdict: Verdict::default(),
                },
            ),
        }
    }

    /// Drop the entry for `directory`, if it is listed.
    pub fn remove(&mut self, directory: &Path) {
        if let Ok(index) = self.position(directory) {
            self.entries.remove(index);
        }
    }

    /// Store the verdict for a listed package. An error when `directory` is not listed.
    pub fn set_verdict(&mut self, directory: &Path, verdict: Verdict) -> Result<(), String> {
        let index = self
            .position(directory)
            .map_err(|_| format!("{} is not in the library", directory.display()))?;
        self.entries[index].verdict = verdict;
        Ok(())
    }

    /// The entry for `directory`, if it is listed.
    pub fn entry(&self, directory: &Path) -> Option<&LibraryEntry> {
        let index = self.position(directory).ok()?;
        Some(&self.entries[index])
    }

    /// Every entry, sorted by directory.
    pub fn entries(&self) -> &[LibraryEntry] {
        &self.entries
    }

    fn position(&self, directory: &Path) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|entry| entry.package.directory.as_path().cmp(directory))
    }
}

/// Scan `roots` and bring the library in line: every package that reads is added or
/// updated, and an entry whose directory no longer holds a package that reads is dropped.
/// A package that cannot be read is reported and the rest still refresh.
pub fn refresh(library: &mut Library, roots: &[PathBuf]) -> RefreshReport {
    let mut report = RefreshReport::default();
    let mut readable = Vec::new();
    for directory in scan(roots) {
        match read_package(&directory) {
            Ok(package) => {
                if library.entry(&directory).is_some() {
                    report.kept += 1;
                } else {
                    report.added += 1;
                }
                library.upsert(package);
                readable.push(directory);
            }
            Err(message) => report.unreadable.push((directory, message)),
        }
    }
    let listed_before = library.entries.len();
    // sorted because scan sorts
    library
        .entries
        .retain(|entry| readable.binary_search(&entry.package.directory).is_ok());
    report.removed = listed_before - library.entries.len();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packaging::{
        AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, PackingList, PklAsset, ns,
    };

    const PACKING_LIST_ID: &str = "0b0b0000-0000-0000-0000-000000000000";
    const ASSETMAP_ID: &str = "0a0a0000-0000-0000-0000-000000000000";
    const FEATURE_ID: &str = "11111111-0000-0000-0000-000000000000";
    const TRAILER_ID: &str = "22222222-0000-0000-0000-000000000000";
    const REPLACEMENT_ID: &str = "33333333-0000-0000-0000-000000000000";
    const TRAILER_KEY_ID: &str = "4e4e0000-0000-0000-0000-000000000000";
    const VERIFIED_AT: &str = "2026-10-06T12:00:00Z";

    struct TestComposition {
        id: &'static str,
        title: &'static str,
        frames_per_second: u32,
        reel_durations: &'static [u64],
        key_id: Option<&'static str>,
    }

    const FEATURE: TestComposition = TestComposition {
        id: FEATURE_ID,
        title: "Feature & Credits",
        frames_per_second: 24,
        reel_durations: &[240, 480, 120],
        key_id: None,
    };

    const TRAILER: TestComposition = TestComposition {
        id: TRAILER_ID,
        title: "Trailer",
        frames_per_second: 25,
        reel_durations: &[150, 100],
        key_id: Some(TRAILER_KEY_ID),
    };

    struct Namespaces {
        cpl: &'static str,
        packing_list: &'static str,
        assetmap: &'static str,
        assetmap_file: &'static str,
    }

    const SMPTE: Namespaces = Namespaces {
        cpl: ns::CPL_SMPTE,
        packing_list: ns::PKL_SMPTE,
        assetmap: ns::AM_SMPTE,
        assetmap_file: "ASSETMAP.xml",
    };

    const INTEROP: Namespaces = Namespaces {
        cpl: ns::CPL_INTEROP,
        packing_list: ns::PKL_INTEROP,
        assetmap: ns::AM_INTEROP,
        assetmap_file: "ASSETMAP",
    };

    fn cpl_file(id: &str) -> String {
        format!("CPL_{id}.xml")
    }

    fn write_package(directory: &Path, namespaces: &Namespaces, compositions: &[TestComposition]) {
        std::fs::create_dir_all(directory).unwrap();
        let packing_list_file = format!("PKL_{PACKING_LIST_ID}.xml");
        let mut assets = vec![AssetMapAsset {
            id: PACKING_LIST_ID.into(),
            path: packing_list_file.clone(),
            packing_list: true,
        }];
        let mut packing_list_assets = Vec::new();
        for composition in compositions {
            let reels = composition
                .reel_durations
                .iter()
                .enumerate()
                .map(|(index, &duration)| DcpCplReel {
                    reel_id: format!("aaaaaaaa-0000-0000-0000-00000000000{index}"),
                    picture_id: format!("bbbbbbbb-0000-0000-0000-00000000000{index}"),
                    picture_edit_rate_num: composition.frames_per_second,
                    picture_edit_rate_den: 1,
                    picture_duration: duration,
                    picture_width: 1998,
                    picture_height: 1080,
                    picture_key_id: composition.key_id.map(str::to_string),
                    ..Default::default()
                })
                .collect();
            let cpl = DcpCpl {
                uuid: composition.id.into(),
                namespace: namespaces.cpl.into(),
                title: composition.title.into(),
                reels,
                ..Default::default()
            };
            std::fs::write(directory.join(cpl_file(composition.id)), cpl.to_xml()).unwrap();
            assets.push(AssetMapAsset {
                id: composition.id.into(),
                path: cpl_file(composition.id),
                packing_list: false,
            });
            packing_list_assets.push(PklAsset {
                id: composition.id.into(),
                asset_type: "text/xml".into(),
                ..Default::default()
            });
        }
        let packing_list = PackingList {
            uuid: PACKING_LIST_ID.into(),
            namespace: namespaces.packing_list.into(),
            assets: packing_list_assets,
            ..Default::default()
        };
        std::fs::write(directory.join(packing_list_file), packing_list.to_xml()).unwrap();
        let assetmap = AssetMap {
            uuid: ASSETMAP_ID.into(),
            namespace: namespaces.assetmap.into(),
            assets,
            ..Default::default()
        };
        std::fs::write(directory.join(namespaces.assetmap_file), assetmap.to_xml()).unwrap();
    }

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).unwrap()
    }

    fn valid_verdict() -> Verdict {
        Verdict {
            state: VerdictState::Valid,
            error_count: 0,
            verified_at: Some(VERIFIED_AT.into()),
        }
    }

    #[test]
    fn a_nested_root_finds_a_package_three_levels_down_and_nothing_inside_it() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("site").join("2026").join("feature");
        write_package(&package, &SMPTE, &[FEATURE]);
        write_package(&package.join("supplement"), &SMPTE, &[TRAILER]);
        std::fs::create_dir_all(root.path().join("empty").join("folder")).unwrap();

        assert_eq!(scan(&[root.path().to_path_buf()]), vec![package]);
    }

    #[test]
    fn the_root_itself_is_a_package_and_overlapping_roots_list_it_once() {
        let root = tempfile::tempdir().unwrap();
        write_package(root.path(), &SMPTE, &[FEATURE]);

        let roots = [root.path().to_path_buf(), root.path().to_path_buf()];
        assert_eq!(scan(&roots), vec![root.path().to_path_buf()]);
    }

    #[test]
    fn a_package_past_the_depth_limit_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        let four_down = root.path().join("a").join("b").join("c").join("four");
        let five_down = root
            .path()
            .join("d")
            .join("e")
            .join("f")
            .join("g")
            .join("five");
        write_package(&four_down, &SMPTE, &[FEATURE]);
        write_package(&five_down, &SMPTE, &[TRAILER]);

        assert_eq!(scan(&[root.path().to_path_buf()]), vec![four_down]);
    }

    #[test]
    fn a_two_cpl_smpte_package_reads_both_compositions() {
        let directory = tempfile::tempdir().unwrap();
        write_package(directory.path(), &SMPTE, &[FEATURE, TRAILER]);

        let package = read_package(directory.path()).unwrap();

        assert_eq!(
            package,
            PackageEntry {
                directory: directory.path().to_path_buf(),
                title: "Feature & Credits".into(),
                standard: Standard::Smpte,
                compositions: vec![
                    CompositionEntry {
                        id: uuid(FEATURE_ID),
                        title: "Feature & Credits".into(),
                        duration_frames: 840,
                        edit_rate: (24, 1),
                        encrypted: false,
                    },
                    CompositionEntry {
                        id: uuid(TRAILER_ID),
                        title: "Trailer".into(),
                        duration_frames: 250,
                        edit_rate: (25, 1),
                        encrypted: true,
                    },
                ],
            }
        );
    }

    #[test]
    fn an_interop_cpl_reads_as_interop() {
        let directory = tempfile::tempdir().unwrap();
        write_package(directory.path(), &INTEROP, &[TRAILER]);

        let package = read_package(directory.path()).unwrap();

        assert_eq!(package.standard, Standard::Interop);
        assert_eq!(package.title, "Trailer");
        assert_eq!(package.compositions[0].duration_frames, 250);
    }

    #[test]
    fn a_3d_cpl_counts_its_stereoscopic_picture() {
        const STEREOSCOPIC_TITLE: &str =
            "ECL44SingleCPL_TST-3D-48_F_XX-XX_UK-U_51_2K_ECL_20180301_ECL_SMPTE-3D_OV";
        const STEREOSCOPIC_CPL_ID: &str = "9be69b0c-db3d-4de2-830d-67099c1f1e08";
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/clairmeta-ecl/DCP/ECL-SET")
            .join("ECL44-SINGLE-CPL_TST-3D-48_F_XX-XX_UK-U_51_2K_ECL_20180301_ECL_SMPTE-3D_OV")
            .join(format!("CPL_{STEREOSCOPIC_TITLE}.xml"));
        let directory = tempfile::tempdir().unwrap();
        std::fs::copy(
            fixture,
            directory.path().join(cpl_file(STEREOSCOPIC_CPL_ID)),
        )
        .unwrap();
        let assetmap = AssetMap {
            uuid: ASSETMAP_ID.into(),
            namespace: ns::AM_SMPTE.into(),
            assets: vec![AssetMapAsset {
                id: STEREOSCOPIC_CPL_ID.into(),
                path: cpl_file(STEREOSCOPIC_CPL_ID),
                packing_list: false,
            }],
            ..Default::default()
        };
        std::fs::write(
            directory.path().join(SMPTE.assetmap_file),
            assetmap.to_xml(),
        )
        .unwrap();

        let package = read_package(directory.path()).unwrap();

        assert_eq!(package.standard, Standard::Smpte);
        assert_eq!(
            package.compositions,
            vec![CompositionEntry {
                id: uuid(STEREOSCOPIC_CPL_ID),
                title: STEREOSCOPIC_TITLE.into(),
                duration_frames: 48,
                edit_rate: (48, 1),
                encrypted: false,
            }]
        );
    }

    #[test]
    fn a_directory_with_no_cpl_is_an_error_naming_it() {
        let directory = tempfile::tempdir().unwrap();
        write_package(directory.path(), &SMPTE, &[]);

        assert_eq!(
            read_package(directory.path()),
            Err(format!(
                "{}: no composition playlist",
                directory.path().display()
            ))
        );
    }

    #[test]
    fn refresh_adds_packages_and_keeps_a_verdict_when_nothing_changed() {
        let root = tempfile::tempdir().unwrap();
        let feature = root.path().join("feature");
        let trailer = root.path().join("trailer");
        write_package(&feature, &SMPTE, &[FEATURE]);
        write_package(&trailer, &INTEROP, &[TRAILER]);
        let mut library = Library::load(&root.path().join("library.json")).unwrap();
        let roots = [root.path().to_path_buf()];

        let first = refresh(&mut library, &roots);
        library.set_verdict(&feature, valid_verdict()).unwrap();
        let second = refresh(&mut library, &roots);

        assert_eq!(
            first,
            RefreshReport {
                added: 2,
                ..Default::default()
            }
        );
        assert_eq!(
            second,
            RefreshReport {
                kept: 2,
                ..Default::default()
            }
        );
        let directories: Vec<&Path> = library
            .entries()
            .iter()
            .map(|entry| entry.package.directory.as_path())
            .collect();
        assert_eq!(directories, vec![feature.as_path(), trailer.as_path()]);
        assert_eq!(library.entry(&feature).unwrap().verdict, valid_verdict());
        assert_eq!(library.entry(&trailer).unwrap().verdict, Verdict::default());
    }

    #[test]
    fn refresh_resets_the_verdict_when_a_cpl_id_changes() {
        let root = tempfile::tempdir().unwrap();
        let feature = root.path().join("feature");
        write_package(&feature, &SMPTE, &[FEATURE]);
        let mut library = Library::load(&root.path().join("library.json")).unwrap();
        let roots = [root.path().to_path_buf()];
        refresh(&mut library, &roots);
        library.set_verdict(&feature, valid_verdict()).unwrap();

        let replacement = TestComposition {
            id: REPLACEMENT_ID,
            ..FEATURE
        };
        write_package(&feature, &SMPTE, &[replacement]);
        let report = refresh(&mut library, &roots);

        assert_eq!(
            report,
            RefreshReport {
                kept: 1,
                ..Default::default()
            }
        );
        let entry = library.entry(&feature).unwrap();
        assert_eq!(entry.verdict, Verdict::default());
        assert_eq!(entry.package.compositions[0].id, uuid(REPLACEMENT_ID));
    }

    #[test]
    fn refresh_removes_a_package_whose_directory_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let feature = root.path().join("feature");
        let trailer = root.path().join("trailer");
        write_package(&feature, &SMPTE, &[FEATURE]);
        write_package(&trailer, &SMPTE, &[TRAILER]);
        let mut library = Library::load(&root.path().join("library.json")).unwrap();
        let roots = [root.path().to_path_buf()];
        refresh(&mut library, &roots);

        std::fs::remove_dir_all(&trailer).unwrap();
        let report = refresh(&mut library, &roots);

        assert_eq!(
            report,
            RefreshReport {
                kept: 1,
                removed: 1,
                ..Default::default()
            }
        );
        assert_eq!(library.entries().len(), 1);
        assert_eq!(library.entries()[0].package.directory, feature);
        assert!(library.entry(&trailer).is_none());
    }

    #[test]
    fn refresh_reports_an_unreadable_directory_by_path_and_reads_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let feature = root.path().join("feature");
        let broken = root.path().join("broken");
        write_package(&feature, &SMPTE, &[FEATURE]);
        write_package(&broken, &SMPTE, &[]);
        let mut library = Library::load(&root.path().join("library.json")).unwrap();

        let report = refresh(&mut library, &[root.path().to_path_buf()]);

        assert_eq!(
            report,
            RefreshReport {
                added: 1,
                unreadable: vec![(
                    broken.clone(),
                    format!("{}: no composition playlist", broken.display())
                )],
                ..Default::default()
            }
        );
        assert_eq!(library.entries().len(), 1);
        assert_eq!(library.entries()[0].package.directory, feature);
    }

    #[test]
    fn the_library_file_round_trips_and_verifying_loads_as_unverified() {
        let root = tempfile::tempdir().unwrap();
        let feature = root.path().join("feature");
        let trailer = root.path().join("trailer");
        write_package(&feature, &SMPTE, &[FEATURE]);
        write_package(&trailer, &SMPTE, &[TRAILER]);
        let library_path = root.path().join("state").join("library.json");
        let mut library = Library::load(&library_path).unwrap();
        assert!(library.entries().is_empty());
        refresh(&mut library, &[root.path().to_path_buf()]);
        let failed = Verdict {
            state: VerdictState::Failed,
            error_count: 3,
            verified_at: Some(VERIFIED_AT.into()),
        };
        library.set_verdict(&feature, failed.clone()).unwrap();
        library
            .set_verdict(
                &trailer,
                Verdict {
                    state: VerdictState::Verifying,
                    ..Default::default()
                },
            )
            .unwrap();

        library.save().unwrap();
        let loaded = Library::load(&library_path).unwrap();

        assert_eq!(loaded.entries().len(), 2);
        assert_eq!(loaded.entry(&feature), library.entry(&feature));
        assert_eq!(loaded.entry(&feature).unwrap().verdict, failed);
        let loaded_trailer = loaded.entry(&trailer).unwrap();
        assert_eq!(
            loaded_trailer.package,
            library.entry(&trailer).unwrap().package
        );
        assert_eq!(loaded_trailer.verdict.state, VerdictState::Unverified);
    }

    #[test]
    fn a_verdict_for_an_unlisted_directory_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let mut library = Library::load(&root.path().join("library.json")).unwrap();
        let missing = root.path().join("missing");

        assert_eq!(
            library.set_verdict(&missing, valid_verdict()),
            Err(format!("{} is not in the library", missing.display()))
        );
    }
}
