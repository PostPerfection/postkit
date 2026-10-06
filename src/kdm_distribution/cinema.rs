use super::flm::{
    Address, Contact, FacilityListMessage, FlmDevice, FlmVersion, KdmSuite, parse_flm,
};
use crate::certificate::{CertInfo, cert_info_from_file, cert_info_from_pem, chain_to_pem};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CertSource {
    Path(PathBuf),
    // PEM text, leaf first when it carries the whole chain
    Inline(String),
}

impl CertSource {
    pub fn materialize(&self, tmp_dir: &Path) -> Result<PathBuf, String> {
        match self {
            CertSource::Path(p) => Ok(p.clone()),
            CertSource::Inline(pem) => {
                let path = tmp_dir.join(format!("{}.pem", uuid::Uuid::new_v4()));
                std::fs::write(&path, pem).map_err(|e| format!("cannot write temp cert: {e}"))?;
                Ok(path)
            }
        }
    }

    pub fn pem(&self) -> Result<String, String> {
        match self {
            CertSource::Path(path) => std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read certificate {}: {e}", path.display())),
            CertSource::Inline(pem) => Ok(pem.clone()),
        }
    }
}

const PEM_BEGIN_MARKER: &str = "-----BEGIN ";
const PEM_DELIMITER: &str = "-----";
const CERTIFICATE_PEM_LABEL: &str = "CERTIFICATE";

fn refuse_non_certificate_pem_blocks(pem: &str) -> Result<(), String> {
    for line in pem.lines() {
        let Some((_, after_begin_marker)) = line.split_once(PEM_BEGIN_MARKER) else {
            continue;
        };
        let label = after_begin_marker
            .split_once(PEM_DELIMITER)
            .map_or(after_begin_marker, |(label, _)| label)
            .trim();
        if label != CERTIFICATE_PEM_LABEL {
            return Err(format!(
                "the inline certificate holds a PEM block labelled {label}, only {CERTIFICATE_PEM_LABEL} blocks can be stored"
            ));
        }
    }
    Ok(())
}

// a device the KDM lists in its AuthorizedDeviceInfo, besides the recipient
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthorizedDevice {
    pub device_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    // PEM, leaf first
    pub certificate: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Screen {
    pub name: String,
    pub cert: CertSource,
    // cached from the certificate for search, not authoritative key material
    pub cert_serial: String,
    pub cert_thumbprint: String,
    pub cert_subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_serial: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorized_devices: Vec<AuthorizedDevice>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Cinema {
    pub name: String,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub screens: Vec<Screen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facility_id: Option<String>,
    // IANA time zone name, the zone KDM windows are written in
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<Contact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CinemaDb {
    #[serde(default)]
    pub cinemas: Vec<Cinema>,
}

pub struct Recipient {
    pub cinema: String,
    pub emails: Vec<String>,
    pub screen: String,
    pub cert_path: PathBuf,
}

fn cert_info(cert: &CertSource) -> Result<CertInfo, String> {
    match cert {
        CertSource::Path(p) => cert_info_from_file(p),
        CertSource::Inline(pem) => cert_info_from_pem(pem),
    }
}

impl Screen {
    // the certificate is untrusted input, so it is parsed here and refused if it is not X.509
    pub fn new(name: &str, cert: CertSource) -> Result<Self, String> {
        if let CertSource::Inline(pem) = &cert {
            refuse_non_certificate_pem_blocks(pem)?;
        }
        let info = cert_info(&cert)?;
        Ok(Screen {
            name: name.to_string(),
            cert,
            cert_serial: info.serial,
            cert_thumbprint: info.thumbprint,
            cert_subject: info.subject_cn,
            device_serial: None,
            authorized_devices: Vec::new(),
        })
    }
}

fn device_chain_pem(device: &FlmDevice) -> Option<String> {
    device
        .certificate_chains
        .first()
        .map(|chain| chain_to_pem(chain))
}

fn screen_from_suite(name: String, suite: &KdmSuite<'_>) -> Result<Screen, String> {
    let recipient = device_chain_pem(suite.security_manager).ok_or_else(|| {
        format!("screen '{name}': the SM device carries no certificate chain in the FLM")
    })?;
    let mut screen = Screen::new(&name, CertSource::Inline(recipient))?;
    screen.device_serial = suite.security_manager.serial.clone();
    for device in &suite.authorized_devices {
        let Some(certificate) = device_chain_pem(device) else {
            continue;
        };
        screen.authorized_devices.push(AuthorizedDevice {
            device_type: device.device_type.clone().unwrap_or_default(),
            serial: device.serial.clone(),
            certificate,
        });
    }
    Ok(screen)
}

fn extended_flm_screens(flm: &FacilityListMessage) -> Result<Vec<Screen>, String> {
    let suites = flm.kdm_suites()?;
    let mut screens = Vec::new();
    for suite in &suites {
        let suites_in_auditorium = suites
            .iter()
            .filter(|other| other.auditorium == suite.auditorium)
            .count();
        let name = match &suite.security_manager.serial {
            Some(serial) if suites_in_auditorium > 1 => format!("{} ({serial})", suite.auditorium),
            _ => suite.auditorium.to_string(),
        };
        screens.push(screen_from_suite(name, suite)?);
    }
    Ok(screens)
}

// ST 430-7 has no suites, so every device with a certificate becomes a screen
fn original_flm_screens(flm: &FacilityListMessage) -> Result<Vec<Screen>, String> {
    let mut screens = Vec::new();
    for auditorium in &flm.auditoriums {
        let devices: Vec<&FlmDevice> = auditorium
            .suites
            .iter()
            .flat_map(|suite| &suite.devices)
            .collect();
        for device in devices
            .iter()
            .filter(|device| !device.certificate_chains.is_empty())
        {
            let name = match &device.serial {
                Some(serial) if devices.len() > 1 => {
                    format!("{} ({serial})", auditorium.number_or_name)
                }
                _ => auditorium.number_or_name.clone(),
            };
            let chain = &device.certificate_chains[0];
            let leaf_is_first = cert_info_from_pem(&chain_to_pem(&chain[..1]))
                .map(|info| !info.is_ca)
                .unwrap_or(false);
            let pem = if leaf_is_first {
                chain_to_pem(chain)
            } else {
                chain
                    .iter()
                    .map(|certificate| chain_to_pem(std::slice::from_ref(certificate)))
                    .find(|pem| cert_info_from_pem(pem).is_ok_and(|info| !info.is_ca))
                    .ok_or_else(|| {
                        format!("screen '{name}' has no usable leaf certificate in the FLM")
                    })?
            };
            let mut screen = Screen::new(&name, CertSource::Inline(pem))?;
            screen.device_serial = device.serial.clone();
            screens.push(screen);
        }
    }
    if screens.is_empty() {
        return Err("FLM has no auditoriums with device certificates".to_string());
    }
    Ok(screens)
}

pub fn cinema_from_flm(flm: &FacilityListMessage) -> Result<Cinema, String> {
    let screens = match flm.version {
        FlmVersion::Extended => extended_flm_screens(flm)?,
        FlmVersion::Original => original_flm_screens(flm)?,
    };
    Ok(Cinema {
        name: flm.facility.name.clone(),
        emails: flm.emails(),
        notes: String::new(),
        screens,
        facility_id: flm.facility.id.clone(),
        time_zone: flm.facility.time_zone.clone(),
        contacts: flm.facility.contacts.clone(),
        address: flm.facility.physical_address.clone(),
    })
}

pub fn read_flm_cinema(flm_path: &Path) -> Result<Cinema, String> {
    let xml = std::fs::read_to_string(flm_path)
        .map_err(|e| format!("cannot read FLM {}: {e}", flm_path.display()))?;
    cinema_from_flm(&parse_flm(&xml)?)
}

impl CinemaDb {
    // corrupt json fails loud rather than silently discarding cinemas
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut db: Self = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| format!("cannot parse cinema db {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read cinema db {}: {e}", path.display())),
        };
        // list and search have to keep working on a store that cannot be written
        if db.refresh_cached_cert_fields()
            && let Err(e) = db.save(path)
        {
            tracing::warn!(
                "cinema db {} holds outdated certificate details and could not be rewritten: {e}",
                path.display()
            );
        }
        Ok(db)
    }

    // a screen whose certificate is gone keeps its cached values, the only record of which it was
    fn refresh_cached_cert_fields(&mut self) -> bool {
        let mut changed = false;
        for cinema in &mut self.cinemas {
            for screen in &mut cinema.screens {
                let Ok(info) = cert_info(&screen.cert) else {
                    continue;
                };
                if screen.cert_serial != info.serial
                    || screen.cert_thumbprint != info.thumbprint
                    || screen.cert_subject != info.subject_cn
                {
                    screen.cert_serial = info.serial;
                    screen.cert_thumbprint = info.thumbprint;
                    screen.cert_subject = info.subject_cn;
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| format!("serialize db: {e}"))?;
        crate::fs::write_atomic(path, &json)
    }

    pub fn find(&self, name: &str) -> Option<&Cinema> {
        self.cinemas.iter().find(|c| c.name == name)
    }

    pub fn add_cinema(
        &mut self,
        name: &str,
        emails: Vec<String>,
        notes: String,
    ) -> Result<(), String> {
        if self.find(name).is_some() {
            return Err(format!("cinema '{name}' already exists"));
        }
        self.cinemas.push(Cinema {
            name: name.to_string(),
            emails,
            notes,
            ..Default::default()
        });
        Ok(())
    }

    pub fn remove_cinema(&mut self, name: &str) -> Result<(), String> {
        let before = self.cinemas.len();
        self.cinemas.retain(|c| c.name != name);
        if self.cinemas.len() == before {
            return Err(format!("cinema '{name}' not found"));
        }
        Ok(())
    }

    pub fn add_screen(
        &mut self,
        cinema: &str,
        screen: &str,
        cert: CertSource,
    ) -> Result<(), String> {
        let s = Screen::new(screen, cert)?;
        let c = self
            .cinemas
            .iter_mut()
            .find(|c| c.name == cinema)
            .ok_or_else(|| format!("cinema '{cinema}' not found"))?;
        if c.screens.iter().any(|x| x.name == screen) {
            return Err(format!("screen '{screen}' already exists in '{cinema}'"));
        }
        c.screens.push(s);
        Ok(())
    }

    pub fn remove_screen(&mut self, cinema: &str, screen: &str) -> Result<(), String> {
        let c = self
            .cinemas
            .iter_mut()
            .find(|c| c.name == cinema)
            .ok_or_else(|| format!("cinema '{cinema}' not found"))?;
        let before = c.screens.len();
        c.screens.retain(|s| s.name != screen);
        if c.screens.len() == before {
            return Err(format!("screen '{screen}' not found in '{cinema}'"));
        }
        Ok(())
    }

    // case-insensitive match on cinema, screen, certificate serial, thumbprint or subject
    pub fn search(&self, query: &str) -> Vec<(String, String)> {
        let q = query.to_lowercase();
        let mut hits = Vec::new();
        for c in &self.cinemas {
            let cinema_match = c.name.to_lowercase().contains(&q);
            for s in &c.screens {
                if cinema_match
                    || s.name.to_lowercase().contains(&q)
                    || s.cert_serial.to_lowercase().contains(&q)
                    || s.cert_thumbprint.to_lowercase().contains(&q)
                    || s.cert_subject.to_lowercase().contains(&q)
                {
                    hits.push((c.name.clone(), s.name.clone()));
                }
            }
            if cinema_match && c.screens.is_empty() {
                hits.push((c.name.clone(), String::new()));
            }
        }
        hits
    }

    // replaces any cinema of the same name
    pub fn import_flm(&mut self, flm_path: &Path) -> Result<String, String> {
        let cinema = read_flm_cinema(flm_path)?;
        self.cinemas.retain(|c| c.name != cinema.name);
        let summary = format!("{} ({} screens)", cinema.name, cinema.screens.len());
        self.cinemas.push(cinema);
        Ok(summary)
    }

    // --cinema names and --screen "cinema/screen" specs, inline certs written under tmp_dir
    pub fn resolve(
        &self,
        cinemas: &[String],
        screens: &[String],
        tmp_dir: &Path,
    ) -> Result<Vec<Recipient>, String> {
        let mut out = Vec::new();
        for name in cinemas {
            let c = self
                .find(name)
                .ok_or_else(|| format!("cinema '{name}' not found in db"))?;
            if c.screens.is_empty() {
                return Err(format!("cinema '{name}' has no screens"));
            }
            for s in &c.screens {
                out.push(Recipient {
                    cinema: c.name.clone(),
                    emails: c.emails.clone(),
                    screen: s.name.clone(),
                    cert_path: s.cert.materialize(tmp_dir)?,
                });
            }
        }
        for spec in screens {
            let (cn, sn) = spec
                .split_once('/')
                .ok_or_else(|| format!("--screen must be 'cinema/screen', got '{spec}'"))?;
            let c = self
                .find(cn)
                .ok_or_else(|| format!("cinema '{cn}' not found in db"))?;
            let s = c
                .screens
                .iter()
                .find(|s| s.name == sn)
                .ok_or_else(|| format!("screen '{sn}' not found in '{cn}'"))?;
            out.push(Recipient {
                cinema: c.name.clone(),
                emails: c.emails.clone(),
                screen: s.name.clone(),
                cert_path: s.cert.materialize(tmp_dir)?,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::test_support::{
        FlmDeviceSpec, certificate_base64, extended_flm, fixtures, read,
    };

    // a copy, so a test can delete or lock it without touching the shared fixture
    fn leaf_cert(dir: &Path, stem: &str) -> PathBuf {
        let copy = dir.join(format!("{stem}.pem"));
        std::fs::copy(&fixtures().security_managers[0].certificate, &copy).unwrap();
        copy
    }

    #[test]
    fn add_search_and_persist_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "screen1");
        let info = cert_info_from_file(&cert).unwrap();

        let db_path = dir.path().join("cinemas.json");
        let mut db = CinemaDb::default();
        db.add_cinema("Odeon", vec!["ops@odeon.test".into()], "notes".into())
            .unwrap();
        db.add_screen("Odeon", "Screen 1", CertSource::Path(cert.clone()))
            .unwrap();
        db.save(&db_path).unwrap();

        let loaded = CinemaDb::load(&db_path).unwrap();
        assert_eq!(loaded.cinemas.len(), 1);
        assert_eq!(loaded.cinemas[0].screens[0].cert_serial, info.serial);
        assert_eq!(
            loaded.search(&info.serial),
            vec![("Odeon".to_string(), "Screen 1".to_string())]
        );
        assert_eq!(loaded.search("odeon").len(), 1);
        assert!(loaded.search("nonexistent").is_empty());
    }

    #[test]
    fn a_legacy_database_file_is_written_back_without_the_new_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "screen1");
        let mut db = CinemaDb::default();
        db.add_cinema("Odeon", vec![], String::new()).unwrap();
        db.add_screen("Odeon", "Screen 1", CertSource::Path(cert))
            .unwrap();
        let json = serde_json::to_string(&db).unwrap();
        for field in [
            "time_zone",
            "facility_id",
            "authorized_devices",
            "device_serial",
        ] {
            assert!(
                !json.contains(field),
                "{field} written for a legacy cinema: {json}"
            );
        }
    }

    #[test]
    fn duplicate_and_missing_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "s");
        let mut db = CinemaDb::default();
        db.add_cinema("A", vec![], String::new()).unwrap();
        assert!(db.add_cinema("A", vec![], String::new()).is_err());
        db.add_screen("A", "S1", CertSource::Path(cert.clone()))
            .unwrap();
        assert!(
            db.add_screen("A", "S1", CertSource::Path(cert.clone()))
                .is_err()
        );
        assert!(
            db.add_screen("Missing", "S1", CertSource::Path(cert))
                .is_err()
        );
        assert!(db.remove_cinema("Missing").is_err());
    }

    #[test]
    fn bad_cert_rejected_on_add() {
        let mut db = CinemaDb::default();
        db.add_cinema("A", vec![], String::new()).unwrap();
        let r = db.add_screen("A", "S1", CertSource::Inline("not a cert".into()));
        assert!(r.is_err());
    }

    #[test]
    fn an_inline_certificate_is_refused_with_a_private_key_and_stored_as_is_without() {
        let f = fixtures();
        let cert_pem = read(&f.security_managers[0].certificate);
        let key_pem = read(&f.security_managers[0].key);
        let mut db = CinemaDb::default();
        db.add_cinema("A", vec![], String::new()).unwrap();

        let error = db
            .add_screen(
                "A",
                "S1",
                CertSource::Inline(format!("{cert_pem}{key_pem}")),
            )
            .unwrap_err();
        assert!(error.contains("PRIVATE KEY"), "{error}");
        assert!(db.cinemas[0].screens.is_empty());

        db.add_screen("A", "S1", CertSource::Inline(cert_pem.clone()))
            .unwrap();
        assert_eq!(db.cinemas[0].screens[0].cert, CertSource::Inline(cert_pem));
    }

    #[test]
    fn import_of_an_original_flm_picks_the_real_leaf_cert() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "dev");
        let b64 = certificate_base64(&read(&cert));
        let flm = format!(
            r#"<?xml version="1.0"?>
<flm:FacilityListMessage xmlns:flm="http://www.smpte-ra.org/schemas/430-7/20XX/FLM" xmlns:ds="http://www.w3.org/2000/09/xmldsig#">
  <flm:FacilityInfo><flm:FacilityName>Real Cinema</flm:FacilityName>
    <flm:ContactList><flm:Contact><flm:Email>a@real.test</flm:Email></flm:Contact></flm:ContactList>
  </flm:FacilityInfo>
  <flm:AuditoriumList><flm:Auditorium><flm:AuditoriumNumberOrName>1</flm:AuditoriumNumberOrName>
    <flm:SuiteList><flm:Suite><flm:Device><flm:DeviceSerial>1</flm:DeviceSerial>
      <flm:KeyInfoList><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{b64}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></flm:KeyInfoList>
    </flm:Device></flm:Suite></flm:SuiteList>
  </flm:Auditorium></flm:AuditoriumList>
</flm:FacilityListMessage>"#
        );
        let flm_path = dir.path().join("f.xml");
        std::fs::write(&flm_path, flm).unwrap();

        let mut db = CinemaDb::default();
        let summary = db.import_flm(&flm_path).unwrap();
        assert!(summary.contains("Real Cinema"));
        let c = db.find("Real Cinema").unwrap();
        assert_eq!(c.emails, vec!["a@real.test"]);
        assert_eq!(c.screens.len(), 1);
        let info = cert_info_from_file(&cert).unwrap();
        assert_eq!(c.screens[0].cert_serial, info.serial);
        assert!(matches!(c.screens[0].cert, CertSource::Inline(_)));
    }

    #[test]
    fn an_extended_flm_becomes_one_screen_per_suite_with_its_authorized_devices() {
        let f = fixtures();
        let chain = |leaf: &Path| {
            vec![
                read(leaf),
                read(&f.vendor_intermediate),
                read(&f.vendor_root),
            ]
        };
        let device = |device_type, serial, leaf: &Path| FlmDeviceSpec {
            device_type,
            serial,
            chain_pems: chain(leaf),
        };
        let xml = extended_flm(
            "Rex",
            "Europe/London",
            &[
                (
                    "1",
                    vec![vec![
                        device("SM", "1001", &f.security_managers[0].certificate),
                        device("LD", "2001", &f.link_decryptor.certificate),
                        device("PR", "3001", &f.projector.certificate),
                    ]],
                ),
                (
                    "2",
                    vec![
                        vec![device("SM", "1002", &f.security_managers[1].certificate)],
                        vec![device("SM", "1003", &f.security_managers[2].certificate)],
                    ],
                ),
            ],
        );
        let cinema = cinema_from_flm(&parse_flm(&xml).unwrap()).unwrap();
        assert_eq!(cinema.time_zone.as_deref(), Some("Europe/London"));
        assert_eq!(
            cinema.facility_id.as_deref(),
            Some("urn:x-facilityID:example.com:Rex")
        );
        assert_eq!(
            cinema.address.as_ref().unwrap().postal_code.as_deref(),
            Some("N1 1AA")
        );
        let names: Vec<&str> = cinema.screens.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["1", "2 (1002)", "2 (1003)"]);

        let screen = &cinema.screens[0];
        assert_eq!(screen.device_serial.as_deref(), Some("1001"));
        let recipient = cert_info_from_file(&f.security_managers[0].certificate).unwrap();
        assert_eq!(screen.cert_thumbprint, recipient.thumbprint);
        let CertSource::Inline(pem) = &screen.cert else {
            panic!("an FLM screen stores its chain inline");
        };
        assert_eq!(pem.matches("BEGIN CERTIFICATE").count(), 3);
        let device_types: Vec<&str> = screen
            .authorized_devices
            .iter()
            .map(|device| device.device_type.as_str())
            .collect();
        assert_eq!(device_types, vec!["LD", "PR"]);
        assert!(cinema.screens[1].authorized_devices.is_empty());
    }

    #[test]
    fn resolve_cinema_and_screen() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "s");
        let pem = read(&cert);
        let mut db = CinemaDb::default();
        db.add_cinema("A", vec!["a@a.test".into()], String::new())
            .unwrap();
        db.add_screen("A", "S1", CertSource::Path(cert.clone()))
            .unwrap();
        db.add_screen("A", "S2", CertSource::Inline(pem)).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let recips = db.resolve(&["A".into()], &[], tmp.path()).unwrap();
        assert_eq!(recips.len(), 2);
        assert!(recips[0].cert_path.exists());
        assert!(recips[1].cert_path.exists());

        let one = db.resolve(&[], &["A/S1".into()], tmp.path()).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].screen, "S1");
        assert!(db.resolve(&[], &["A/nope".into()], tmp.path()).is_err());
    }

    // the cached fields hold whatever an older build computed
    fn db_with_stale_cache(cert: CertSource) -> CinemaDb {
        let mut screen = Screen::new("Screen 1", cert).unwrap();
        screen.cert_serial = "stale-serial".into();
        screen.cert_thumbprint = "4ca4b493deadbeef".into();
        screen.cert_subject = "stale-subject".into();
        CinemaDb {
            cinemas: vec![Cinema {
                name: "Odeon".into(),
                screens: vec![screen],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn loading_an_old_db_recomputes_the_cached_certificate_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "screen1");
        let info = cert_info_from_file(&cert).unwrap();
        let db_path = dir.path().join("cinemas.json");
        db_with_stale_cache(CertSource::Path(cert))
            .save(&db_path)
            .unwrap();

        let loaded = CinemaDb::load(&db_path).unwrap();
        let screen = &loaded.cinemas[0].screens[0];
        assert_eq!(screen.cert_thumbprint, info.thumbprint);
        assert_eq!(screen.cert_serial, info.serial);
        assert_eq!(screen.cert_subject, info.subject_cn);
        let on_disk = CinemaDb::load(&db_path).unwrap();
        assert_eq!(
            on_disk.cinemas[0].screens[0].cert_thumbprint,
            info.thumbprint
        );
        assert_eq!(loaded.search(&info.thumbprint).len(), 1);
    }

    #[test]
    fn a_screen_whose_certificate_is_gone_keeps_its_cached_values() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "screen1");
        let db_path = dir.path().join("cinemas.json");
        db_with_stale_cache(CertSource::Path(cert.clone()))
            .save(&db_path)
            .unwrap();
        std::fs::remove_file(&cert).unwrap();

        let loaded =
            CinemaDb::load(&db_path).expect("a missing certificate must not fail the load");
        let screen = &loaded.cinemas[0].screens[0];
        assert_eq!(screen.cert_thumbprint, "4ca4b493deadbeef");
        assert_eq!(screen.cert_serial, "stale-serial");
    }

    #[test]
    fn a_second_load_rewrites_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "screen1");
        let db_path = dir.path().join("cinemas.json");
        db_with_stale_cache(CertSource::Path(cert))
            .save(&db_path)
            .unwrap();

        CinemaDb::load(&db_path).unwrap();
        let after_migration = std::fs::read(&db_path).unwrap();
        let modified = std::fs::metadata(&db_path).unwrap().modified().unwrap();

        CinemaDb::load(&db_path).unwrap();
        assert_eq!(std::fs::read(&db_path).unwrap(), after_migration);
        assert_eq!(
            std::fs::metadata(&db_path).unwrap().modified().unwrap(),
            modified,
            "an up-to-date db must not be rewritten at all"
        );
    }

    #[test]
    fn an_unwritable_store_still_loads_and_searches() {
        let dir = tempfile::tempdir().unwrap();
        let cert = leaf_cert(dir.path(), "screen1");
        let info = cert_info_from_file(&cert).unwrap();
        let store_dir = dir.path().join("readonly");
        std::fs::create_dir(&store_dir).unwrap();
        let db_path = store_dir.join("cinemas.json");
        db_with_stale_cache(CertSource::Path(cert))
            .save(&db_path)
            .unwrap();

        // the atomic write needs a temp file in the directory, which a read-only one refuses
        let mut perms = std::fs::metadata(&store_dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(true);
        std::fs::set_permissions(&store_dir, perms).unwrap();

        let loaded = CinemaDb::load(&db_path).expect("a read-only store must still load");
        assert_eq!(
            loaded.cinemas[0].screens[0].cert_thumbprint,
            info.thumbprint
        );
        assert_eq!(loaded.search(&info.thumbprint).len(), 1);

        let mut perms = std::fs::metadata(&store_dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&store_dir, perms).unwrap();
    }
}
