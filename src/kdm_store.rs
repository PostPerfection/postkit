use crate::certificate::{KdmMetadata, KdmWindow, distinguished_name, parse_kdm};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KDM_EXTENSION: &str = "xml";
const NAME_CLASH_SEPARATOR: &str = "-";

/// A KDM file in the store and the public metadata read from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredKdm {
    pub path: PathBuf,
    pub metadata: KdmMetadata,
}

/// Whether a KDM can open its composition at a given time on a given player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KdmFit {
    Valid,
    NotYetValid,
    Expired,
    WrongRecipient,
}

/// How a KDM fits a player whose certificate has `recipient_subject_name` at
/// `now`. A KDM that names no recipient fits on its validity window alone.
pub fn fit(
    metadata: &KdmMetadata,
    now: chrono::DateTime<chrono::Utc>,
    recipient_subject_name: &str,
) -> KdmFit {
    // both names are RFC 4514 as distinguished_name renders them
    let names_another_recipient = metadata
        .recipient_subject_name
        .as_deref()
        .is_some_and(|name| name.trim() != recipient_subject_name.trim());
    if names_another_recipient {
        return KdmFit::WrongRecipient;
    }
    match window(metadata) {
        Ok(window) if now < window.not_before => KdmFit::NotYetValid,
        Ok(window) if now > window.not_after => KdmFit::Expired,
        Ok(_) => KdmFit::Valid,
        // a window that cannot be read never opens
        Err(_) => KdmFit::Expired,
    }
}

/// The subject DN of a PEM certificate, spelled the way a KDM names it in
/// Recipient X509SubjectName.
pub fn recipient_subject_name(certificate: &Path) -> Result<String, String> {
    use x509_parser::prelude::*;

    let data = std::fs::read(certificate)
        .map_err(|e| format!("cannot read certificate {}: {e}", certificate.display()))?;
    let (_, pem) = parse_x509_pem(&data).map_err(|e| {
        format!(
            "certificate {} is not valid PEM: {e}",
            certificate.display()
        )
    })?;
    let parsed = pem.parse_x509().map_err(|e| {
        format!(
            "certificate {} is not valid X.509: {e}",
            certificate.display()
        )
    })?;
    Ok(distinguished_name(parsed.subject()))
}

/// The KDMs kept in one directory, sorted by path.
#[derive(Debug, Clone)]
pub struct KdmStore {
    directory: PathBuf,
    kdms: Vec<StoredKdm>,
}

impl KdmStore {
    /// Read every `*.xml` directly in `directory`. Returns the store and the
    /// files it could not read, each with the reason. A missing directory is
    /// an empty store.
    pub fn load(directory: &Path) -> (KdmStore, Vec<(PathBuf, String)>) {
        let mut store = KdmStore {
            directory: directory.to_path_buf(),
            kdms: Vec::new(),
        };
        let mut unreadable = Vec::new();
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (store, unreadable),
            Err(e) => {
                let reason = format!("cannot list KDM directory {}: {e}", directory.display());
                unreadable.push((directory.to_path_buf(), reason));
                return (store, unreadable);
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    let reason = format!("cannot list KDM directory {}: {e}", directory.display());
                    unreadable.push((directory.to_path_buf(), reason));
                    continue;
                }
            };
            if !path.is_file() || !has_kdm_extension(&path) {
                continue;
            }
            match read_kdm(&path) {
                Ok(metadata) => store.kdms.push(StoredKdm { path, metadata }),
                Err(reason) => unreadable.push((path, reason)),
            }
        }
        store.kdms.sort_by(|left, right| left.path.cmp(&right.path));
        unreadable.sort();
        (store, unreadable)
    }

    /// Every KDM in the store, sorted by path.
    pub fn kdms(&self) -> &[StoredKdm] {
        &self.kdms
    }

    /// Copy the KDM at `file` into the store under its own file name, with a
    /// numeric suffix when another KDM already has that name. Refuses a KDM
    /// for the same composition and window as one already stored.
    pub fn ingest(&mut self, file: &Path) -> Result<StoredKdm, String> {
        let metadata = read_kdm(file)?;
        if let Some(stored) = self
            .kdms
            .iter()
            .find(|stored| same_kdm(&stored.metadata, &metadata))
        {
            return Err(format!(
                "{} is the same KDM as {}: same composition and validity window",
                file.display(),
                stored.path.display()
            ));
        }
        std::fs::create_dir_all(&self.directory).map_err(|e| {
            format!(
                "cannot create KDM directory {}: {e}",
                self.directory.display()
            )
        })?;
        let path = self.free_path(file)?;
        std::fs::copy(file, &path)
            .map_err(|e| format!("cannot copy {} to {}: {e}", file.display(), path.display()))?;
        let stored = StoredKdm { path, metadata };
        let position = self
            .kdms
            .partition_point(|existing| existing.path < stored.path);
        self.kdms.insert(position, stored.clone());
        Ok(stored)
    }

    /// Delete a stored KDM's file and drop it from the store. A path the store
    /// does not hold is refused, so nothing outside it is deleted.
    pub fn remove(&mut self, path: &Path) -> Result<(), String> {
        let position = self
            .kdms
            .iter()
            .position(|stored| stored.path == path)
            .ok_or_else(|| format!("{} is not in the KDM store", path.display()))?;
        std::fs::remove_file(path)
            .map_err(|e| format!("cannot delete KDM {}: {e}", path.display()))?;
        self.kdms.remove(position);
        Ok(())
    }

    /// Every stored KDM for `cpl_id`, in store order, with how it fits a
    /// player whose certificate has `recipient_subject_name` at `now`.
    pub fn kdms_for(
        &self,
        cpl_id: uuid::Uuid,
        now: chrono::DateTime<chrono::Utc>,
        recipient_subject_name: &str,
    ) -> Vec<(&StoredKdm, KdmFit)> {
        self.kdms
            .iter()
            .filter(|stored| stored.metadata.cpl_id == cpl_id)
            .map(|stored| (stored, fit(&stored.metadata, now, recipient_subject_name)))
            .collect()
    }

    /// The KDM `cpl_id` plays with: of those that are Valid, the one whose
    /// window ends last, the first in store order on a tie.
    pub fn kdm_for_playback(
        &self,
        cpl_id: uuid::Uuid,
        now: chrono::DateTime<chrono::Utc>,
        recipient_subject_name: &str,
    ) -> Option<&StoredKdm> {
        self.kdms_for(cpl_id, now, recipient_subject_name)
            .into_iter()
            .filter(|(_, kdm_fit)| *kdm_fit == KdmFit::Valid)
            .map(|(stored, _)| stored)
            .min_by_key(|stored| {
                std::cmp::Reverse(window(&stored.metadata).ok().map(|window| window.not_after))
            })
    }

    fn free_path(&self, file: &Path) -> Result<PathBuf, String> {
        let name = file
            .file_name()
            .ok_or_else(|| format!("{} has no file name", file.display()))?;
        let mut name = PathBuf::from(name);
        if !has_kdm_extension(&name) {
            name.as_mut_os_string().push(format!(".{KDM_EXTENSION}"));
        }
        let candidate = self.directory.join(&name);
        if !candidate.exists() {
            return Ok(candidate);
        }
        let stem = name
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let extension = name
            .extension()
            .map(|extension| extension.to_string_lossy().into_owned())
            .unwrap_or_default();
        let suffixed = (1..)
            .map(|suffix| {
                self.directory
                    .join(format!("{stem}{NAME_CLASH_SEPARATOR}{suffix}.{extension}"))
            })
            .find(|candidate| !candidate.exists())
            .expect("an unbounded range always has a free suffix");
        Ok(suffixed)
    }
}

fn has_kdm_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(KDM_EXTENSION))
}

fn window(metadata: &KdmMetadata) -> Result<KdmWindow, String> {
    KdmWindow::parse(&metadata.not_valid_before, &metadata.not_valid_after)
}

fn read_kdm(path: &Path) -> Result<KdmMetadata, String> {
    let xml = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read KDM {}: {e}", path.display()))?;
    let metadata = parse_kdm(&xml).map_err(|e| format!("{}: {e}", path.display()))?;
    window(&metadata).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(metadata)
}

fn same_kdm(left: &KdmMetadata, right: &KdmMetadata) -> bool {
    left.cpl_id == right.cpl_id
        && left.not_valid_before == right.not_valid_before
        && left.not_valid_after == right.not_valid_after
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certificate::{KdmConfig, KdmContentKey, KdmFormulation, build_kdm, generate_chain};
    use chrono::{DateTime, Duration, SubsecRound, Utc};
    use std::sync::OnceLock;

    const KDM_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S+00:00";
    const COMPOSITION: &str = "5d1c0a2e-7b3f-4c8d-9e6a-1f2b3c4d5e6f";
    const OTHER_COMPOSITION: &str = "0e9d8c7b-6a5f-4e3d-8c2b-1a0f9e8d7c6b";
    const PICTURE_KEY_ID: &str = "8f2c6a10-3b4d-4e5f-8a6b-7c8d9e0f1a2b";
    const CONTENT_KEY: [u8; 16] = [0x5A; 16];
    const PLAYER_ORGANIZATION: &str = "Acme";
    const OTHER_PLAYER_ORGANIZATION: &str = "Bolt";
    const PLAYER_LEAF_COMMON_NAME: &str = "CN=CS.Acme.smpte-430-2.LEAF";
    const DAYS_UNTIL_NOW: i64 = 6;

    const EXPIRED_FILE: &str = "a_expired.xml";
    const VALID_EARLY_FILE: &str = "b_valid_early.xml";
    const VALID_LATE_FILE: &str = "c_valid_late.xml";
    const NOT_YET_VALID_FILE: &str = "d_not_yet_valid.xml";
    const OTHER_RECIPIENT_FILE: &str = "e_other_recipient.xml";
    const OTHER_COMPOSITION_FILE: &str = "f_other_composition.xml";
    const BROKEN_FILE: &str = "g_broken.xml";
    const NOTES_FILE: &str = "notes.txt";
    const NESTED_DIRECTORY: &str = "archive";

    struct Chain {
        _directory: tempfile::TempDir,
        root: PathBuf,
        root_key: PathBuf,
        leaf: PathBuf,
    }

    fn chain(organization: &str) -> Chain {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(generate_chain(organization, directory.path()), 0);
        Chain {
            root: directory.path().join("root.pem"),
            root_key: directory.path().join("root.key"),
            leaf: directory.path().join("signer.pem"),
            _directory: directory,
        }
    }

    fn player() -> &'static Chain {
        static PLAYER: OnceLock<Chain> = OnceLock::new();
        PLAYER.get_or_init(|| chain(PLAYER_ORGANIZATION))
    }

    fn other_player() -> &'static Chain {
        static OTHER_PLAYER: OnceLock<Chain> = OnceLock::new();
        OTHER_PLAYER.get_or_init(|| chain(OTHER_PLAYER_ORGANIZATION))
    }

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).unwrap()
    }

    fn timestamp(base: DateTime<Utc>, day: i64) -> String {
        (base + Duration::days(day))
            .format(KDM_TIMESTAMP_FORMAT)
            .to_string()
    }

    // signed by the recipient's own root, so the chain needs no intermediate
    fn kdm_xml(
        recipient: &Chain,
        cpl_id: &str,
        base: DateTime<Utc>,
        first_day: i64,
        last_day: i64,
    ) -> String {
        let config = KdmConfig {
            cpl_id: cpl_id.to_string(),
            content_title: "Store Feature".to_string(),
            recipient_cert_file: recipient.leaf.clone(),
            signer_cert_file: recipient.root.clone(),
            signer_key_file: recipient.root_key.clone(),
            valid_from: timestamp(base, first_day),
            valid_to: timestamp(base, last_day),
            formulation: KdmFormulation::DciAny,
            content_keys: vec![KdmContentKey {
                key_type: *b"MDIK",
                key_id: uuid(PICTURE_KEY_ID),
                content_key: CONTENT_KEY,
            }],
            ..Default::default()
        };
        build_kdm(&config).unwrap().xml
    }

    struct StoreFixture {
        directory: tempfile::TempDir,
        base: DateTime<Utc>,
        player_subject_name: String,
    }

    impl StoreFixture {
        fn path(&self, name: &str) -> PathBuf {
            self.directory.path().join(name)
        }

        fn now(&self) -> DateTime<Utc> {
            self.base + Duration::days(DAYS_UNTIL_NOW)
        }
    }

    fn store_fixture() -> StoreFixture {
        let directory = tempfile::tempdir().unwrap();
        let base = Utc::now().trunc_subsecs(0);
        let kdms = [
            (EXPIRED_FILE, player(), COMPOSITION, 2, 4),
            (VALID_EARLY_FILE, player(), COMPOSITION, 3, 7),
            (VALID_LATE_FILE, player(), COMPOSITION, 5, 9),
            (NOT_YET_VALID_FILE, player(), COMPOSITION, 10, 14),
            (OTHER_RECIPIENT_FILE, other_player(), COMPOSITION, 5, 8),
            (OTHER_COMPOSITION_FILE, player(), OTHER_COMPOSITION, 5, 9),
        ];
        for (name, recipient, cpl_id, first_day, last_day) in kdms {
            let xml = kdm_xml(recipient, cpl_id, base, first_day, last_day);
            std::fs::write(directory.path().join(name), xml).unwrap();
        }
        std::fs::write(directory.path().join(BROKEN_FILE), "<not a kdm").unwrap();
        std::fs::write(directory.path().join(NOTES_FILE), "not a kdm").unwrap();
        let nested = directory.path().join(NESTED_DIRECTORY);
        std::fs::create_dir(&nested).unwrap();
        std::fs::copy(
            directory.path().join(VALID_LATE_FILE),
            nested.join(VALID_LATE_FILE),
        )
        .unwrap();
        StoreFixture {
            directory,
            base,
            player_subject_name: recipient_subject_name(&player().leaf).unwrap(),
        }
    }

    fn file_names(kdms: &[StoredKdm]) -> Vec<String> {
        kdms.iter()
            .map(|stored| {
                stored
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn load_reads_every_kdm_directly_in_the_directory_and_names_the_unreadable_ones() {
        let fixture = store_fixture();
        let (store, unreadable) = KdmStore::load(fixture.directory.path());

        assert_eq!(
            file_names(store.kdms()),
            [
                EXPIRED_FILE,
                VALID_EARLY_FILE,
                VALID_LATE_FILE,
                NOT_YET_VALID_FILE,
                OTHER_RECIPIENT_FILE,
                OTHER_COMPOSITION_FILE,
            ]
        );
        assert_eq!(store.kdms()[0].metadata.cpl_id, uuid(COMPOSITION));
        assert_eq!(
            store.kdms()[0].metadata.not_valid_after,
            timestamp(fixture.base, 4)
        );
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].0, fixture.path(BROKEN_FILE));
        assert!(unreadable[0].1.contains(BROKEN_FILE), "{}", unreadable[0].1);
    }

    #[test]
    fn a_missing_directory_is_an_empty_store() {
        let directory = tempfile::tempdir().unwrap();
        let (store, unreadable) = KdmStore::load(&directory.path().join("absent"));
        assert!(store.kdms().is_empty());
        assert!(unreadable.is_empty());
    }

    #[test]
    fn kdms_for_fits_each_kdm_of_the_composition_against_the_player_and_the_time() {
        let fixture = store_fixture();
        let (store, _) = KdmStore::load(fixture.directory.path());

        let fits: Vec<(String, KdmFit)> = store
            .kdms_for(
                uuid(COMPOSITION),
                fixture.now(),
                &fixture.player_subject_name,
            )
            .into_iter()
            .map(|(stored, kdm_fit)| {
                let name = stored.path.file_name().unwrap().to_string_lossy();
                (name.into_owned(), kdm_fit)
            })
            .collect();
        assert_eq!(
            fits,
            [
                (EXPIRED_FILE.to_string(), KdmFit::Expired),
                (VALID_EARLY_FILE.to_string(), KdmFit::Valid),
                (VALID_LATE_FILE.to_string(), KdmFit::Valid),
                (NOT_YET_VALID_FILE.to_string(), KdmFit::NotYetValid),
                (OTHER_RECIPIENT_FILE.to_string(), KdmFit::WrongRecipient),
            ]
        );
    }

    #[test]
    fn a_kdm_naming_no_recipient_fits_on_its_window_alone() {
        let fixture = store_fixture();
        let (store, _) = KdmStore::load(fixture.directory.path());
        let other_recipient = store
            .kdms()
            .iter()
            .find(|stored| stored.path == fixture.path(OTHER_RECIPIENT_FILE))
            .unwrap();
        let mut unnamed = other_recipient.metadata.clone();
        unnamed.recipient_subject_name = None;

        assert_eq!(
            fit(&unnamed, fixture.now(), &fixture.player_subject_name),
            KdmFit::Valid
        );
        let window_end = fixture.base + Duration::days(8);
        assert_eq!(
            fit(&unnamed, window_end, &fixture.player_subject_name),
            KdmFit::Valid
        );
        assert_eq!(
            fit(
                &unnamed,
                window_end + Duration::seconds(1),
                &fixture.player_subject_name
            ),
            KdmFit::Expired
        );
    }

    #[test]
    fn kdm_for_playback_picks_the_valid_kdm_that_ends_last() {
        let fixture = store_fixture();
        let (store, _) = KdmStore::load(fixture.directory.path());

        let picked = store
            .kdm_for_playback(
                uuid(COMPOSITION),
                fixture.now(),
                &fixture.player_subject_name,
            )
            .unwrap();
        assert_eq!(picked.path, fixture.path(VALID_LATE_FILE));

        let before_any = fixture.base + Duration::days(1);
        assert!(
            store
                .kdm_for_playback(uuid(COMPOSITION), before_any, &fixture.player_subject_name)
                .is_none()
        );
    }

    #[test]
    fn ingest_refuses_a_duplicate_and_suffixes_a_name_clash() {
        let fixture = store_fixture();
        let (mut store, _) = KdmStore::load(fixture.directory.path());
        let incoming = tempfile::tempdir().unwrap();

        let duplicate = incoming.path().join("resent.xml");
        std::fs::copy(fixture.path(VALID_LATE_FILE), &duplicate).unwrap();
        let error = store.ingest(&duplicate).unwrap_err();
        assert!(
            error.contains(&fixture.path(VALID_LATE_FILE).display().to_string()),
            "{error}"
        );
        assert!(!fixture.path("resent.xml").exists());

        let clashing = incoming.path().join(VALID_LATE_FILE);
        let xml = kdm_xml(player(), COMPOSITION, fixture.base, 6, 12);
        std::fs::write(&clashing, &xml).unwrap();
        let stored = store.ingest(&clashing).unwrap();

        let suffixed = fixture.path("c_valid_late-1.xml");
        assert_eq!(stored.path, suffixed);
        assert_eq!(stored.metadata.not_valid_after, timestamp(fixture.base, 12));
        assert_eq!(std::fs::read_to_string(&suffixed).unwrap(), xml);
        assert_eq!(
            file_names(store.kdms()),
            [
                EXPIRED_FILE,
                VALID_EARLY_FILE,
                "c_valid_late-1.xml",
                VALID_LATE_FILE,
                NOT_YET_VALID_FILE,
                OTHER_RECIPIENT_FILE,
                OTHER_COMPOSITION_FILE,
            ]
        );
        let picked = store
            .kdm_for_playback(
                uuid(COMPOSITION),
                fixture.now(),
                &fixture.player_subject_name,
            )
            .unwrap();
        assert_eq!(picked.path, suffixed);
    }

    #[test]
    fn remove_deletes_the_file_and_the_entry() {
        let fixture = store_fixture();
        let (mut store, _) = KdmStore::load(fixture.directory.path());

        store.remove(&fixture.path(VALID_LATE_FILE)).unwrap();

        assert!(!fixture.path(VALID_LATE_FILE).exists());
        assert_eq!(
            file_names(store.kdms()),
            [
                EXPIRED_FILE,
                VALID_EARLY_FILE,
                NOT_YET_VALID_FILE,
                OTHER_RECIPIENT_FILE,
                OTHER_COMPOSITION_FILE,
            ]
        );
        let picked = store
            .kdm_for_playback(
                uuid(COMPOSITION),
                fixture.now(),
                &fixture.player_subject_name,
            )
            .unwrap();
        assert_eq!(picked.path, fixture.path(VALID_EARLY_FILE));

        let outside = fixture.path(NOTES_FILE);
        let error = store.remove(&outside).unwrap_err();
        assert!(error.contains(NOTES_FILE), "{error}");
        assert!(outside.exists());
    }

    #[test]
    fn recipient_subject_name_matches_the_kdm_made_for_that_certificate() {
        let fixture = store_fixture();
        let (store, _) = KdmStore::load(fixture.directory.path());
        let subject_name_of = |name: &str| {
            store
                .kdms()
                .iter()
                .find(|stored| stored.path == fixture.path(name))
                .and_then(|stored| stored.metadata.recipient_subject_name.clone())
                .unwrap()
        };

        assert!(
            fixture
                .player_subject_name
                .contains(PLAYER_LEAF_COMMON_NAME),
            "{}",
            fixture.player_subject_name
        );
        assert_eq!(
            subject_name_of(VALID_LATE_FILE),
            fixture.player_subject_name
        );
        let other_subject_name = recipient_subject_name(&other_player().leaf).unwrap();
        assert_eq!(subject_name_of(OTHER_RECIPIENT_FILE), other_subject_name);
        assert_ne!(other_subject_name, fixture.player_subject_name);
    }
}
