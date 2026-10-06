use crate::certificate::{ChainCertificate, certificate_label, leaf_first};
use serde::{Deserialize, Serialize};

pub const EXTENDED_FLM_NAMESPACE: &str = "http://www.smpte-ra.org/ns/430-16/2017/FLM";
pub const SECURITY_MANAGER_DEVICE_TYPE: &str = "SM";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlmVersion {
    // SMPTE ST 430-16:2017
    Extended,
    // SMPTE ST 430-7:2008 and the ISDCF FLM-x samples written against it
    Original,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternate_phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressee: Option<String>,
    pub street_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub street_address_2: Option<String>,
    pub city: String,
    pub province: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    pub country: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facility {
    pub id: Option<String>,
    pub name: String,
    pub time_zone: Option<String>,
    pub circuit: Option<String>,
    pub contacts: Vec<Contact>,
    pub physical_address: Option<Address>,
    pub kdm_delivery_emails: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlmDevice {
    pub device_type: Option<String>,
    pub identifier: Option<String>,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub is_active: Option<bool>,
    // one entry per ds:KeyInfo, each ordered leaf first
    pub certificate_chains: Vec<Vec<ChainCertificate>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suite {
    pub devices: Vec<FlmDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Auditorium {
    pub number_or_name: String,
    pub suites: Vec<Suite>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacilityListMessage {
    pub version: FlmVersion,
    pub message_id: Option<String>,
    pub issue_date: Option<String>,
    pub facility: Facility,
    pub auditoriums: Vec<Auditorium>,
}

// one ST 430-16 suite: the KDM recipient plus the devices its KDM lists
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KdmSuite<'a> {
    pub auditorium: &'a str,
    pub security_manager: &'a FlmDevice,
    pub authorized_devices: Vec<&'a FlmDevice>,
}

type Node<'a> = roxmltree::Node<'a, 'a>;

fn children<'a>(node: Node<'a>, name: &'a str) -> impl Iterator<Item = Node<'a>> {
    node.children()
        .filter(move |child| child.is_element() && child.tag_name().name() == name)
}

fn child<'a>(node: Node<'a>, name: &'a str) -> Option<Node<'a>> {
    children(node, name).next()
}

// FLM samples put xml comments inside text, which splits the text nodes
fn element_text(node: Node<'_>) -> String {
    node.descendants()
        .filter(|descendant| descendant.is_text())
        .filter_map(|text| text.text())
        .collect::<String>()
        .trim()
        .to_string()
}

fn child_text(node: Node<'_>, name: &str) -> Option<String> {
    child(node, name)
        .map(element_text)
        .filter(|text| !text.is_empty())
}

fn parse_address(node: Node<'_>) -> Address {
    Address {
        addressee: child_text(node, "Addressee"),
        street_address: child_text(node, "StreetAddress").unwrap_or_default(),
        street_address_2: child_text(node, "StreetAddress2"),
        city: child_text(node, "City").unwrap_or_default(),
        province: child_text(node, "Province").unwrap_or_default(),
        postal_code: child_text(node, "PostalCode"),
        country: child_text(node, "Country").unwrap_or_default(),
    }
}

fn parse_contact(node: Node<'_>) -> Contact {
    Contact {
        name: child_text(node, "Name").unwrap_or_default(),
        email: child_text(node, "Email"),
        phone: child_text(node, "Phone1"),
        alternate_phone: child_text(node, "Phone2"),
        country_code: child_text(node, "CountryCode"),
        contact_type: child_text(node, "Type"),
    }
}

fn parse_certificate_chain(
    key_info: Node<'_>,
    device: &str,
) -> Result<Vec<ChainCertificate>, String> {
    use base64::Engine;
    let mut chain = Vec::new();
    for certificate in key_info
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "X509Certificate")
    {
        let base64_text: String = element_text(certificate).split_whitespace().collect();
        if base64_text.is_empty() {
            continue;
        }
        let der = base64::engine::general_purpose::STANDARD
            .decode(base64_text.as_bytes())
            .map_err(|e| format!("device {device}: an X509Certificate is not base64: {e}"))?;
        chain.push(ChainCertificate {
            label: certificate_label(&der, chain.len()),
            der,
        });
    }
    Ok(chain)
}

// a chain with a certificate that does not parse cannot be linked, so the readable leaf goes first
fn leaf_to_front(mut chain: Vec<ChainCertificate>) -> Vec<ChainCertificate> {
    let leaf = chain.iter().position(|certificate| {
        x509_parser::parse_x509_certificate(&certificate.der)
            .is_ok_and(|(_, parsed)| !parsed.is_ca())
    });
    if let Some(index) = leaf {
        let certificate = chain.remove(index);
        chain.insert(0, certificate);
    }
    chain
}

fn parse_device(node: Node<'_>) -> Result<FlmDevice, String> {
    let identifier = child_text(node, "DeviceIdentifier");
    let serial = child_text(node, "DeviceSerial");
    let label = serial
        .clone()
        .or_else(|| identifier.clone())
        .unwrap_or_else(|| "without a serial".to_string());
    let mut certificate_chains = Vec::new();
    for key_info in node
        .descendants()
        .filter(|descendant| descendant.is_element() && descendant.tag_name().name() == "KeyInfo")
    {
        let chain = parse_certificate_chain(key_info, &label)?;
        if chain.is_empty() {
            continue;
        }
        let ordered = leaf_first(chain.clone()).unwrap_or_else(|_| leaf_to_front(chain));
        certificate_chains.push(ordered);
    }
    Ok(FlmDevice {
        device_type: child_text(node, "DeviceTypeID"),
        identifier,
        serial,
        manufacturer: child_text(node, "Manufacturer"),
        model: child_text(node, "ModelNumber"),
        is_active: child_text(node, "IsActive").map(|value| value == "true" || value == "1"),
        certificate_chains,
    })
}

fn kdm_delivery_emails(facility: Node<'_>) -> Vec<String> {
    let mut emails = Vec::new();
    let delivery_lists = facility
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "KDMDeliveryMethodList");
    for list in delivery_lists {
        for email in list
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "EmailAddress")
        {
            let address = element_text(email);
            if !address.is_empty() && !emails.contains(&address) {
                emails.push(address);
            }
        }
    }
    emails
}

pub fn parse_flm(xml: &str) -> Result<FacilityListMessage, String> {
    let document = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            ..Default::default()
        },
    )
    .map_err(|e| format!("FLM is not valid XML: {e}"))?;
    let root = document.root_element();
    if root.tag_name().name() != "FacilityListMessage" {
        return Err(format!(
            "not an FLM document: root element is <{}>, expected <FacilityListMessage>",
            root.tag_name().name()
        ));
    }
    let version = if root.tag_name().namespace() == Some(EXTENDED_FLM_NAMESPACE) {
        FlmVersion::Extended
    } else {
        FlmVersion::Original
    };

    let facility_node = child(root, "FacilityInfo").ok_or("FLM has no FacilityInfo element")?;
    let name =
        child_text(facility_node, "FacilityName").ok_or("FLM FacilityInfo has no FacilityName")?;
    let contacts = facility_node
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Contact")
        .map(parse_contact)
        .collect();
    let physical_address = child(facility_node, "AddressList")
        .and_then(|list| child(list, "Physical"))
        .map(parse_address);
    let facility = Facility {
        id: child_text(facility_node, "FacilityID"),
        name,
        time_zone: child_text(facility_node, "FacilityTimeZone"),
        circuit: child_text(facility_node, "Circuit"),
        contacts,
        physical_address,
        kdm_delivery_emails: kdm_delivery_emails(facility_node),
    };

    let mut auditoriums = Vec::new();
    let auditorium_nodes = root
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Auditorium");
    for (index, auditorium) in auditorium_nodes.enumerate() {
        let number_or_name = child_text(auditorium, "AuditoriumNumberOrName")
            .unwrap_or_else(|| format!("screen-{}", index + 1));
        let mut suites = Vec::new();
        let suite_nodes = child(auditorium, "SuiteList")
            .into_iter()
            .flat_map(|list| children(list, "Suite"));
        for suite in suite_nodes {
            let devices = children(suite, "Device")
                .map(parse_device)
                .collect::<Result<Vec<_>, _>>()?;
            suites.push(Suite { devices });
        }
        // a 430-7 auditorium can group its devices without a SuiteList
        if suites.is_empty() && version == FlmVersion::Original {
            let devices = auditorium
                .descendants()
                .filter(|node| node.is_element() && node.tag_name().name() == "Device")
                .map(parse_device)
                .collect::<Result<Vec<_>, _>>()?;
            suites.push(Suite { devices });
        }
        auditoriums.push(Auditorium {
            number_or_name,
            suites,
        });
    }

    Ok(FacilityListMessage {
        version,
        message_id: child_text(root, "MessageId"),
        issue_date: child_text(root, "IssueDate"),
        facility,
        auditoriums,
    })
}

impl FacilityListMessage {
    // ST 430-16 5.7: every suite holds exactly one SM device, the KDM recipient
    pub fn kdm_suites(&self) -> Result<Vec<KdmSuite<'_>>, String> {
        let mut suites = Vec::new();
        for auditorium in &self.auditoriums {
            for (index, suite) in auditorium.suites.iter().enumerate() {
                let (security_managers, others): (Vec<&FlmDevice>, Vec<&FlmDevice>) =
                    suite.devices.iter().partition(|device| {
                        device.device_type.as_deref() == Some(SECURITY_MANAGER_DEVICE_TYPE)
                    });
                let [security_manager] = security_managers.as_slice() else {
                    return Err(format!(
                        "auditorium {} suite {} holds {} SM devices, ST 430-16 requires exactly one",
                        auditorium.number_or_name,
                        index + 1,
                        security_managers.len()
                    ));
                };
                suites.push(KdmSuite {
                    auditorium: &auditorium.number_or_name,
                    security_manager,
                    authorized_devices: others,
                });
            }
        }
        Ok(suites)
    }

    pub fn emails(&self) -> Vec<String> {
        let mut emails = self.facility.kdm_delivery_emails.clone();
        for email in self
            .facility
            .contacts
            .iter()
            .filter_map(|contact| contact.email.clone())
        {
            if !emails.contains(&email) {
                emails.push(email);
            }
        }
        emails
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::test_support::{FlmDeviceSpec, extended_flm, fixtures, read};

    // the example spells its CommonNames as T61String, which a DN renders in hex
    const LEAF_COMMON_NAME_HEX: &str = "43532E736D7074652D3433302D322E4C454146";
    const ROOT_COMMON_NAME_HEX: &str = "2E736D7074652D3433302D322E524F4F54";
    const SMPTE_EXAMPLE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flm/st430-16b-2017.xml"
    );

    #[test]
    fn the_smpte_example_instance_parses() {
        let flm = parse_flm(&read(std::path::Path::new(SMPTE_EXAMPLE))).unwrap();
        assert_eq!(flm.version, FlmVersion::Extended);
        assert_eq!(
            flm.message_id.as_deref(),
            Some("urn:uuid:48c0194e-1c0b-4ffb-abd7-9d214d7b9d98")
        );
        let facility = &flm.facility;
        assert_eq!(
            facility.id.as_deref(),
            Some("urn:x-facilityID:example.com:XPL:US-1600176-XPL")
        );
        assert_eq!(facility.name, "ExampleFacility Cinema");
        assert_eq!(facility.time_zone.as_deref(), Some("Australia/Melbourne"));
        assert_eq!(facility.contacts.len(), 2);
        assert_eq!(facility.contacts[0].name, "Jane Doe");
        assert_eq!(
            facility.contacts[1].email.as_deref(),
            Some("johndoe@facility.example.com")
        );
        let address = facility.physical_address.as_ref().unwrap();
        assert_eq!(address.city, "South Melbourne");
        assert_eq!(address.country, "AU");
        assert_eq!(
            facility.kdm_delivery_emails,
            vec![
                "kdms_examplefacility@example.com",
                "kdms_examplefacility2@example.com"
            ]
        );

        let suites = flm.kdm_suites().unwrap();
        assert_eq!(suites.len(), 1);
        let security_manager = suites[0].security_manager;
        assert_eq!(suites[0].auditorium, "1");
        assert_eq!(security_manager.serial.as_deref(), Some("000100"));
        assert_eq!(security_manager.model.as_deref(), Some("DS1000"));
        // the sound processor sits in NonSecurityDeviceList, outside the suite
        assert!(suites[0].authorized_devices.is_empty());
        // the published intermediate is cut short, so the leaf is moved up and the rest kept as listed
        let chain = &security_manager.certificate_chains[0];
        assert_eq!(chain.len(), 3);
        assert!(
            chain[0].label.contains(LEAF_COMMON_NAME_HEX),
            "{}",
            chain[0].label
        );
        assert!(
            chain[1].label.contains(ROOT_COMMON_NAME_HEX),
            "{}",
            chain[1].label
        );
        assert_eq!(chain[2].label, "certificate 2");
    }

    #[test]
    fn a_generated_flm_lists_each_suite_with_its_devices_leaf_first() {
        let f = fixtures();
        let root_first = |leaf: &std::path::Path| {
            vec![
                read(&f.vendor_root),
                read(leaf),
                read(&f.vendor_intermediate),
            ]
        };
        let xml = extended_flm(
            "Rex",
            "Europe/London",
            &[(
                "1",
                vec![vec![
                    FlmDeviceSpec {
                        device_type: "SM",
                        serial: "1001",
                        chain_pems: root_first(&f.security_managers[0].certificate),
                    },
                    FlmDeviceSpec {
                        device_type: "LD",
                        serial: "2001",
                        chain_pems: root_first(&f.link_decryptor.certificate),
                    },
                    FlmDeviceSpec {
                        device_type: "PR",
                        serial: "3001",
                        chain_pems: root_first(&f.projector.certificate),
                    },
                ]],
            )],
        );
        let flm = parse_flm(&xml).unwrap();
        assert_eq!(flm.facility.time_zone.as_deref(), Some("Europe/London"));
        assert_eq!(flm.emails(), vec!["kdm@cinema.test", "booth@cinema.test"]);
        let suites = flm.kdm_suites().unwrap();
        assert_eq!(suites.len(), 1);
        let devices: Vec<&str> = suites[0]
            .authorized_devices
            .iter()
            .map(|device| device.device_type.as_deref().unwrap())
            .collect();
        assert_eq!(devices, vec!["LD", "PR"]);
        let chain = &suites[0].security_manager.certificate_chains[0];
        assert!(
            chain[0].label.contains("SM.Vendor.IMB.1001"),
            "{}",
            chain[0].label
        );
        assert!(chain[2].label.contains("ROOT"), "{}", chain[2].label);
    }

    #[test]
    fn a_suite_without_exactly_one_security_manager_is_refused() {
        let f = fixtures();
        let leaf = read(&f.link_decryptor.certificate);
        let xml = extended_flm(
            "Rex",
            "Europe/London",
            &[(
                "7",
                vec![vec![FlmDeviceSpec {
                    device_type: "LD",
                    serial: "2001",
                    chain_pems: vec![leaf],
                }]],
            )],
        );
        let error = parse_flm(&xml).unwrap().kdm_suites().unwrap_err();
        assert!(
            error.contains("auditorium 7 suite 1 holds 0 SM devices"),
            "{error}"
        );
    }

    // the ST 430-7 shape of the MovieLabs FLM-x sample
    const ORIGINAL_FLM: &str = r#"<?xml version="1.0"?>
<flm:FacilityListMessage xmlns:flm="http://www.smpte-ra.org/schemas/430-7/20XX/FLM"
    xmlns:ds="http://www.w3.org/2000/09/xmldsig#">
  <flm:FacilityInfo>
    <flm:FacilityName>Contoso 20</flm:FacilityName>
    <flm:ContactList>
      <flm:Contact><flm:Email>jdoe@contoso.biz</flm:Email></flm:Contact>
      <flm:Contact><flm:Email>ops@contoso.biz</flm:Email></flm:Contact>
    </flm:ContactList>
  </flm:FacilityInfo>
  <flm:AuditoriumList>
    <flm:Auditorium>
      <flm:AuditoriumNumberOrName>1</flm:AuditoriumNumberOrName>
      <flm:SuiteList><flm:Suite><flm:Device>
        <flm:DeviceSerial>218281828</flm:DeviceSerial>
        <flm:KeyInfoList><ds:KeyInfo><ds:X509Data>
          <ds:X509Certificate>QUJDREVG</ds:X509Certificate>
        </ds:X509Data></ds:KeyInfo></flm:KeyInfoList>
      </flm:Device></flm:Suite></flm:SuiteList>
    </flm:Auditorium>
  </flm:AuditoriumList>
</flm:FacilityListMessage>"#;

    #[test]
    fn the_original_shape_still_parses() {
        let flm = parse_flm(ORIGINAL_FLM).unwrap();
        assert_eq!(flm.version, FlmVersion::Original);
        assert_eq!(flm.facility.name, "Contoso 20");
        assert_eq!(flm.emails(), vec!["jdoe@contoso.biz", "ops@contoso.biz"]);
        assert_eq!(flm.auditoriums[0].number_or_name, "1");
        let device = &flm.auditoriums[0].suites[0].devices[0];
        assert_eq!(device.serial.as_deref(), Some("218281828"));
        assert_eq!(device.certificate_chains[0][0].der, b"ABCDEF");
    }

    #[test]
    fn a_document_that_is_not_an_flm_is_refused() {
        assert!(parse_flm("<something/>").is_err());
        assert!(parse_flm("not xml <<<").is_err());
    }
}
