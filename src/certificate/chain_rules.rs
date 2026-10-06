use super::{distinguished_name, public_key_digest_base64};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use x509_parser::extensions::ParsedExtension;
use x509_parser::prelude::*;

// ISDCF Doc 5 Annex A allows a chain of one certificate in every context
const MINIMUM_CHAIN_LENGTH: usize = 1;
const SHA256_WITH_RSA_ENCRYPTION_OID: &str = "1.2.840.113549.1.1.11";
const DN_QUALIFIER_OID: &str = "2.5.4.46";
const REQUIRED_RSA_MODULUS_BITS: usize = 2048;
const REQUIRED_RSA_PUBLIC_EXPONENT: u64 = 65537;
// ST 430-2 5.3.4: roles end at the leftmost period of the CommonName
const ROLE_SEPARATOR: char = '.';
const REVOCATION_NOT_CHECKED: &str =
    "no revocation list is held, so revoked keys and certificates are not checked";

pub const KDM_RECIPIENT_ROLES: &[&str] = &["SM"];
pub const AUTHORIZED_DEVICE_ROLES: &[&str] = &["LD", "PR"];
// ISDCF Doc 5 Annex A marks these Best Effort for a remote SPB chain
pub const AUTHORIZED_DEVICE_BEST_EFFORT_RULES: &[ChainRule] =
    &[ChainRule::AuthorityKeyIdentifier, ChainRule::Issuer];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChainRule {
    DerEncoding,
    Version,
    CriticalExtensions,
    RequiredFields,
    PathLengthConstraint,
    KeyUsage,
    OrganizationName,
    Role,
    DesiredTime,
    SignatureAlgorithm,
    RsaKey,
    Revocation,
    DnQualifier,
    AuthorityKeyIdentifier,
    SignatureValue,
    MinimumChainLength,
    Issuer,
    ValidityDates,
}

impl ChainRule {
    pub fn number_and_name(self) -> (u8, &'static str) {
        match self {
            Self::DerEncoding => (1, "DER encoding"),
            Self::Version => (2, "X.509 version 3"),
            Self::CriticalExtensions => (3, "unrecognized critical extension"),
            Self::RequiredFields => (4, "required fields"),
            Self::PathLengthConstraint => (5, "path length constraint"),
            Self::KeyUsage => (6, "key usage"),
            Self::OrganizationName => (7, "organization name"),
            Self::Role => (8, "role"),
            Self::DesiredTime => (9, "desired time"),
            Self::SignatureAlgorithm => (10, "signature algorithm"),
            Self::RsaKey => (11, "RSA key"),
            Self::Revocation => (12, "revoked key or certificate"),
            Self::DnQualifier => (13, "dnQualifier"),
            Self::AuthorityKeyIdentifier => (14, "authority key identifier"),
            Self::SignatureValue => (15, "signature value"),
            Self::MinimumChainLength => (16, "minimum chain length"),
            Self::Issuer => (17, "issuer"),
            Self::ValidityDates => (18, "validity dates"),
        }
    }
}

impl std::fmt::Display for ChainRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (number, name) = self.number_and_name();
        write!(f, "ST 430-2 rule {number} ({name})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainRuleFinding {
    pub rule: ChainRule,
    pub certificate: String,
    pub detail: String,
}

impl std::fmt::Display for ChainRuleFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}: {}", self.certificate, self.rule, self.detail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainRuleSkip {
    pub rule: ChainRule,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainReport {
    pub failures: Vec<ChainRuleFinding>,
    pub best_effort_failures: Vec<ChainRuleFinding>,
    pub not_checked: Vec<ChainRuleSkip>,
}

impl ChainReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafRoles {
    ZeroOrMore,
    AnyOf(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy)]
pub struct ChainContext<'a> {
    pub leaf_roles: LeafRoles,
    pub desired_times: &'a [DateTime<Utc>],
    pub best_effort_rules: &'static [ChainRule],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainCertificate {
    pub label: String,
    pub der: Vec<u8>,
}

pub fn chain_from_files(paths: &[PathBuf]) -> Result<Vec<ChainCertificate>, String> {
    paths
        .iter()
        .map(|path| {
            let data = std::fs::read(path)
                .map_err(|e| format!("failed to read certificate {}: {e}", path.display()))?;
            let (_, pem) = parse_x509_pem(&data)
                .map_err(|e| format!("failed to parse PEM {}: {e}", path.display()))?;
            Ok(ChainCertificate {
                label: path.display().to_string(),
                der: pem.contents,
            })
        })
        .collect()
}

pub fn chain_from_pem(pem: &str) -> Result<Vec<ChainCertificate>, String> {
    let mut chain = Vec::new();
    for block in Pem::iter_from_buffer(pem.as_bytes()) {
        let block = block.map_err(|e| format!("the certificate PEM cannot be read: {e}"))?;
        if block.label != CERTIFICATE_PEM_LABEL {
            return Err(format!(
                "the certificate PEM holds a {} block, only certificates belong in a chain",
                block.label
            ));
        }
        let label = certificate_label(&block.contents, chain.len());
        chain.push(ChainCertificate {
            label,
            der: block.contents,
        });
    }
    if chain.is_empty() {
        return Err("the certificate PEM holds no certificate".to_string());
    }
    Ok(chain)
}

const CERTIFICATE_PEM_LABEL: &str = "CERTIFICATE";

pub fn certificate_label(der: &[u8], position: usize) -> String {
    match X509Certificate::from_der(der) {
        Ok((_, certificate)) => distinguished_name(certificate.subject()),
        Err(_) => format!("certificate {}", position + 1),
    }
}

pub fn chain_to_pem(chain: &[ChainCertificate]) -> String {
    chain
        .iter()
        .map(|certificate| der_to_pem(&certificate.der))
        .collect()
}

pub fn der_to_pem(der: &[u8]) -> String {
    use base64::Engine;
    super::der_base64_to_pem(&base64::engine::general_purpose::STANDARD.encode(der))
}

// FLM KeyInfo lists a chain in any order, a KDM wants it leaf first
pub fn leaf_first(chain: Vec<ChainCertificate>) -> Result<Vec<ChainCertificate>, String> {
    let names: Vec<(String, String)> = chain
        .iter()
        .map(|certificate| {
            let (_, parsed) = X509Certificate::from_der(&certificate.der)
                .map_err(|e| format!("{}: not a DER certificate: {e}", certificate.label))?;
            Ok((
                distinguished_name(parsed.subject()),
                distinguished_name(parsed.issuer()),
            ))
        })
        .collect::<Result<_, String>>()?;
    let issues_another = |index: usize| {
        names
            .iter()
            .enumerate()
            .any(|(other, (_, issuer))| other != index && *issuer == names[index].0)
    };
    let leaves: Vec<usize> = (0..chain.len()).filter(|i| !issues_another(*i)).collect();
    let [leaf] = leaves.as_slice() else {
        return Err(format!(
            "the certificates do not form one chain: {} of them sign no other certificate",
            leaves.len()
        ));
    };
    let mut order = vec![*leaf];
    while order.len() < chain.len() {
        let current = order[order.len() - 1];
        let (subject, issuer) = &names[current];
        if subject == issuer {
            break;
        }
        let Some(parent) = (0..chain.len()).find(|i| !order.contains(i) && names[*i].0 == *issuer)
        else {
            break;
        };
        order.push(parent);
    }
    if order.len() != chain.len() {
        return Err(format!(
            "the certificates do not form one chain: {} of {} link up from the leaf",
            order.len(),
            chain.len()
        ));
    }
    let mut slots: Vec<Option<ChainCertificate>> = chain.into_iter().map(Some).collect();
    Ok(order
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect())
}

pub fn certificate_roles(common_name: &str) -> Vec<&str> {
    let roles = common_name
        .split_once(ROLE_SEPARATOR)
        .map_or("", |(roles, _)| roles);
    roles.split(' ').filter(|role| !role.is_empty()).collect()
}

struct Findings<'a> {
    report: ChainReport,
    context: &'a ChainContext<'a>,
}

impl Findings<'_> {
    fn record(&mut self, rule: ChainRule, certificate: &str, detail: String) {
        let finding = ChainRuleFinding {
            rule,
            certificate: certificate.to_string(),
            detail,
        };
        if self.context.best_effort_rules.contains(&rule) {
            self.report.best_effort_failures.push(finding);
        } else {
            self.report.failures.push(finding);
        }
    }
}

pub fn check_chain(chain: &[ChainCertificate], context: &ChainContext) -> ChainReport {
    let mut findings = Findings {
        report: ChainReport::default(),
        context,
    };
    findings.report.not_checked.push(ChainRuleSkip {
        rule: ChainRule::Revocation,
        reason: REVOCATION_NOT_CHECKED.to_string(),
    });
    if chain.len() < MINIMUM_CHAIN_LENGTH {
        findings.record(
            ChainRule::MinimumChainLength,
            "chain",
            format!("the chain holds {} certificates", chain.len()),
        );
        return findings.report;
    }

    // a certificate that does not parse is reported and the rest are still checked
    let parsed: Vec<Option<X509Certificate<'_>>> = chain
        .iter()
        .map(
            |certificate| match X509Certificate::from_der(&certificate.der) {
                Ok(([], cert)) => Some(cert),
                Ok((rest, _)) => {
                    findings.record(
                        ChainRule::DerEncoding,
                        &certificate.label,
                        format!("{} bytes follow the certificate", rest.len()),
                    );
                    None
                }
                Err(e) => {
                    findings.record(
                        ChainRule::DerEncoding,
                        &certificate.label,
                        format!("not a DER certificate: {e}"),
                    );
                    None
                }
            },
        )
        .collect();

    for (index, (certificate, cert)) in chain.iter().zip(&parsed).enumerate() {
        if let Some(cert) = cert {
            check_certificate(&mut findings, &certificate.label, cert, index == 0);
        }
    }
    for index in 0..parsed.len() {
        let parent_index = (index + 1).min(parsed.len() - 1);
        if let (Some(child), Some(parent)) = (&parsed[index], &parsed[parent_index]) {
            check_pair(
                &mut findings,
                &chain[index].label,
                child,
                parent,
                index == parent_index,
            );
        }
    }
    findings.report
}

fn first_attribute<'a>(
    attributes: impl Iterator<Item = &'a AttributeTypeAndValue<'a>>,
) -> (usize, Option<String>) {
    let values: Vec<String> = attributes
        .map(|attribute| attribute.as_str().unwrap_or_default().to_string())
        .collect();
    (values.len(), values.into_iter().next())
}

fn check_certificate(
    findings: &mut Findings,
    label: &str,
    cert: &X509Certificate<'_>,
    is_leaf: bool,
) {
    if cert.version() != X509Version::V3 {
        findings.record(
            ChainRule::Version,
            label,
            format!("version {} instead of X.509v3", cert.version().0 + 1),
        );
    }

    for extension in cert.extensions() {
        match extension.parsed_extension() {
            ParsedExtension::UnsupportedExtension { oid } if extension.critical => findings.record(
                ChainRule::CriticalExtensions,
                label,
                format!("extension {} is marked critical", oid.to_id_string()),
            ),
            ParsedExtension::ParseError { error } => findings.record(
                ChainRule::DerEncoding,
                label,
                format!(
                    "extension {} cannot be parsed: {error}",
                    extension.oid.to_id_string()
                ),
            ),
            _ => {}
        }
    }

    let has_authority_key_identifier = cert.iter_extensions().any(|extension| {
        matches!(
            extension.parsed_extension(),
            ParsedExtension::AuthorityKeyIdentifier(_)
        )
    });
    let basic_constraints = cert.basic_constraints().ok().flatten();
    let mut missing = Vec::new();
    if !has_authority_key_identifier {
        missing.push("AuthorityKeyIdentifier");
    }
    if basic_constraints.is_none() {
        missing.push("BasicConstraint");
    }
    if cert.issuer().iter_attributes().next().is_none() {
        missing.push("Issuer");
    }
    if cert.subject().iter_attributes().next().is_none() {
        missing.push("Subject");
    }
    if !missing.is_empty() {
        findings.record(
            ChainRule::RequiredFields,
            label,
            format!("missing {}", missing.join(", ")),
        );
    }

    let is_certificate_authority = basic_constraints
        .as_ref()
        .is_some_and(|constraints| constraints.value.ca);
    if let Some(constraints) = basic_constraints {
        let path_length = constraints.value.path_len_constraint;
        if is_certificate_authority && path_length.is_none() {
            findings.record(
                ChainRule::PathLengthConstraint,
                label,
                "a certificate authority without a PathLenConstraint".to_string(),
            );
        }
        if !is_certificate_authority && path_length.is_some_and(|length| length != 0) {
            findings.record(
                ChainRule::PathLengthConstraint,
                label,
                format!(
                    "a leaf with PathLenConstraint {}",
                    path_length.unwrap_or_default()
                ),
            );
        }
    }

    check_key_usage(findings, label, cert, is_certificate_authority);

    let (subject_organization_count, subject_organization) =
        first_attribute(cert.subject().iter_organization());
    let (_, issuer_organization) = first_attribute(cert.issuer().iter_organization());
    if subject_organization_count != 1 || subject_organization != issuer_organization {
        findings.record(
            ChainRule::OrganizationName,
            label,
            format!(
                "subject OrganizationName {subject_organization:?} does not match issuer \
                 OrganizationName {issuer_organization:?}"
            ),
        );
    }

    if !is_certificate_authority {
        check_role(findings, label, cert, is_leaf);
    }

    for desired_time in findings.context.desired_times {
        let instant = desired_time.timestamp();
        let validity = cert.validity();
        if instant < validity.not_before.timestamp() || instant > validity.not_after.timestamp() {
            findings.record(
                ChainRule::DesiredTime,
                label,
                format!(
                    "not valid at {desired_time}: valid from {} to {}",
                    validity.not_before, validity.not_after
                ),
            );
        }
    }

    let outer_algorithm = cert.signature_algorithm.algorithm.to_id_string();
    let inner_algorithm = cert.tbs_certificate.signature.algorithm.to_id_string();
    if outer_algorithm != inner_algorithm || outer_algorithm != SHA256_WITH_RSA_ENCRYPTION_OID {
        findings.record(
            ChainRule::SignatureAlgorithm,
            label,
            format!(
                "signature algorithms {outer_algorithm} and {inner_algorithm}, \
                 expected sha256WithRSAEncryption ({SHA256_WITH_RSA_ENCRYPTION_OID})"
            ),
        );
    }

    match cert.public_key().parsed() {
        Ok(x509_parser::public_key::PublicKey::RSA(rsa)) => {
            let exponent = rsa.try_exponent().unwrap_or_default();
            if rsa.key_size() != REQUIRED_RSA_MODULUS_BITS
                || exponent != REQUIRED_RSA_PUBLIC_EXPONENT
            {
                findings.record(
                    ChainRule::RsaKey,
                    label,
                    format!(
                        "a {}-bit RSA key with exponent {exponent}, expected \
                         {REQUIRED_RSA_MODULUS_BITS} bits and {REQUIRED_RSA_PUBLIC_EXPONENT}",
                        rsa.key_size()
                    ),
                );
            }
        }
        _ => findings.record(ChainRule::RsaKey, label, "not an RSA key".to_string()),
    }

    let (dn_qualifier_count, dn_qualifier) = first_attribute(
        cert.subject()
            .iter_attributes()
            .filter(|attribute| attribute.attr_type().to_id_string() == DN_QUALIFIER_OID),
    );
    let expected = public_key_digest_base64(cert.public_key().raw).unwrap_or_default();
    if dn_qualifier_count != 1 || dn_qualifier.as_deref() != Some(expected.as_str()) {
        findings.record(
            ChainRule::DnQualifier,
            label,
            format!(
                "subject dnQualifier {dn_qualifier:?}, the public key thumbprint is {expected}"
            ),
        );
    }
}

fn check_key_usage(
    findings: &mut Findings,
    label: &str,
    cert: &X509Certificate<'_>,
    is_certificate_authority: bool,
) {
    let key_usage = match cert.key_usage() {
        Ok(Some(key_usage)) => key_usage.value,
        Ok(None) => {
            findings.record(ChainRule::KeyUsage, label, "no KeyUsage field".to_string());
            return;
        }
        Err(e) => {
            findings.record(
                ChainRule::KeyUsage,
                label,
                format!("KeyUsage unreadable: {e}"),
            );
            return;
        }
    };
    let signs_certificates = key_usage.key_cert_sign() || key_usage.crl_sign();
    let detail = if is_certificate_authority {
        let other_flags_set = key_usage.digital_signature()
            || key_usage.non_repudiation()
            || key_usage.key_encipherment()
            || key_usage.data_encipherment()
            || key_usage.key_agreement()
            || key_usage.encipher_only()
            || key_usage.decipher_only();
        (!key_usage.key_cert_sign() || other_flags_set).then(|| {
            format!(
                "a certificate authority may set only keyCertSign and cRLSign, flags are {:#x}",
                key_usage.flags
            )
        })
    } else {
        (signs_certificates || !(key_usage.digital_signature() || key_usage.key_encipherment()))
            .then(|| {
                format!(
                    "a leaf needs digitalSignature or keyEncipherment and no certificate \
                     signing, flags are {:#x}",
                    key_usage.flags
                )
            })
    };
    if let Some(detail) = detail {
        findings.record(ChainRule::KeyUsage, label, detail);
    }
}

fn check_role(findings: &mut Findings, label: &str, cert: &X509Certificate<'_>, is_leaf: bool) {
    let LeafRoles::AnyOf(wanted) = findings.context.leaf_roles else {
        return;
    };
    let (_, common_name) = first_attribute(cert.subject().iter_common_name());
    let common_name = common_name.unwrap_or_default();
    let roles = certificate_roles(&common_name);
    if roles.is_empty() {
        findings.record(
            ChainRule::Role,
            label,
            format!("the leaf CommonName '{common_name}' carries no role"),
        );
        return;
    }
    if is_leaf && !roles.iter().any(|role| wanted.contains(role)) {
        findings.record(
            ChainRule::Role,
            label,
            format!(
                "the leaf roles {roles:?} include none of {wanted:?} (CommonName '{common_name}')"
            ),
        );
    }
}

fn subject_key_identifier<'a>(cert: &'a X509Certificate<'a>) -> Option<&'a [u8]> {
    cert.iter_extensions()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(identifier) => Some(identifier.0),
            _ => None,
        })
}

fn check_pair(
    findings: &mut Findings,
    label: &str,
    child: &X509Certificate<'_>,
    parent: &X509Certificate<'_>,
    is_root: bool,
) {
    let authority_key_identifier =
        child
            .iter_extensions()
            .find_map(|extension| match extension.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(identifier) => Some(identifier),
                _ => None,
            });
    if let Some(identifier) = authority_key_identifier {
        let parent_key_digest = super::public_key_digest(parent.public_key().raw).ok();
        let parent_key_identifier = subject_key_identifier(parent)
            .map(<[u8]>::to_vec)
            .or_else(|| parent_key_digest.map(|digest| digest.to_vec()));
        let found = match (&identifier.key_identifier, identifier.authority_cert_serial) {
            (Some(key_identifier), _) => parent_key_identifier.as_deref() == Some(key_identifier.0),
            (None, Some(serial)) => serial == parent.raw_serial(),
            (None, None) => false,
        };
        if !found {
            findings.record(
                ChainRule::AuthorityKeyIdentifier,
                label,
                "the AuthorityKeyIdentifier does not identify the issuing certificate".to_string(),
            );
        }
    }

    if let Err(e) = child.verify_signature(Some(parent.public_key())) {
        findings.record(
            ChainRule::SignatureValue,
            label,
            format!("signature verification failed: {e}"),
        );
    }

    if child.issuer() != parent.subject() {
        let detail = if is_root {
            format!(
                "the last certificate is not self-issued, so the chain does not reach a root \
                 (subject '{}', issuer '{}')",
                distinguished_name(child.subject()),
                distinguished_name(child.issuer())
            )
        } else {
            format!(
                "chain broken: issuer '{}' does not match the next certificate's subject '{}'",
                distinguished_name(child.issuer()),
                distinguished_name(parent.subject())
            )
        };
        findings.record(ChainRule::Issuer, label, detail);
    }

    let child_validity = child.validity();
    let parent_validity = parent.validity();
    if !is_root
        && (child_validity.not_before < parent_validity.not_before
            || child_validity.not_after > parent_validity.not_after)
    {
        findings.record(
            ChainRule::ValidityDates,
            label,
            format!(
                "valid {} to {}, outside its issuer's {} to {}",
                child_validity.not_before,
                child_validity.not_after,
                parent_validity.not_before,
                parent_validity.not_after
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, Certificate, CertificateParams, CustomExtension, DnType, IsCa,
        KeyIdMethod, KeyPair, KeyUsagePurpose, SerialNumber,
    };
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    const ORGANIZATION: &str = "Vendor";
    const DN_QUALIFIER_ARCS: [u64; 4] = [2, 5, 4, 46];
    const UNKNOWN_EXTENSION_ARCS: [u64; 9] = [1, 3, 6, 1, 4, 1, 55555, 1, 1];
    const ASN1_NULL: [u8; 2] = [0x05, 0x00];
    // TBSCertificate version [0] EXPLICIT INTEGER 2, the X.509v3 marker
    const VERSION_3_ENCODING: [u8; 5] = [0xa0, 0x03, 0x02, 0x01, 0x02];
    const VERSION_2_VALUE: u8 = 0x01;

    struct Keys {
        root: KeyPair,
        intermediate: KeyPair,
        leaf: KeyPair,
        stranger: KeyPair,
    }

    fn keys() -> &'static Keys {
        static KEYS: OnceLock<Keys> = OnceLock::new();
        KEYS.get_or_init(|| {
            let key = || super::super::generate_rsa_keypair(2048).expect("RSA key");
            Keys {
                root: key(),
                intermediate: key(),
                leaf: key(),
                stranger: key(),
            }
        })
    }

    fn digest(key: &KeyPair) -> Vec<u8> {
        super::super::public_key_digest(&key.public_key_der())
            .expect("digest")
            .to_vec()
    }

    fn dn_qualifier(key: &KeyPair) -> String {
        public_key_digest_base64(&key.public_key_der()).expect("dnQualifier")
    }

    fn serial() -> SerialNumber {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        SerialNumber::from(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn params(
        common_name: &str,
        key: &KeyPair,
        path_length: Option<u8>,
        days: i64,
    ) -> CertificateParams {
        let mut params = CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::OrganizationName, ORGANIZATION);
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.distinguished_name.push(
            DnType::CustomDnType(DN_QUALIFIER_ARCS.to_vec()),
            dn_qualifier(key),
        );
        params.key_identifier_method = KeyIdMethod::PreSpecified(digest(key));
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(serial());
        params.not_before = ::time::OffsetDateTime::now_utc() - ::time::Duration::days(1);
        params.not_after = ::time::OffsetDateTime::now_utc() + ::time::Duration::days(days);
        match path_length {
            Some(length) => {
                params.is_ca = IsCa::Ca(BasicConstraints::Constrained(length));
                params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            }
            None => {
                params.is_ca = IsCa::ExplicitNoCa;
                params.key_usages = vec![
                    KeyUsagePurpose::DigitalSignature,
                    KeyUsagePurpose::KeyEncipherment,
                ];
            }
        }
        params
    }

    struct Chain {
        root: CertificateParams,
        intermediate: CertificateParams,
        leaf: CertificateParams,
        leaf_key: Option<KeyPair>,
        leaf_signer: Option<KeyPair>,
        // the leaf is signed by an intermediate carrying these params instead
        intermediate_as_issuer: Option<CertificateParams>,
    }

    impl Chain {
        fn new(leaf_common_name: &str) -> Self {
            let keys = keys();
            Self {
                root: params(".Vendor.ROOT", &keys.root, Some(3), 3650),
                intermediate: params(".Vendor.INTERMEDIATE", &keys.intermediate, Some(2), 3600),
                leaf: params(leaf_common_name, &keys.leaf, None, 3000),
                leaf_key: None,
                leaf_signer: None,
                intermediate_as_issuer: None,
            }
        }

        fn certificates(self) -> Vec<Certificate> {
            let keys = keys();
            let root = self.root.self_signed(&keys.root).expect("root");
            let intermediate = self
                .intermediate
                .signed_by(&keys.intermediate, &root, &keys.root)
                .expect("intermediate");
            let stand_in = self.intermediate_as_issuer.map(|params| {
                params
                    .self_signed(&keys.intermediate)
                    .expect("issuer stand-in")
            });
            let issuer = stand_in.as_ref().unwrap_or(&intermediate);
            let leaf_key = self.leaf_key.as_ref().unwrap_or(&keys.leaf);
            let signer = self.leaf_signer.as_ref().unwrap_or(&keys.intermediate);
            let leaf = self.leaf.signed_by(leaf_key, issuer, signer).expect("leaf");
            vec![leaf, intermediate, root]
        }

        fn build(self) -> Vec<ChainCertificate> {
            self.certificates()
                .into_iter()
                .enumerate()
                .map(|(index, certificate)| ChainCertificate {
                    label: format!("certificate {index}"),
                    der: certificate.der().to_vec(),
                })
                .collect()
        }
    }

    fn recipient_report(chain: &[ChainCertificate]) -> ChainReport {
        check_chain(
            chain,
            &ChainContext {
                leaf_roles: LeafRoles::AnyOf(KDM_RECIPIENT_ROLES),
                desired_times: &[Utc::now()],
                best_effort_rules: &[],
            },
        )
    }

    fn failed_rules(report: &ChainReport) -> Vec<ChainRule> {
        report.failures.iter().map(|finding| finding.rule).collect()
    }

    fn assert_fails(chain: &[ChainCertificate], rule: ChainRule) {
        let report = recipient_report(chain);
        assert!(
            failed_rules(&report).contains(&rule),
            "expected {rule} among the failures: {:#?}",
            report.failures
        );
    }

    #[test]
    fn a_conforming_security_manager_chain_passes_and_says_revocation_was_not_checked() {
        let report = recipient_report(&Chain::new("SM.Vendor.IMB.1").build());
        assert!(report.passed(), "{:#?}", report.failures);
        assert_eq!(report.not_checked[0].rule, ChainRule::Revocation);
    }

    #[test]
    fn rule_1_a_truncated_certificate() {
        let mut chain = Chain::new("SM.Vendor.IMB.1").build();
        let half = chain[0].der.len() / 2;
        chain[0].der.truncate(half);
        assert_fails(&chain, ChainRule::DerEncoding);
    }

    #[test]
    fn rule_2_a_version_2_certificate() {
        let mut chain = Chain::new("SM.Vendor.IMB.1").build();
        let der = &mut chain[0].der;
        let at = der
            .windows(VERSION_3_ENCODING.len())
            .position(|window| window == VERSION_3_ENCODING)
            .expect("version field");
        der[at + VERSION_3_ENCODING.len() - 1] = VERSION_2_VALUE;
        assert_fails(&chain, ChainRule::Version);
    }

    #[test]
    fn rule_3_an_unknown_critical_extension() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        let mut extension =
            CustomExtension::from_oid_content(&UNKNOWN_EXTENSION_ARCS, ASN1_NULL.to_vec());
        extension.set_criticality(true);
        chain.leaf.custom_extensions.push(extension);
        assert_fails(&chain.build(), ChainRule::CriticalExtensions);
    }

    #[test]
    fn rule_4_no_authority_key_identifier() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf.use_authority_key_identifier_extension = false;
        assert_fails(&chain.build(), ChainRule::RequiredFields);
    }

    #[test]
    fn rule_5_a_certificate_authority_without_a_path_length() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.intermediate.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        assert_fails(&chain.build(), ChainRule::PathLengthConstraint);
    }

    #[test]
    fn rule_6_a_leaf_that_signs_certificates() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf.key_usages.push(KeyUsagePurpose::KeyCertSign);
        assert_fails(&chain.build(), ChainRule::KeyUsage);
    }

    #[test]
    fn rule_7_an_organization_other_than_the_issuers() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain
            .leaf
            .distinguished_name
            .remove(DnType::OrganizationName);
        chain
            .leaf
            .distinguished_name
            .push(DnType::OrganizationName, "Elsewhere");
        assert_fails(&chain.build(), ChainRule::OrganizationName);
    }

    #[test]
    fn rule_8_a_leaf_without_the_wanted_role_or_any_role() {
        assert_fails(&Chain::new("LD.Vendor.LDB.1").build(), ChainRule::Role);
        assert_fails(&Chain::new(".Vendor.IMB.1").build(), ChainRule::Role);
    }

    #[test]
    fn rule_9_a_leaf_that_expired() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf.not_after = ::time::OffsetDateTime::now_utc() - ::time::Duration::hours(1);
        assert_fails(&chain.build(), ChainRule::DesiredTime);
    }

    #[test]
    fn rule_10_a_sha384_signature() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf_signer = Some(
            KeyPair::from_pem_and_sign_algo(
                &keys().intermediate.serialize_pem(),
                &rcgen::PKCS_RSA_SHA384,
            )
            .expect("SHA-384 signer"),
        );
        assert_fails(&chain.build(), ChainRule::SignatureAlgorithm);
    }

    #[test]
    fn rule_11_an_elliptic_curve_key() {
        let ec_key = KeyPair::generate().expect("EC key");
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf = params("SM.Vendor.IMB.1", &ec_key, None, 3000);
        chain.leaf_key = Some(ec_key);
        assert_fails(&chain.build(), ChainRule::RsaKey);
    }

    #[test]
    fn rule_13_a_dn_qualifier_of_another_key() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain
            .leaf
            .distinguished_name
            .remove(DnType::CustomDnType(DN_QUALIFIER_ARCS.to_vec()));
        chain.leaf.distinguished_name.push(
            DnType::CustomDnType(DN_QUALIFIER_ARCS.to_vec()),
            dn_qualifier(&keys().stranger),
        );
        assert_fails(&chain.build(), ChainRule::DnQualifier);
    }

    #[test]
    fn rule_14_an_authority_key_identifier_naming_another_key() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        let mut stand_in = chain.intermediate.clone();
        stand_in.key_identifier_method = KeyIdMethod::PreSpecified(digest(&keys().stranger));
        chain.intermediate_as_issuer = Some(stand_in);
        assert_fails(&chain.build(), ChainRule::AuthorityKeyIdentifier);
    }

    #[test]
    fn rule_15_a_signature_by_another_key() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf_signer =
            Some(KeyPair::from_pem(&keys().stranger.serialize_pem()).expect("stranger key"));
        let report = recipient_report(&chain.build());
        assert_eq!(failed_rules(&report), vec![ChainRule::SignatureValue]);
    }

    #[test]
    fn rule_16_an_empty_chain() {
        assert_fails(&[], ChainRule::MinimumChainLength);
    }

    #[test]
    fn rule_17_a_chain_missing_its_intermediate() {
        let mut chain = Chain::new("SM.Vendor.IMB.1").build();
        chain.remove(1);
        assert_fails(&chain, ChainRule::Issuer);
    }

    #[test]
    fn rule_18_a_leaf_that_outlives_its_issuer() {
        let mut chain = Chain::new("SM.Vendor.IMB.1");
        chain.leaf.not_after = ::time::OffsetDateTime::now_utc() + ::time::Duration::days(4000);
        assert_fails(&chain.build(), ChainRule::ValidityDates);
    }

    #[test]
    fn a_device_chain_reports_rules_14_and_17_as_best_effort() {
        let mut chain = Chain::new("LD.Vendor.LDB.1").build();
        chain.remove(1);
        let report = check_chain(
            &chain,
            &ChainContext {
                leaf_roles: LeafRoles::AnyOf(AUTHORIZED_DEVICE_ROLES),
                desired_times: &[Utc::now()],
                best_effort_rules: AUTHORIZED_DEVICE_BEST_EFFORT_RULES,
            },
        );
        let best_effort: Vec<ChainRule> = report
            .best_effort_failures
            .iter()
            .map(|finding| finding.rule)
            .collect();
        assert!(best_effort.contains(&ChainRule::Issuer), "{report:#?}");
        assert!(
            best_effort.contains(&ChainRule::AuthorityKeyIdentifier),
            "{report:#?}"
        );
        assert!(!failed_rules(&report).contains(&ChainRule::Issuer));
        assert!(failed_rules(&report).contains(&ChainRule::SignatureValue));
    }

    #[test]
    fn a_chain_listed_root_first_is_put_leaf_first() {
        let chain = Chain::new("SM.Vendor.IMB.1").build();
        let shuffled = vec![chain[2].clone(), chain[0].clone(), chain[1].clone()];
        assert_eq!(leaf_first(shuffled).expect("one chain"), chain);
    }

    #[test]
    fn roles_are_the_words_before_the_first_period() {
        assert_eq!(
            certificate_roles("SM MD LE.Vendor.IMB.1"),
            vec!["SM", "MD", "LE"]
        );
        assert!(certificate_roles(".Vendor.ROOT").is_empty());
    }
}
