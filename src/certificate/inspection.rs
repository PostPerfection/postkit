use super::{
    ChainCertificate, ChainContext, ChainReport, KdmMetadata, LeafRoles, LoneLeaf, cert_thumbprint,
    certificate_label, certificate_roles, chain_from_pem, check_chain, distinguished_name,
    leaf_first, parse_kdm, thumbprint_base64, verify_kdm_signature,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display, Formatter};
use std::path::Path;
use x509_parser::extensions::KeyUsage;
use x509_parser::oid_registry::OID_X509_DN_QUALIFIER;
use x509_parser::prelude::*;

type KeyUsageFlag = fn(&KeyUsage) -> bool;

const KEY_USAGE_NAMES: &[(&str, KeyUsageFlag)] = &[
    ("digitalSignature", KeyUsage::digital_signature),
    ("nonRepudiation", KeyUsage::non_repudiation),
    ("keyEncipherment", KeyUsage::key_encipherment),
    ("dataEncipherment", KeyUsage::data_encipherment),
    ("keyAgreement", KeyUsage::key_agreement),
    ("keyCertSign", KeyUsage::key_cert_sign),
    ("cRLSign", KeyUsage::crl_sign),
    ("encipherOnly", KeyUsage::encipher_only),
    ("decipherOnly", KeyUsage::decipher_only),
];
const PEM_BEGIN_MARKER: &str = "-----BEGIN";
const KDM_ROOT_ELEMENT: &str = "DCinemaSecurityMessage";
const INDENT: &str = "  ";
const NONE_TEXT: &str = "(none)";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateDetails {
    pub subject: String,
    pub issuer: String,
    pub serial: String,
    pub not_before: String,
    pub not_after: String,
    pub expired: bool,
    pub rsa_key_bits: Option<usize>,
    pub is_certificate_authority: bool,
    pub path_length: Option<u32>,
    pub key_usage: Vec<String>,
    pub signature_algorithm: String,
    pub thumbprint: String,
    pub dn_qualifier: Option<String>,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainInspection {
    pub certificates: Vec<CertificateDetails>,
    pub ordering_error: Option<String>,
    pub check: ChainReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdmKeyIdText {
    pub key_type: Option<String>,
    pub key_id: uuid::Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "detail", rename_all = "snake_case")]
pub enum SignatureVerification {
    Passed,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KdmInspection {
    pub metadata: KdmMetadata,
    pub key_ids: Vec<KdmKeyIdText>,
    pub signature: SignatureVerification,
    pub signer_chain: ChainInspection,
    pub undecodable_certificates: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileInspection {
    Kdm(Box<KdmInspection>),
    Certificates(ChainInspection),
}

fn rfc3339(time: ASN1Time) -> String {
    DateTime::from_timestamp(time.timestamp(), 0)
        .map(|instant| instant.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| time.to_string())
}

pub fn certificate_details(der: &[u8], at: DateTime<Utc>) -> Result<CertificateDetails, String> {
    let (_, certificate) =
        X509Certificate::from_der(der).map_err(|e| format!("not a DER certificate: {e}"))?;
    let validity = certificate.validity();
    let basic_constraints = certificate.basic_constraints().ok().flatten();
    let key_usage = match certificate.key_usage() {
        Ok(Some(extension)) => KEY_USAGE_NAMES
            .iter()
            .filter(|(_, is_set)| is_set(extension.value))
            .map(|(name, _)| name.to_string())
            .collect(),
        _ => Vec::new(),
    };
    let rsa_key_bits = match certificate.public_key().parsed() {
        Ok(x509_parser::public_key::PublicKey::RSA(rsa)) => Some(rsa.key_size()),
        _ => None,
    };
    let signature_oid = &certificate.signature_algorithm.algorithm;
    let signature_algorithm =
        x509_parser::objects::oid2sn(signature_oid, x509_parser::objects::oid_registry())
            .map_or_else(|_| signature_oid.to_id_string(), str::to_string);
    let common_name = certificate
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attribute| attribute.as_str().ok())
        .unwrap_or_default();
    let dn_qualifier = certificate
        .subject()
        .iter_by_oid(&OID_X509_DN_QUALIFIER)
        .next()
        .and_then(|attribute| attribute.as_str().ok())
        .map(str::to_string);
    Ok(CertificateDetails {
        subject: distinguished_name(certificate.subject()),
        issuer: distinguished_name(certificate.issuer()),
        serial: certificate.serial.to_string(),
        not_before: rfc3339(validity.not_before),
        not_after: rfc3339(validity.not_after),
        expired: validity.not_after.timestamp() < at.timestamp(),
        rsa_key_bits,
        is_certificate_authority: basic_constraints
            .as_ref()
            .is_some_and(|constraints| constraints.value.ca),
        path_length: basic_constraints
            .and_then(|constraints| constraints.value.path_len_constraint),
        key_usage,
        signature_algorithm,
        thumbprint: thumbprint_base64(&cert_thumbprint(certificate.tbs_certificate.as_ref())),
        dn_qualifier,
        roles: certificate_roles(common_name)
            .into_iter()
            .map(str::to_string)
            .collect(),
    })
}

fn inspect_chain(chain: Vec<ChainCertificate>, at: DateTime<Utc>) -> ChainInspection {
    let (chain, ordering_error) = if chain.is_empty() {
        (chain, None)
    } else {
        match leaf_first(chain.clone()) {
            Ok(ordered) => (ordered, None),
            Err(e) => (chain, Some(e)),
        }
    };
    let check = check_chain(
        &chain,
        &ChainContext {
            leaf_roles: LeafRoles::ZeroOrMore,
            desired_times: &[at],
            best_effort_rules: &[],
            lone_leaf: LoneLeaf::Refused,
        },
    );
    // one that does not parse is already a DER encoding failure in the check
    let certificates = chain
        .iter()
        .filter_map(|certificate| certificate_details(&certificate.der, at).ok())
        .collect();
    ChainInspection {
        certificates,
        ordering_error,
        check,
    }
}

pub fn inspect_certificate_pem(pem: &str, at: DateTime<Utc>) -> Result<ChainInspection, String> {
    Ok(inspect_chain(chain_from_pem(pem)?, at))
}

pub fn inspect_kdm(kdm_xml: &str, at: DateTime<Utc>) -> Result<KdmInspection, String> {
    let metadata = parse_kdm(kdm_xml)?;
    let key_ids = metadata
        .key_ids
        .iter()
        .map(|key| KdmKeyIdText {
            key_type: key
                .key_type
                .map(|key_type| String::from_utf8_lossy(&key_type).into_owned()),
            key_id: key.key_id,
        })
        .collect();
    let signature = match verify_kdm_signature(kdm_xml) {
        Ok(()) => SignatureVerification::Passed,
        Err(e) => SignatureVerification::Failed(e),
    };
    let (certificates, undecodable_certificates) = crate::xmldsig::extract_certificates(kdm_xml);
    let chain = certificates
        .into_iter()
        .enumerate()
        .map(|(position, der)| ChainCertificate {
            label: certificate_label(&der, position),
            der,
        })
        .collect();
    Ok(KdmInspection {
        metadata,
        key_ids,
        signature,
        signer_chain: inspect_chain(chain, at),
        undecodable_certificates,
    })
}

pub fn inspect_file(path: &Path, at: DateTime<Utc>) -> Result<FileInspection, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.contains(PEM_BEGIN_MARKER) {
        return Ok(FileInspection::Certificates(inspect_certificate_pem(
            &text, at,
        )?));
    }
    if text.contains(KDM_ROOT_ELEMENT) {
        return Ok(FileInspection::Kdm(Box::new(inspect_kdm(&text, at)?)));
    }
    Err(format!(
        "{} is neither a KDM nor a PEM certificate file",
        path.display()
    ))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn or_none(value: Option<&str>) -> &str {
    value.unwrap_or(NONE_TEXT)
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        NONE_TEXT.to_string()
    } else {
        values.join(", ")
    }
}

impl Display for CertificateDetails {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let key = match self.rsa_key_bits {
            Some(bits) => format!("{bits}-bit RSA"),
            None => "not RSA".to_string(),
        };
        let certificate_authority = match (self.is_certificate_authority, self.path_length) {
            (true, Some(length)) => format!("yes, path length {length}"),
            (is_certificate_authority, _) => yes_no(is_certificate_authority).to_string(),
        };
        writeln!(f, "{INDENT}Subject: {}", self.subject)?;
        writeln!(f, "{INDENT}Issuer: {}", self.issuer)?;
        writeln!(f, "{INDENT}Serial: {}", self.serial)?;
        writeln!(
            f,
            "{INDENT}Valid: {} to {}",
            self.not_before, self.not_after
        )?;
        writeln!(f, "{INDENT}Expired: {}", yes_no(self.expired))?;
        writeln!(f, "{INDENT}Key: {key}")?;
        writeln!(f, "{INDENT}Certificate authority: {certificate_authority}")?;
        writeln!(f, "{INDENT}Key usage: {}", list_or_none(&self.key_usage))?;
        writeln!(
            f,
            "{INDENT}Signature algorithm: {}",
            self.signature_algorithm
        )?;
        writeln!(f, "{INDENT}Thumbprint: {}", self.thumbprint)?;
        writeln!(
            f,
            "{INDENT}dnQualifier: {}",
            or_none(self.dn_qualifier.as_deref())
        )?;
        writeln!(f, "{INDENT}Roles: {}", list_or_none(&self.roles))
    }
}

impl Display for ChainInspection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let count = self.certificates.len();
        for (index, certificate) in self.certificates.iter().enumerate() {
            writeln!(f, "Certificate {} of {count}", index + 1)?;
            write!(f, "{certificate}")?;
        }
        match &self.ordering_error {
            Some(e) => writeln!(f, "Order: as found, {e}")?,
            None => writeln!(f, "Order: leaf first")?,
        }
        let verdict = if self.check.passed() {
            "passed"
        } else {
            "failed"
        };
        writeln!(f, "Chain check: {verdict}")?;
        for failure in &self.check.failures {
            writeln!(f, "{INDENT}{failure}")?;
        }
        for failure in &self.check.best_effort_failures {
            writeln!(f, "{INDENT}Best effort: {failure}")?;
        }
        for skipped in &self.check.not_checked {
            writeln!(
                f,
                "{INDENT}Not checked: {}: {}",
                skipped.rule, skipped.reason
            )?;
        }
        Ok(())
    }
}

impl Display for KdmInspection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let metadata = &self.metadata;
        writeln!(f, "KDM")?;
        writeln!(f, "{INDENT}Content title: {}", metadata.content_title)?;
        writeln!(f, "{INDENT}Annotation: {}", metadata.annotation_text)?;
        writeln!(f, "{INDENT}CPL: urn:uuid:{}", metadata.cpl_id)?;
        writeln!(
            f,
            "{INDENT}Valid: {} to {}",
            metadata.not_valid_before, metadata.not_valid_after
        )?;
        writeln!(
            f,
            "{INDENT}Recipient: {}",
            or_none(metadata.recipient_subject_name.as_deref())
        )?;
        writeln!(
            f,
            "{INDENT}Content authenticator: {}",
            or_none(metadata.content_authenticator.as_deref())
        )?;
        writeln!(f, "{INDENT}Keys: {}", self.key_ids.len())?;
        for key in &self.key_ids {
            writeln!(
                f,
                "{INDENT}{INDENT}{} urn:uuid:{}",
                or_none(key.key_type.as_deref()),
                key.key_id
            )?;
        }
        match &self.signature {
            SignatureVerification::Passed => writeln!(f, "{INDENT}Signature: passed")?,
            SignatureVerification::Failed(e) => writeln!(f, "{INDENT}Signature: failed, {e}")?,
        }
        if self.undecodable_certificates > 0 {
            writeln!(
                f,
                "{INDENT}Signer certificates that are not base64: {}",
                self.undecodable_certificates
            )?;
        }
        writeln!(f, "Signer chain")?;
        write!(f, "{}", self.signer_chain)
    }
}

impl Display for FileInspection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kdm(inspection) => write!(f, "{inspection}"),
            Self::Certificates(inspection) => write!(f, "{inspection}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::test_support::{
        CPL_ID, DISTRIBUTOR, VENDOR, dkdm, fixtures, read,
    };

    const TITLE: &str = "Inspected Feature";

    fn common_name_of(details: &CertificateDetails) -> &str {
        details
            .subject
            .split(',')
            .find_map(|attribute| attribute.strip_prefix("CN="))
            .unwrap()
    }

    #[test]
    fn a_certificate_shows_its_whole_distinguished_name() {
        let f = fixtures();
        let inspection =
            inspect_certificate_pem(&read(&f.security_managers[0].certificate), Utc::now())
                .unwrap();

        let leaf = &inspection.certificates[0];
        let dn_qualifier = leaf.dn_qualifier.as_deref().unwrap();
        assert_eq!(
            leaf.subject,
            format!(
                "dnQualifier={},C=US,O={VENDOR},CN=SM.{VENDOR}.IMB.1001",
                crate::certificate::escape_dn_value(dn_qualifier)
            )
        );
        let intermediate =
            inspect_certificate_pem(&read(&f.vendor_intermediate), Utc::now()).unwrap();
        assert_eq!(leaf.issuer, intermediate.certificates[0].subject);
        assert!(
            leaf.issuer
                .ends_with(&format!(",CN=.{VENDOR}.smpte-430-2.INTERMEDIATE")),
            "{}",
            leaf.issuer
        );
        assert_eq!(leaf.roles, ["SM"]);
        assert!(!leaf.is_certificate_authority);
        assert_eq!(leaf.rsa_key_bits, Some(2048));
        assert_eq!(leaf.signature_algorithm, "sha256WithRSAEncryption");
        assert!(!leaf.expired);
        assert!(
            inspection
                .to_string()
                .contains(&format!("Subject: {}\n", leaf.subject))
        );
    }

    #[test]
    fn every_certificate_of_a_chain_file_is_listed_leaf_first() {
        let f = fixtures();
        let shuffled = format!(
            "{}{}{}",
            read(&f.vendor_root),
            read(&f.security_managers[0].certificate),
            read(&f.vendor_intermediate)
        );

        let inspection = inspect_certificate_pem(&shuffled, Utc::now()).unwrap();

        let common_names: Vec<&str> = inspection.certificates.iter().map(common_name_of).collect();
        assert_eq!(
            common_names,
            [
                format!("SM.{VENDOR}.IMB.1001"),
                format!(".{VENDOR}.smpte-430-2.INTERMEDIATE"),
                format!(".{VENDOR}.smpte-430-2.ROOT"),
            ]
        );
        assert_eq!(inspection.ordering_error, None);
        assert!(inspection.check.passed(), "{:?}", inspection.check);
        let text = inspection.to_string();
        assert!(text.contains("Certificate 3 of 3\n"), "{text}");
        assert!(text.contains("Chain check: passed\n"), "{text}");
    }

    #[test]
    fn unrelated_certificates_stay_in_file_order_with_the_ordering_error() {
        let f = fixtures();
        let distributor_root = &f.distributor_chain[1];
        let pem = format!("{}{}", read(&f.vendor_root), read(distributor_root));

        let inspection = inspect_certificate_pem(&pem, Utc::now()).unwrap();

        let common_names: Vec<&str> = inspection.certificates.iter().map(common_name_of).collect();
        assert_eq!(
            common_names,
            [
                format!(".{VENDOR}.smpte-430-2.ROOT"),
                format!(".{DISTRIBUTOR}.smpte-430-2.ROOT"),
            ]
        );
        assert!(
            inspection
                .ordering_error
                .as_deref()
                .is_some_and(|e| e.contains("do not form one chain")),
            "{:?}",
            inspection.ordering_error
        );
        assert!(!inspection.check.passed());
    }

    #[test]
    fn a_signed_kdm_shows_its_signer_chain_and_key_types() {
        let inspection = inspect_kdm(&dkdm(TITLE, 7), Utc::now()).unwrap();

        assert_eq!(inspection.metadata.cpl_id.to_string(), CPL_ID);
        assert_eq!(inspection.metadata.content_title, TITLE);
        assert_eq!(inspection.signature, SignatureVerification::Passed);
        let key_types: Vec<Option<&str>> = inspection
            .key_ids
            .iter()
            .map(|key| key.key_type.as_deref())
            .collect();
        assert_eq!(key_types, [Some("MDIK"), Some("MDAK")]);
        let common_names: Vec<&str> = inspection
            .signer_chain
            .certificates
            .iter()
            .map(common_name_of)
            .collect();
        assert_eq!(
            common_names,
            [
                format!("CS.{DISTRIBUTOR}.smpte-430-2.LEAF"),
                format!(".{DISTRIBUTOR}.smpte-430-2.INTERMEDIATE"),
                format!(".{DISTRIBUTOR}.smpte-430-2.ROOT"),
            ]
        );
        assert!(
            inspection.signer_chain.check.passed(),
            "{:?}",
            inspection.signer_chain.check
        );
        let text = inspection.to_string();
        assert!(text.contains("Signature: passed\n"), "{text}");
        assert!(text.contains("MDIK urn:uuid:"), "{text}");
    }

    #[test]
    fn a_tampered_kdm_still_inspects_with_the_signature_failing() {
        let tampered = dkdm(TITLE, 7).replacen(
            &format!(">{TITLE}</ContentTitleText>"),
            ">Edited Feature</ContentTitleText>",
            1,
        );

        let inspection = inspect_kdm(&tampered, Utc::now()).unwrap();

        assert_eq!(inspection.metadata.content_title, "Edited Feature");
        let SignatureVerification::Failed(error) = &inspection.signature else {
            panic!("a tampered KDM must fail its signature");
        };
        assert!(error.contains("KDM signature is invalid"), "{error}");
        assert_eq!(inspection.signer_chain.certificates.len(), 3);
        assert!(
            inspection
                .to_string()
                .contains(&format!("Signature: failed, {error}\n"))
        );
    }

    #[test]
    fn a_file_is_inspected_by_its_content() {
        let f = fixtures();
        let directory = tempfile::tempdir().unwrap();
        let kdm_path = directory.path().join("kdm.xml");
        let pem_path = directory.path().join("chain.txt");
        let other_path = directory.path().join("notes.xml");
        std::fs::write(&kdm_path, dkdm(TITLE, 7)).unwrap();
        std::fs::write(&pem_path, f.vendor_chain_pem(&f.projector.certificate)).unwrap();
        std::fs::write(&other_path, "<Notes/>").unwrap();

        assert!(matches!(
            inspect_file(&kdm_path, Utc::now()).unwrap(),
            FileInspection::Kdm(_)
        ));
        let FileInspection::Certificates(chain) = inspect_file(&pem_path, Utc::now()).unwrap()
        else {
            panic!("a PEM file must inspect as certificates");
        };
        assert_eq!(chain.certificates.len(), 3);
        assert!(
            inspect_file(&other_path, Utc::now())
                .unwrap_err()
                .contains("neither a KDM nor a PEM certificate file")
        );
    }
}
