use super::cinema::{AuthorizedDevice, CertSource, Cinema, Screen};
use super::window::LocalWindow;
use crate::certificate::{
    CertOptions, CertType, KdmConfig, KdmContentKey, build_kdm, generate_certificate,
    generate_chain,
};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const VENDOR: &str = "Vendor";
pub const DISTRIBUTOR: &str = "Distributor";
pub const CPL_ID: &str = "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9";
pub const DCNC_TITLE: &str =
    "LongerThanFourteenTitle_FTR-1-3D_F-185_EN-XX_US-13_51-HI-VI_2K_STU_20261001_FAC_SMPTE_OV";
const KDM_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S+00:00";

pub struct Device {
    pub certificate: PathBuf,
    pub key: PathBuf,
}

pub struct Fixtures {
    _directory: tempfile::TempDir,
    pub vendor_root: PathBuf,
    pub vendor_intermediate: PathBuf,
    pub distributor_signer: PathBuf,
    pub distributor_signer_key: PathBuf,
    pub distributor_chain: Vec<PathBuf>,
    pub security_managers: Vec<Device>,
    pub link_decryptor: Device,
    pub projector: Device,
}

fn leaf(directory: &Path, stem: &str, common_name: &str) -> Device {
    let device = Device {
        certificate: directory.join(format!("{stem}.pem")),
        key: directory.join(format!("{stem}.key")),
    };
    let options = CertOptions {
        cert_type: CertType::Leaf,
        common_name: common_name.to_string(),
        organization: VENDOR.to_string(),
        output_cert: device.certificate.clone(),
        output_key: device.key.clone(),
        issuer_cert: directory.join("intermediate.pem"),
        issuer_key: directory.join("intermediate.key"),
        ..Default::default()
    };
    assert_eq!(generate_certificate(&options), 0, "{stem} certificate");
    device
}

// RSA key generation is slow, so every test shares one set
pub fn fixtures() -> &'static Fixtures {
    static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let directory = tempfile::tempdir().expect("tempdir");
        let vendor = directory.path().join("vendor");
        let distributor = directory.path().join("distributor");
        assert_eq!(generate_chain(VENDOR, &vendor), 0, "vendor chain");
        assert_eq!(
            generate_chain(DISTRIBUTOR, &distributor),
            0,
            "distributor chain"
        );
        let security_managers = (1..=3)
            .map(|index| {
                leaf(
                    &vendor,
                    &format!("sm{index}"),
                    &format!("SM.{VENDOR}.IMB.100{index}"),
                )
            })
            .collect();
        Fixtures {
            vendor_root: vendor.join("root.pem"),
            vendor_intermediate: vendor.join("intermediate.pem"),
            distributor_signer: distributor.join("signer.pem"),
            distributor_signer_key: distributor.join("signer.key"),
            distributor_chain: vec![
                distributor.join("intermediate.pem"),
                distributor.join("root.pem"),
            ],
            security_managers,
            link_decryptor: leaf(&vendor, "ld", &format!("LD.{VENDOR}.LDB.2001")),
            projector: leaf(&vendor, "pr", &format!("PR.{VENDOR}.PRJ.3001")),
            _directory: directory,
        }
    })
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

impl Fixtures {
    pub fn vendor_chain_pem(&self, leaf: &Path) -> String {
        format!(
            "{}{}{}",
            read(leaf),
            read(&self.vendor_intermediate),
            read(&self.vendor_root)
        )
    }

    pub fn signer(&self) -> super::issue::KdmSigner {
        super::issue::KdmSigner {
            certificate: self.distributor_signer.clone(),
            key: self.distributor_signer_key.clone(),
            chain: self.distributor_chain.clone(),
        }
    }
}

pub fn content_keys() -> Vec<KdmContentKey> {
    vec![
        KdmContentKey {
            key_type: *b"MDIK",
            key_id: uuid::Uuid::from_u128(0x1111),
            content_key: [0x11; 16],
        },
        KdmContentKey {
            key_type: *b"MDAK",
            key_id: uuid::Uuid::from_u128(0x2222),
            content_key: [0x22; 16],
        },
    ]
}

// a DKDM to the distributor's own signer certificate, as a mastering facility would send
pub fn dkdm(content_title: &str, days: i64) -> String {
    let f = fixtures();
    let start = chrono::Utc::now() + chrono::Duration::days(1);
    let end = start + chrono::Duration::days(days);
    build_kdm(&KdmConfig {
        cpl_id: CPL_ID.to_string(),
        content_title: content_title.to_string(),
        recipient_cert_file: f.distributor_signer.clone(),
        signer_cert_file: f.distributor_signer.clone(),
        signer_key_file: f.distributor_signer_key.clone(),
        signer_chain_files: f.distributor_chain.clone(),
        valid_from: start.format(KDM_TIMESTAMP_FORMAT).to_string(),
        valid_to: end.format(KDM_TIMESTAMP_FORMAT).to_string(),
        content_keys: content_keys(),
        ..Default::default()
    })
    .expect("DKDM")
    .xml
}

pub fn certificate_base64(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct FlmDeviceSpec<'a> {
    pub device_type: &'a str,
    pub serial: &'a str,
    // each certificate gets its own X509Data, in the order given
    pub chain_pems: Vec<String>,
}

fn device_xml(device: &FlmDeviceSpec<'_>) -> String {
    let certificates: String = device
        .chain_pems
        .iter()
        .map(|pem| {
            format!(
                "<ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data>",
                certificate_base64(pem)
            )
        })
        .collect();
    format!(
        r#"<Device>
  <DeviceTypeID scope="http://www.smpte-ra.org/schemas/433/2008/dcmlTypes/#device-type-tokens">{device_type}</DeviceTypeID>
  <DeviceIdentifier idtype="DeviceUID">urn:uuid:{identifier}</DeviceIdentifier>
  <DeviceSerial>{serial}</DeviceSerial>
  <Manufacturer>{VENDOR}</Manufacturer>
  <ModelNumber>M-1</ModelNumber>
  <IsActive>true</IsActive>
  <KeyInfoList><ds:KeyInfo>{certificates}</ds:KeyInfo></KeyInfoList>
  <Capabilities/>
</Device>"#,
        device_type = device.device_type,
        serial = device.serial,
        identifier = uuid::Uuid::new_v4(),
    )
}

// auditoriums of suites of devices, laid out per the ST 430-16 schema
pub fn extended_flm(
    facility_name: &str,
    time_zone: &str,
    auditoriums: &[(&str, Vec<Vec<FlmDeviceSpec<'_>>>)],
) -> String {
    let auditorium_xml: String = auditoriums
        .iter()
        .map(|(name, suites)| {
            let suite_xml: String = suites
                .iter()
                .map(|devices| {
                    let devices: String = devices.iter().map(device_xml).collect();
                    format!("<Suite>{devices}</Suite>")
                })
                .collect();
            format!(
                "<Auditorium><AuditoriumNumberOrName>{name}</AuditoriumNumberOrName>\
                 <SuiteList>{suite_xml}</SuiteList></Auditorium>"
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FacilityListMessage xmlns="http://www.smpte-ra.org/ns/430-16/2017/FLM" xmlns:ds="http://www.w3.org/2000/09/xmldsig#">
  <MessageId>urn:uuid:{message_id}</MessageId>
  <IssueDate>2026-10-01T10:00:00+00:00</IssueDate>
  <FacilityInfo>
    <FacilityID>urn:x-facilityID:example.com:{facility_name}</FacilityID>
    <FacilityName>{facility_name}</FacilityName>
    <FacilityTimeZone>{time_zone}</FacilityTimeZone>
    <Circuit>Independent</Circuit>
    <ContactList>
      <Contact><Name>Projection Booth</Name><Email>booth@cinema.test</Email><Type>Projectionist</Type></Contact>
    </ContactList>
    <AddressList>
      <Physical>
        <StreetAddress>1 Screen Street</StreetAddress>
        <City>London</City>
        <Province>Greater London</Province>
        <PostalCode>N1 1AA</PostalCode>
        <Country>GB</Country>
      </Physical>
    </AddressList>
    <Capabilities>
      <KDMDeliveryMethodList><DeliveryMethod><Email><EmailAddress>kdm@cinema.test</EmailAddress></Email></DeliveryMethod></KDMDeliveryMethodList>
    </Capabilities>
  </FacilityInfo>
  <AuditoriumList>{auditorium_xml}</AuditoriumList>
</FacilityListMessage>"#,
        message_id = uuid::Uuid::new_v4(),
    )
}

pub fn chain_screen(
    name: &str,
    serial: &str,
    recipient: &Path,
    devices: &[(&str, &Path)],
) -> Screen {
    let f = fixtures();
    let mut screen = Screen::new(name, CertSource::Inline(f.vendor_chain_pem(recipient))).unwrap();
    screen.device_serial = Some(serial.to_string());
    screen.authorized_devices = devices
        .iter()
        .map(|(device_type, leaf)| AuthorizedDevice {
            device_type: device_type.to_string(),
            serial: None,
            certificate: f.vendor_chain_pem(leaf),
        })
        .collect();
    screen
}

pub fn cinemas() -> (Cinema, Cinema) {
    let f = fixtures();
    let rex = Cinema {
        name: "Rex".into(),
        emails: vec!["kdm@rex.test".into()],
        time_zone: Some("Europe/London".into()),
        screens: vec![
            chain_screen(
                "1",
                "1001",
                &f.security_managers[0].certificate,
                &[
                    ("LD", &f.link_decryptor.certificate),
                    ("PR", &f.projector.certificate),
                ],
            ),
            chain_screen("2", "1002", &f.security_managers[1].certificate, &[]),
        ],
        ..Default::default()
    };
    let odeon = Cinema {
        name: "Odeon".into(),
        emails: vec!["kdm@odeon.test".into()],
        time_zone: Some("America/New_York".into()),
        screens: vec![chain_screen(
            "A",
            "1003",
            &f.security_managers[2].certificate,
            &[],
        )],
        ..Default::default()
    };
    (rex, odeon)
}

pub fn local_window() -> LocalWindow {
    let today = chrono::Utc::now().date_naive();
    LocalWindow {
        start: (today + chrono::Duration::days(2))
            .and_hms_opt(18, 0, 0)
            .unwrap(),
        end: (today + chrono::Duration::days(9))
            .and_hms_opt(23, 0, 0)
            .unwrap(),
    }
}
