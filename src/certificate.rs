use crate::packaging::escape_xml as xml_escape;
use crate::xmldsig::{DSIG_NS, XmlSigner};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Certificate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CertType {
    /// Self-signed root CA
    Root,
    /// Intermediate CA
    Intermediate,
    /// End-entity (screen/projector)
    Leaf,
    /// Content signer
    Signer,
}

/// Certificate generation options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertOptions {
    pub cert_type: CertType,
    pub common_name: String,
    pub organization: String,
    pub organizational_unit: String,
    pub country: String,
    /// RSA key size
    pub key_bits: u32,
    /// Validity in days, defaulting to `CERTIFICATE_VALIDITY_DAYS`
    pub validity_days: u32,
    pub output_cert: PathBuf,
    pub output_key: PathBuf,
    /// For non-root certs: issuer cert/key
    pub issuer_cert: PathBuf,
    pub issuer_key: PathBuf,
}

impl Default for CertOptions {
    fn default() -> Self {
        Self {
            cert_type: CertType::Signer,
            common_name: String::new(),
            organization: String::new(),
            organizational_unit: String::new(),
            country: "US".to_string(),
            key_bits: 2048,
            validity_days: CERTIFICATE_VALIDITY_DAYS,
            output_cert: PathBuf::new(),
            output_key: PathBuf::new(),
            issuer_cert: PathBuf::new(),
            issuer_key: PathBuf::new(),
        }
    }
}

/// Certificate info (parsed from PEM/DER).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CertInfo {
    pub subject_cn: String,
    pub issuer_cn: String,
    pub serial: String,
    pub not_before: String,
    pub not_after: String,
    pub key_bits: u32,
    pub is_ca: bool,
    pub is_expired: bool,
    /// Base64 SMPTE ST 430-2 thumbprint, the value a KDM lists as
    /// CertificateThumbprint. Empty when the certificate could not be parsed.
    pub thumbprint: String,
}

/// A trusted device entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub name: String,
    /// Base64 SMPTE ST 430-2 thumbprint, the same spelling a KDM carries.
    pub thumbprint: String,
    pub certificate_path: PathBuf,
}

/// Application directory under the XDG data dir.
const DATA_DIR_NAME: &str = "postkit";
/// Trusted device store, under `DATA_DIR_NAME`.
const TRUSTED_DEVICES_DIR_NAME: &str = "trusted_devices";
/// The store keeps a copy of each device certificate beside its record.
const CERTIFICATE_EXTENSION: &str = "pem";
/// The record itself, one `TrustedDevice` per file.
const DEVICE_RECORD_EXTENSION: &str = "json";

/// Get the trusted devices directory (XDG data or fallback).
fn trusted_devices_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(DATA_DIR_NAME).join(TRUSTED_DEVICES_DIR_NAME)
}

/// Generate an RSA key pair for rcgen to sign with.
///
/// rcgen signs via ring, which cannot *generate* RSA keys, so the key comes
/// from the `rsa` crate and is handed over as PKCS#8.
fn generate_rsa_keypair(bits: u32) -> Result<rcgen::KeyPair, String> {
    use rsa::pkcs8::EncodePrivateKey;

    // DCI DCSS 9.7.6 requires 2048-bit RSA throughout the digital cinema chain.
    if bits < 2048 {
        return Err(format!("RSA key size {bits} is below the 2048-bit minimum"));
    }

    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, bits as usize)
        .map_err(|e| format!("RSA key generation failed: {e}"))?;
    let pem = key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .map_err(|e| format!("cannot encode RSA key as PKCS#8: {e}"))?;
    rcgen::KeyPair::from_pem(&pem).map_err(|e| format!("rcgen rejected the RSA key: {e}"))
}

/// ST 430-2 5.3.1 requires a dnQualifier on every certificate in the chain.
/// The attribute type is X.520 dnQualifier, OID 2.5.4.46.
const DN_QUALIFIER_OID: [u64; 4] = [2, 5, 4, 46];

/// ST 430-2 5.3.1 CommonName role token, the text before the first '.'. A CA
/// carries no role, which is why its CommonName starts with the separator; a
/// content signer carries "CS".
const CN_ROLE_CERTIFICATE_AUTHORITY: &str = "";
const CN_ROLE_CONTENT_SIGNER: &str = "CS";
/// The standard a ST 430-2 CommonName names between the entity and the tier.
const CN_STANDARD_TOKEN: &str = "smpte-430-2";
const CN_TIER_ROOT: &str = "ROOT";
const CN_TIER_INTERMEDIATE: &str = "INTERMEDIATE";
const CN_TIER_LEAF: &str = "LEAF";

/// basicConstraints path lengths, as libdcp writes them in
/// `certificate_chain.cc`: three CAs may sit below the root, two below the
/// intermediate, and the leaf is not a CA at all.
const ROOT_PATH_LEN_CONSTRAINT: u8 = 3;
const INTERMEDIATE_PATH_LEN_CONSTRAINT: u8 = 2;

/// DCP-o-matic's `Config::check_certificates` refuses a signer chain holding a
/// certificate whose not_after year is more than this many years past its
/// not_before year.
const MAX_CERTIFICATE_VALIDITY_YEARS: i32 = 15;
/// The lifetime every tier of a generated chain gets, matching the
/// `CERTIFICATE_VALIDITY_PERIOD` DCP-o-matic mints its own chain with.
const CERTIFICATE_VALIDITY_YEARS: u32 = 10;
/// Validity is counted in whole days, and a nominal year is enough: the leap
/// days it ignores only shorten the span the limit above caps.
const DAYS_PER_YEAR: u32 = 365;
const CERTIFICATE_VALIDITY_DAYS: u32 = CERTIFICATE_VALIDITY_YEARS * DAYS_PER_YEAR;

/// libdcp's `CertificateChain::CertificateChain` staggers a generated chain by a
/// day per tier. Each tier is minted after the one above it, so equal spans
/// would leave a child outliving the issuer that vouches for it.
const ROOT_VALIDITY_DAYS: u32 = CERTIFICATE_VALIDITY_DAYS;
const INTERMEDIATE_VALIDITY_DAYS: u32 = ROOT_VALIDITY_DAYS - 1;
const LEAF_VALIDITY_DAYS: u32 = INTERMEDIATE_VALIDITY_DAYS - 1;

/// A ST 430-2 5.3.1 CommonName: role token, entity, standard, tier.
fn common_name(role: &str, organization: &str, tier: &str) -> String {
    format!("{role}.{organization}.{CN_STANDARD_TOKEN}.{tier}")
}

/// ST 430-2 5.3 requires every DN value to be an ASN.1 PrintableString, and
/// DCP-o-matic rejects a signer chain that uses UTF8String instead
/// (`Config::check_certificates`). Anything outside that charset has to fail
/// rather than silently fall back to a type deployed gear will not take.
fn printable_dn_value(label: &str, value: &str) -> Result<rcgen::DnValue, String> {
    rcgen::PrintableString::try_from(value.to_string())
        .map(rcgen::DnValue::PrintableString)
        .map_err(|e| {
            format!(
                "{label} '{value}' cannot be encoded as the ASN.1 PrintableString \
                 ST 430-2 5.3 requires: {e}"
            )
        })
}

/// RFC 4514 2 joins RDNs with this, unpadded, most specific first.
const DN_RDN_SEPARATOR: &str = ",";
/// RFC 4514 2 joins the attributes inside one multi-valued RDN with this.
const DN_ATTRIBUTE_SEPARATOR: &str = "+";
/// RFC 4514 3 short names for the attribute types it names, plus the X.520
/// dnQualifier ST 430-2 5.3.1 puts on every digital cinema certificate. The
/// five a DCI certificate can hold are spelled the same by OpenSSL, so libdcp's
/// output agrees attribute for attribute.
const DN_ATTRIBUTE_SHORT_NAMES: &[(&[u64], &str)] = &[
    (&[2, 5, 4, 3], "CN"),
    (&[2, 5, 4, 6], "C"),
    (&[2, 5, 4, 7], "L"),
    (&[2, 5, 4, 8], "ST"),
    (&[2, 5, 4, 9], "STREET"),
    (&[2, 5, 4, 10], "O"),
    (&[2, 5, 4, 11], "OU"),
    (&DN_QUALIFIER_OID, "dnQualifier"),
    (&[0, 9, 2342, 19200300, 100, 1, 1], "UID"),
    (&[0, 9, 2342, 19200300, 100, 1, 25], "DC"),
];
/// RFC 4514 3 puts a backslash before an escaped character or its hex pair.
const DN_ESCAPE: char = '\\';
/// RFC 4514 3 escapes these wherever they appear in a value.
const DN_ALWAYS_ESCAPED: &[char] = &['"', '+', ',', ';', '<', '>', '\\'];
/// RFC 4514 3 escapes these only as the first character of a value.
const DN_LEADING_ESCAPED: &[char] = &['#', ' '];
/// RFC 4514 3 escapes a space as the last character of a value too.
const DN_TRAILING_ESCAPED: char = ' ';
/// RFC 4514 2.4 marks a hex-encoded AttributeValue with this.
const DN_HEX_VALUE_PREFIX: &str = "#";

/// Render a distinguished name the way ST 430-1 wants X509SubjectName and
/// X509IssuerName spelled: RFC 4514, most specific RDN first.
///
/// Every DN postkit writes or prints goes through here. x509-parser's own
/// `Display` walks the DER order and pads the separator, so a projector
/// matching a KDM recipient against its own certificate would not recognise
/// the name; libdcp prints these with OpenSSL's `XN_FLAG_RFC2253`, and postkit
/// has to agree with it byte for byte.
pub(crate) fn distinguished_name(name: &x509_parser::x509::X509Name<'_>) -> String {
    let mut rdns: Vec<String> = name
        .iter_rdn()
        .map(|rdn| {
            rdn.iter()
                .map(render_dn_attribute)
                .collect::<Vec<_>>()
                .join(DN_ATTRIBUTE_SEPARATOR)
        })
        .collect();
    rdns.reverse();
    rdns.join(DN_RDN_SEPARATOR)
}

/// One `type=value` pair of a distinguished name.
fn render_dn_attribute(attribute: &x509_parser::x509::AttributeTypeAndValue<'_>) -> String {
    let oid = attribute.attr_type();
    match dn_attribute_short_name(oid).zip(attribute.as_str().ok()) {
        Some((short_name, value)) => format!("{short_name}={}", escape_dn_value(value)),
        // RFC 4514 2.4: a type with no short name, or a value that is not a
        // string, is written as the dotted OID and the hex of the value's DER.
        None => {
            use x509_parser::der_parser::asn1_rs::ToDer;
            // A value that was just parsed always re-encodes, and a DN out of a
            // recipient certificate must not be able to panic the KDM writer.
            let der = attribute.attr_value().to_der_vec().unwrap_or_default();
            let hex: String = der.iter().map(|byte| format!("{byte:02X}")).collect();
            format!("{}={DN_HEX_VALUE_PREFIX}{hex}", oid.to_id_string())
        }
    }
}

/// The RFC 4514 short name for an attribute type, if it has one.
fn dn_attribute_short_name(oid: &x509_parser::der_parser::Oid<'_>) -> Option<&'static str> {
    let arcs: Vec<u64> = oid.iter()?.collect();
    DN_ATTRIBUTE_SHORT_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == arcs.as_slice())
        .map(|(_, short_name)| *short_name)
}

/// Escape one attribute value per RFC 4514 3.
fn escape_dn_value(value: &str) -> String {
    let last = value.chars().count().saturating_sub(1);
    let mut escaped = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        let escaped_here = (index == 0 && DN_LEADING_ESCAPED.contains(&character))
            || (index == last && character == DN_TRAILING_ESCAPED);
        if DN_ALWAYS_ESCAPED.contains(&character) || escaped_here {
            escaped.push(DN_ESCAPE);
            escaped.push(character);
        } else if character.is_control() {
            let mut utf8 = [0u8; 4];
            for byte in character.encode_utf8(&mut utf8).as_bytes() {
                escaped.push_str(&format!("{DN_ESCAPE}{byte:02X}"));
            }
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// The ST 430-2 5.3.1 public key digest: SHA-1 over the public key BIT STRING
/// payload of a DER SubjectPublicKeyInfo. This is the dnQualifier value, and
/// libdcp uses the same bytes as the subjectKeyIdentifier.
///
/// libdcp's `public_key_digest` hashes `i2d_RSA_PUBKEY` output from byte 24
/// with an admitted "reasons that are not entirely clear"; 24 is the header
/// length for a 2048-bit RSA key only, so the payload is parsed out here.
fn public_key_digest(spki_der: &[u8]) -> Result<[u8; CERT_THUMBPRINT_LEN], String> {
    use sha1::Digest;
    use x509_parser::prelude::*;

    let (_, spki) = SubjectPublicKeyInfo::from_der(spki_der)
        .map_err(|e| format!("cannot parse the SubjectPublicKeyInfo: {e}"))?;
    Ok(sha1::Sha1::digest(&spki.subject_public_key.data).into())
}

/// The dnQualifier spelling of that digest: base64, unescaped. libdcp's
/// `escape_digest` backslash-escapes '/' and '+' only to get the value past the
/// openssl config parser; what lands in the certificate is the plain base64.
fn public_key_digest_base64(spki_der: &[u8]) -> Result<String, String> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(public_key_digest(spki_der)?))
}

/// A random certificate serial, as the minimal big-endian DER bytes.
///
/// ST 430-2 5.2 requires an unsigned integer of 64 bits or less and DCI CTP
/// 2.1.4 fails anything larger, but rcgen defaults to 20 bytes of a public key
/// hash, so the serial has to be set rather than left to it. 63 bits keeps the
/// value positive without the leading zero byte a full 64-bit value would need.
fn certificate_serial() -> Result<rcgen::SerialNumber, String> {
    let mut bytes: [u8; 8] = rand_bytes()?;
    bytes[0] &= 0x7f;
    let first_significant = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    Ok(rcgen::SerialNumber::from_slice(&bytes[first_significant..]))
}

/// Generate a new X.509 certificate + private key.
///
/// `opts.common_name` is written verbatim. ST 430-2 5.3.1 wants a role token
/// before the first '.' ("CS" for a content signer, empty for a CA), so a
/// caller building a DCI chain by hand has to spell that itself; `generate_chain`
/// does it for the chain it builds. A validity longer than
/// `MAX_CERTIFICATE_VALIDITY_YEARS` is refused here rather than minted into a
/// certificate DCP-o-matic would then reject.
pub fn generate_certificate(opts: &CertOptions) -> i32 {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, KeyIdMethod, KeyPair, KeyUsagePurpose,
    };

    let not_before = time::OffsetDateTime::now_utc();
    let not_after = not_before + time::Duration::days(opts.validity_days as i64);
    let span_years = not_after.year() - not_before.year();
    if span_years > MAX_CERTIFICATE_VALIDITY_YEARS {
        tracing::error!(
            "a validity of {} days spans {span_years} years, and DCP-o-matic refuses \
             any signer certificate spanning more than {MAX_CERTIFICATE_VALIDITY_YEARS}",
            opts.validity_days
        );
        return -1;
    }

    let key_pair = match generate_rsa_keypair(opts.key_bits) {
        Ok(kp) => kp,
        Err(e) => {
            tracing::error!("failed to generate RSA key pair: {e}");
            return -1;
        }
    };
    let public_key_der = key_pair.public_key_der();
    let key_digest = match public_key_digest(&public_key_der) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("{e}");
            return -1;
        }
    };
    let dn_qualifier = match public_key_digest_base64(&public_key_der) {
        Ok(q) => q,
        Err(e) => {
            tracing::error!("{e}");
            return -1;
        }
    };

    let mut dn_values = vec![("CommonName", DnType::CommonName, &opts.common_name)];
    if !opts.organization.is_empty() {
        dn_values.push((
            "OrganizationName",
            DnType::OrganizationName,
            &opts.organization,
        ));
    }
    if !opts.organizational_unit.is_empty() {
        dn_values.push((
            "OrganizationalUnitName",
            DnType::OrganizationalUnitName,
            &opts.organizational_unit,
        ));
    }
    if !opts.country.is_empty() {
        dn_values.push(("CountryName", DnType::CountryName, &opts.country));
    }
    dn_values.push((
        "dnQualifier",
        DnType::CustomDnType(DN_QUALIFIER_OID.to_vec()),
        &dn_qualifier,
    ));

    let mut params = CertificateParams::default();
    for (label, dn_type, value) in dn_values {
        match printable_dn_value(label, value) {
            Ok(dn_value) => params.distinguished_name.push(dn_type, dn_value),
            Err(e) => {
                tracing::error!("{e}");
                return -1;
            }
        }
    }

    // The subjectKeyIdentifier is the same digest as the dnQualifier, which is
    // what openssl's "hash" method gives libdcp, and it becomes the child's
    // authorityKeyIdentifier when this certificate signs one.
    params.key_identifier_method = KeyIdMethod::PreSpecified(key_digest.to_vec());
    params.use_authority_key_identifier_extension = true;

    params.serial_number = match certificate_serial() {
        Ok(serial) => Some(serial),
        Err(e) => {
            tracing::error!("{e}");
            return -1;
        }
    };

    params.not_before = not_before;
    params.not_after = not_after;

    match opts.cert_type {
        CertType::Root => {
            params.is_ca = IsCa::Ca(BasicConstraints::Constrained(ROOT_PATH_LEN_CONSTRAINT));
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        }
        CertType::Intermediate => {
            params.is_ca = IsCa::Ca(BasicConstraints::Constrained(
                INTERMEDIATE_PATH_LEN_CONSTRAINT,
            ));
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        }
        CertType::Leaf | CertType::Signer => {
            params.is_ca = IsCa::ExplicitNoCa;
            // keyEncipherment is what lets a KDM's content key be RSA-wrapped to
            // this certificate; libdcp pairs it with digitalSignature.
            params.key_usages = vec![
                KeyUsagePurpose::DigitalSignature,
                KeyUsagePurpose::KeyEncipherment,
            ];
        }
    }

    let cert = if opts.cert_type == CertType::Root {
        // Self-signed
        match params.self_signed(&key_pair) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to self-sign certificate: {e}");
                return -1;
            }
        }
    } else {
        // Signed by issuer
        let issuer_cert_pem = match std::fs::read_to_string(&opts.issuer_cert) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to read issuer cert: {e}");
                return -1;
            }
        };
        let issuer_key_pem = match std::fs::read_to_string(&opts.issuer_key) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to read issuer key: {e}");
                return -1;
            }
        };

        let issuer_key = match KeyPair::from_pem(&issuer_key_pem) {
            Ok(kp) => kp,
            Err(e) => {
                tracing::error!("failed to parse issuer key: {e}");
                return -1;
            }
        };

        let issuer_params = match CertificateParams::from_ca_cert_pem(&issuer_cert_pem) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("failed to parse issuer cert: {e}");
                return -1;
            }
        };

        let issuer = match issuer_params.self_signed(&issuer_key) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to reconstruct issuer: {e}");
                return -1;
            }
        };

        match params.signed_by(&key_pair, &issuer, &issuer_key) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to sign certificate: {e}");
                return -1;
            }
        }
    };

    if let Err(e) = std::fs::write(&opts.output_cert, cert.pem()) {
        tracing::error!("failed to write cert: {e}");
        return -1;
    }

    if let Err(e) = std::fs::write(&opts.output_key, key_pair.serialize_pem()) {
        tracing::error!("failed to write key: {e}");
        return -1;
    }

    tracing::info!("generated certificate: {}", opts.output_cert.display());
    0
}

/// Generate a self-signed certificate chain (root → intermediate → signer).
pub fn generate_chain(organization: &str, output_dir: &Path) -> i32 {
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        tracing::error!("failed to create output dir: {e}");
        return -1;
    }

    // Root CA
    let root_opts = CertOptions {
        cert_type: CertType::Root,
        common_name: common_name(CN_ROLE_CERTIFICATE_AUTHORITY, organization, CN_TIER_ROOT),
        organization: organization.to_string(),
        organizational_unit: "Digital Cinema".to_string(),
        validity_days: ROOT_VALIDITY_DAYS,
        output_cert: output_dir.join("root.pem"),
        output_key: output_dir.join("root.key"),
        ..Default::default()
    };
    if generate_certificate(&root_opts) != 0 {
        return -1;
    }

    // Intermediate CA
    let inter_opts = CertOptions {
        cert_type: CertType::Intermediate,
        common_name: common_name(
            CN_ROLE_CERTIFICATE_AUTHORITY,
            organization,
            CN_TIER_INTERMEDIATE,
        ),
        organization: organization.to_string(),
        organizational_unit: "Digital Cinema".to_string(),
        validity_days: INTERMEDIATE_VALIDITY_DAYS,
        output_cert: output_dir.join("intermediate.pem"),
        output_key: output_dir.join("intermediate.key"),
        issuer_cert: output_dir.join("root.pem"),
        issuer_key: output_dir.join("root.key"),
        ..Default::default()
    };
    if generate_certificate(&inter_opts) != 0 {
        return -1;
    }

    // Signer (leaf)
    let signer_opts = CertOptions {
        cert_type: CertType::Signer,
        common_name: common_name(CN_ROLE_CONTENT_SIGNER, organization, CN_TIER_LEAF),
        organization: organization.to_string(),
        organizational_unit: "Digital Cinema".to_string(),
        validity_days: LEAF_VALIDITY_DAYS,
        output_cert: output_dir.join("signer.pem"),
        output_key: output_dir.join("signer.key"),
        issuer_cert: output_dir.join("intermediate.pem"),
        issuer_key: output_dir.join("intermediate.key"),
        ..Default::default()
    };
    if generate_certificate(&signer_opts) != 0 {
        return -1;
    }

    tracing::info!("generated certificate chain in {}", output_dir.display());
    0
}

/// Read certificate info from PEM file.
pub fn read_certificate(cert_path: &Path) -> CertInfo {
    use x509_parser::prelude::*;

    let pem_data = match std::fs::read(cert_path) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to read cert {}: {e}", cert_path.display());
            return CertInfo::default();
        }
    };

    let (_, pem) = match parse_x509_pem(&pem_data) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("failed to parse PEM: {e}");
            return CertInfo::default();
        }
    };

    let cert = match pem.parse_x509() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to parse X.509: {e}");
            return CertInfo::default();
        }
    };

    let subject_cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or("")
        .to_string();

    let issuer_cn = cert
        .issuer()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or("")
        .to_string();

    let serial = cert.serial.to_str_radix(16);

    let not_before = cert.validity().not_before.to_rfc2822().unwrap_or_default();
    let not_after = cert.validity().not_after.to_rfc2822().unwrap_or_default();

    let key_bits = cert
        .public_key()
        .parsed()
        .ok()
        .map(|pk| match pk {
            // key_size() is already in bits
            x509_parser::public_key::PublicKey::RSA(rsa) => rsa.key_size() as u32,
            _ => 0,
        })
        .unwrap_or(0);

    let is_ca = cert.is_ca();

    let now = x509_parser::time::ASN1Time::now();
    let is_expired = cert.validity().not_after < now;

    let thumbprint = thumbprint_base64(&cert_thumbprint(cert.tbs_certificate.as_ref()));

    CertInfo {
        subject_cn,
        issuer_cn,
        serial,
        not_before,
        not_after,
        key_bits,
        is_ca,
        is_expired,
        thumbprint,
    }
}

/// A KDM's content-key validity window, as the two ST 430-1 timestamps carry it.
#[derive(Debug, Clone, Copy)]
struct KdmWindow {
    not_before: chrono::DateTime<chrono::Utc>,
    not_after: chrono::DateTime<chrono::Utc>,
}

impl KdmWindow {
    /// Both bounds have to be the exact ST 430-1 spelling, so the key block's
    /// own check runs here and a bad value fails before any crypto is done.
    fn parse(not_before: &str, not_after: &str) -> Result<Self, String> {
        check_kdm_timestamp("ContentKeysNotValidBefore", not_before)?;
        check_kdm_timestamp("ContentKeysNotValidAfter", not_after)?;
        let parse_one = |label: &str, value: &str| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|t| t.with_timezone(&chrono::Utc))
                .map_err(|e| format!("{label} is not a valid RFC 3339 timestamp ('{value}'): {e}"))
        };
        Ok(Self {
            not_before: parse_one("ContentKeysNotValidBefore", not_before)?,
            not_after: parse_one("ContentKeysNotValidAfter", not_after)?,
        })
    }
}

/// Where a KDM's validity window sits relative to a certificate's own validity.
///
/// The three cases are DCP-o-matic's `check_kdm_and_certificate_validity_periods`
/// (`kdm_util.cc`), which hard-errors on `OutsideCertificate` and warns on
/// `OverlapsCertificate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KdmWindowOverlap {
    /// The certificate covers the whole window.
    WithinCertificate,
    /// The certificate covers part of the window: the KDM opens for less time
    /// than it claims.
    OverlapsCertificate,
    /// The two share no time at all: the KDM can never open.
    OutsideCertificate,
}

/// An X.509 validity bound as a UTC instant.
fn certificate_validity_timestamp(
    time: x509_parser::prelude::ASN1Time,
) -> Result<chrono::DateTime<chrono::Utc>, String> {
    let seconds = time.timestamp();
    chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| format!("certificate validity timestamp {seconds} is out of range"))
}

/// The UTC date that bound falls on, for the day-granularity comparison libdcp
/// makes against a KDM window.
fn certificate_validity_date(
    time: x509_parser::prelude::ASN1Time,
) -> Result<chrono::NaiveDate, String> {
    Ok(certificate_validity_timestamp(time)?.date_naive())
}

fn classify_window(recipient: &Recipient, window: &KdmWindow) -> KdmWindowOverlap {
    if recipient.not_before <= window.not_before && recipient.not_after >= window.not_after {
        return KdmWindowOverlap::WithinCertificate;
    }
    if recipient.not_before.max(window.not_before) < recipient.not_after.min(window.not_after) {
        return KdmWindowOverlap::OverlapsCertificate;
    }
    KdmWindowOverlap::OutsideCertificate
}

/// Classify a KDM validity window against the recipient certificate it would be
/// issued to, without generating anything.
///
/// `build_kdm` refuses `OutsideCertificate` on its own; this is here so a caller
/// can show the answer up front and warn on `OverlapsCertificate`, which is
/// legal but means the KDM stops working before its stated end.
pub fn classify_kdm_window(
    recipient_cert_file: &Path,
    valid_from: &str,
    valid_to: &str,
) -> Result<KdmWindowOverlap, String> {
    let recipient = parse_recipient(recipient_cert_file)?;
    let not_valid_before = resolve_valid_from(valid_from);
    let not_valid_after = parse_validity_end(valid_to, &not_valid_before)?;
    let window = KdmWindow::parse(&not_valid_before, &not_valid_after)?;
    Ok(classify_window(&recipient, &window))
}

/// The two validity checks every KDM has to pass before it is worth building:
/// the recipient has to be able to open it at all, and the signer chain has to
/// cover the whole window.
///
/// An overlap is only warned about, matching `kdm_cli.cc`, because a KDM that
/// works for part of its stated window is still useful.
fn check_kdm_window(
    recipient: &Recipient,
    recipient_cert_file: &Path,
    signer_cert_file: &Path,
    signer_chain_files: &[PathBuf],
    window: &KdmWindow,
) -> Result<(), String> {
    match classify_window(recipient, window) {
        KdmWindowOverlap::WithinCertificate => {}
        KdmWindowOverlap::OverlapsCertificate => tracing::warn!(
            "the recipient certificate {} does not cover the whole KDM validity window, \
             so the KDM will stop working before {}",
            recipient_cert_file.display(),
            window.not_after
        ),
        KdmWindowOverlap::OutsideCertificate => {
            return Err(format!(
                "the KDM validity window {} to {} lies entirely outside the validity of the \
                 recipient certificate {}, so the KDM could never open",
                window.not_before,
                window.not_after,
                recipient_cert_file.display()
            ));
        }
    }

    // libdcp throws BadKDMDateError here rather than emitting a KDM the signer
    // chain cannot vouch for. Reusing the chain walk means the issuer linkage
    // and signatures are checked at the same time.
    let mut chain = vec![signer_cert_file.to_path_buf()];
    chain.extend(signer_chain_files.iter().cloned());
    validate_chain_inner(&chain, Some(window))
        .map_err(|e| format!("the signer chain cannot issue this KDM: {e}"))?;
    Ok(())
}

/// Validate a certificate chain, leaf first, root last.
///
/// Verifies the issuer signature on every certificate cryptographically. A
/// signature algorithm that x509-parser/ring cannot check is reported as a
/// failure, never as a pass.
pub fn validate_chain(chain: &[PathBuf]) -> i32 {
    match validate_chain_inner(chain, None) {
        Ok(n) => {
            tracing::info!("certificate chain valid ({n} certificates)");
            0
        }
        Err(e) => {
            tracing::error!("{e}");
            -1
        }
    }
}

/// The chain walk behind `validate_chain`. With `kdm_window` set it also
/// applies the check libdcp makes before it encrypts a KDM: every certificate
/// from leaf to root must cover the whole window.
fn validate_chain_inner(
    chain: &[PathBuf],
    kdm_window: Option<&KdmWindow>,
) -> Result<usize, String> {
    use x509_parser::prelude::*;

    if chain.is_empty() {
        return Err("empty certificate chain".into());
    }

    // Pem owns its contents, so parsed certs below can borrow from this vec.
    let mut pems = Vec::new();
    for path in chain {
        let data = std::fs::read(path)
            .map_err(|e| format!("failed to read certificate {}: {e}", path.display()))?;
        let (_, pem) = parse_x509_pem(&data)
            .map_err(|e| format!("failed to parse PEM {}: {e}", path.display()))?;
        pems.push(pem);
    }

    let mut certs = Vec::new();
    for (pem, path) in pems.iter().zip(chain) {
        let cert = pem
            .parse_x509()
            .map_err(|e| format!("failed to parse X.509 {}: {e}", path.display()))?;
        certs.push(cert);
    }

    let now = ASN1Time::now();
    for (cert, path) in certs.iter().zip(chain) {
        if cert.validity().not_after < now {
            return Err(format!("certificate expired: {}", path.display()));
        }
        if cert.validity().not_before > now {
            return Err(format!("certificate not yet valid: {}", path.display()));
        }
    }

    // libdcp (decrypted_kdm.cc, comparators in util.cc) compares at day
    // granularity and counts an equal day as a failure, so a certificate minted
    // today cannot sign a KDM whose window starts today.
    if let Some(window) = kdm_window {
        let window_start = window.not_before.date_naive();
        let window_end = window.not_after.date_naive();
        for (cert, path) in certs.iter().zip(chain) {
            let cert_start = certificate_validity_date(cert.validity().not_before)?;
            let cert_end = certificate_validity_date(cert.validity().not_after)?;
            if cert_start >= window_start {
                return Err(format!(
                    "certificate {} starts on {cert_start}, not before the day the KDM \
                     validity window starts ({window_start})",
                    path.display()
                ));
            }
            if cert_end <= window_end {
                return Err(format!(
                    "certificate {} expires on {cert_end}, not after the day the KDM \
                     validity window ends ({window_end})",
                    path.display()
                ));
            }
        }
    }

    // Each cert must be signed by the next one up; the last must be self-signed.
    for i in 0..certs.len() {
        let issuer = certs.get(i + 1).unwrap_or(&certs[i]);
        let is_root = i + 1 == certs.len();

        if certs[i].issuer() != issuer.subject() {
            return Err(if is_root {
                format!(
                    "root cert is not self-issued: {} (subject '{}', issuer '{}')",
                    chain[i].display(),
                    distinguished_name(certs[i].subject()),
                    distinguished_name(certs[i].issuer())
                )
            } else {
                format!(
                    "chain broken: issuer of {} ('{}') does not match subject of {} ('{}')",
                    chain[i].display(),
                    distinguished_name(certs[i].issuer()),
                    chain[i + 1].display(),
                    distinguished_name(issuer.subject())
                )
            });
        }

        certs[i]
            .verify_signature(Some(issuer.public_key()))
            .map_err(|e| {
                format!(
                    "signature verification failed for {}: {e}",
                    chain[i].display()
                )
            })?;
    }

    Ok(certs.len())
}

/// Add a trusted device.
pub fn add_trusted_device(cert_path: &Path, name: &str) -> i32 {
    add_trusted_device_in(&trusted_devices_dir(), cert_path, name)
}

fn add_trusted_device_in(dir: &Path, cert_path: &Path, name: &str) -> i32 {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!("failed to create trusted devices dir: {e}");
        return -1;
    }
    migrate_trusted_devices(dir);

    let info = read_certificate(cert_path);
    if info.thumbprint.is_empty() {
        tracing::error!("failed to read certificate for trusted device");
        return -1;
    }
    let stem = match read_cert_thumbprint(cert_path) {
        Ok(digest) => thumbprint_stem(&digest),
        Err(e) => {
            tracing::error!("{e}");
            return -1;
        }
    };

    let device = TrustedDevice {
        name: name.to_string(),
        thumbprint: info.thumbprint.clone(),
        certificate_path: cert_path.to_path_buf(),
    };

    // Copy cert to trusted devices dir
    let dest = dir.join(format!("{stem}.{CERTIFICATE_EXTENSION}"));
    if let Err(e) = std::fs::copy(cert_path, &dest) {
        tracing::error!("failed to copy certificate: {e}");
        return -1;
    }

    // Write metadata JSON
    let meta_path = dir.join(format!("{stem}.{DEVICE_RECORD_EXTENSION}"));
    let json = match serde_json::to_string_pretty(&device) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("failed to serialize device metadata: {e}");
            return -1;
        }
    };
    if let Err(e) = std::fs::write(&meta_path, json) {
        tracing::error!("failed to write device metadata: {e}");
        return -1;
    }

    tracing::info!("added trusted device '{}' ({})", name, info.thumbprint);
    0
}

/// List all trusted devices.
pub fn list_trusted_devices() -> Vec<TrustedDevice> {
    list_trusted_devices_in(&trusted_devices_dir())
}

fn list_trusted_devices_in(dir: &Path) -> Vec<TrustedDevice> {
    migrate_trusted_devices(dir);
    trusted_device_records(dir)
        .into_iter()
        .map(|(_, device)| device)
        .collect()
}

/// Remove a trusted device by its base64 ST 430-2 thumbprint.
pub fn remove_trusted_device(thumbprint: &str) -> i32 {
    remove_trusted_device_in(&trusted_devices_dir(), thumbprint)
}

fn remove_trusted_device_in(dir: &Path, thumbprint: &str) -> i32 {
    migrate_trusted_devices(dir);

    // Matched against the stored records rather than by rebuilding a file name,
    // so the displayed thumbprint stays the only thing a caller has to know.
    let mut removed = false;
    for (json_path, device) in trusted_device_records(dir) {
        if device.thumbprint != thumbprint {
            continue;
        }
        for path in [json_path.with_extension(CERTIFICATE_EXTENSION), json_path] {
            if !path.exists() {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::error!("failed to remove {}: {e}", path.display());
                return -1;
            }
            removed = true;
        }
    }

    if removed {
        tracing::info!("removed trusted device {thumbprint}");
        0
    } else {
        tracing::warn!("trusted device not found: {thumbprint}");
        -1
    }
}

/// Every readable record in the store, paired with the path it was read from.
fn trusted_device_records(dir: &Path) -> Vec<(PathBuf, TrustedDevice)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some(DEVICE_RECORD_EXTENSION)
            && let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(device) = serde_json::from_str::<TrustedDevice>(&data)
        {
            records.push((path, device));
        }
    }
    records
}

/// Bring the store up to the current thumbprint spelling before it is used.
///
/// Both the record's thumbprint and the file stem are recomputed from the stored
/// certificate, so a store already in that form is untouched and a run
/// interrupted halfway is finished on the next pass. Nothing here can fail the
/// operation the caller actually asked for: every problem is logged and skipped.
fn migrate_trusted_devices(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut orphans = 0;
    for entry in entries.flatten() {
        let json_path = entry.path();
        if json_path.extension().and_then(|e| e.to_str()) != Some(DEVICE_RECORD_EXTENSION) {
            continue;
        }
        let pem_path = json_path.with_extension(CERTIFICATE_EXTENSION);
        if !pem_path.exists() {
            orphans += 1;
            continue;
        }
        if let Err(e) = migrate_trusted_device(&json_path, &pem_path) {
            tracing::warn!(
                "leaving trusted device {} as it is: {e}",
                json_path.display()
            );
        }
    }

    if orphans > 0 {
        tracing::warn!(
            "{orphans} trusted device record(s) in {} have no certificate beside them, \
             so their thumbprint cannot be checked",
            dir.display()
        );
    }
}

fn migrate_trusted_device(json_path: &Path, pem_path: &Path) -> Result<(), String> {
    let data = std::fs::read_to_string(json_path)
        .map_err(|e| format!("cannot read {}: {e}", json_path.display()))?;
    let mut device: TrustedDevice = serde_json::from_str(&data)
        .map_err(|e| format!("cannot parse {}: {e}", json_path.display()))?;

    let digest = read_cert_thumbprint(pem_path)?;
    let thumbprint = thumbprint_base64(&digest);
    let stem = thumbprint_stem(&digest);

    let stem_is_current = json_path.file_stem().and_then(|s| s.to_str()) == Some(stem.as_str());
    if stem_is_current && device.thumbprint == thumbprint {
        return Ok(());
    }

    let dir = json_path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", json_path.display()))?;
    let new_json_path = dir.join(format!("{stem}.{DEVICE_RECORD_EXTENSION}"));
    if !stem_is_current {
        std::fs::rename(
            pem_path,
            dir.join(format!("{stem}.{CERTIFICATE_EXTENSION}")),
        )
        .map_err(|e| format!("cannot rename {}: {e}", pem_path.display()))?;
        std::fs::rename(json_path, &new_json_path)
            .map_err(|e| format!("cannot rename {}: {e}", json_path.display()))?;
    }

    device.thumbprint = thumbprint;
    let json = serde_json::to_string_pretty(&device)
        .map_err(|e| format!("cannot serialize {}: {e}", new_json_path.display()))?;
    std::fs::write(&new_json_path, json)
        .map_err(|e| format!("cannot write {}: {e}", new_json_path.display()))?;

    tracing::info!("migrated trusted device '{}' to {stem}", device.name);
    Ok(())
}

/// KDM output format: modern SMPTE (ST 430-1) or legacy Interop (pre-SMPTE).
///
/// Interop differs from SMPTE in three ways handled here: the key block drops
/// the 4-byte KeyType field (138 -> 134 bytes), the KDMRequiredExtensions uses
/// the digicine namespace, and KeyIdList carries bare KeyId elements without the
/// TypedKeyId wrapper. Interop output has not been checked against real legacy
/// gear; validate before production use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KdmFormat {
    #[default]
    Smpte,
    Interop,
}

impl KdmFormat {
    /// Every format, for a caller listing the choices on a command line.
    pub const ALL: [Self; 2] = [Self::Smpte, Self::Interop];

    /// The command line spelling, which `Display` and `FromStr` both go
    /// through. Serde is derived from the variant names and does not use this.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smpte => "smpte",
            Self::Interop => "interop",
        }
    }
}

impl std::fmt::Display for KdmFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Case-insensitive, so a command line spelling the format `SMPTE` parses. An
/// empty value is still an error rather than a silent default.
impl std::str::FromStr for KdmFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|format| format.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| unknown_spelling("KDM format", s, &Self::ALL.map(Self::as_str)))
    }
}

/// The error a `FromStr` over a fixed table of spellings returns, naming what
/// the caller could have written instead. Shared by the two KDM vocabulary
/// enums so both read the same on a command line.
fn unknown_spelling(label: &str, spelling: &str, known: &[&str]) -> String {
    format!("unknown {label} '{spelling}', expected one of {known:?}")
}

/// ISDCF Doc 5 KDM formulation: which devices the KDM names and whether it
/// carries a ContentAuthenticator.
///
/// The two choices are tabulated in libdcp's `EncryptedKDM::EncryptedKDM`
/// (encrypted_kdm.cc), which is what deployed gear was built against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KdmFormulation {
    #[default]
    ModifiedTransitional1,
    MultipleModifiedTransitional1,
    DciAny,
    DciSpecific,
}

impl KdmFormulation {
    /// Every formulation, for a caller listing the choices on a command line.
    pub const ALL: [Self; 4] = [
        Self::ModifiedTransitional1,
        Self::MultipleModifiedTransitional1,
        Self::DciAny,
        Self::DciSpecific,
    ];

    /// The ISDCF spelling. The only place these strings exist: `FromStr` and
    /// both serde impls go through here.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModifiedTransitional1 => "modified-transitional-1",
            Self::MultipleModifiedTransitional1 => "multiple-modified-transitional-1",
            Self::DciAny => "dci-any",
            Self::DciSpecific => "dci-specific",
        }
    }

    /// True when the DeviceList carries the caller's device certificates, false
    /// when it carries the assume-trust thumbprint alone.
    ///
    /// `KdmConfig::device_cert_files` has to agree with this, so a caller can
    /// reject the combination and name its own flags before doing any work.
    pub fn lists_supplied_devices(self) -> bool {
        matches!(
            self,
            Self::MultipleModifiedTransitional1 | Self::DciSpecific
        )
    }

    /// True when the KDM carries a ContentAuthenticator element.
    fn carries_content_authenticator(self) -> bool {
        matches!(self, Self::DciAny | Self::DciSpecific)
    }

    /// The formulation with the same ContentAuthenticator choice and the other
    /// device-list rule, named when a caller's device list contradicts theirs.
    pub fn device_list_counterpart(self) -> Self {
        match self {
            Self::ModifiedTransitional1 => Self::MultipleModifiedTransitional1,
            Self::MultipleModifiedTransitional1 => Self::ModifiedTransitional1,
            Self::DciAny => Self::DciSpecific,
            Self::DciSpecific => Self::DciAny,
        }
    }
}

impl std::fmt::Display for KdmFormulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Case-insensitive, like [`KdmFormat`]'s. `Deserialize` goes through here, so a
/// stored formulation reads back whatever case it was written in.
impl std::str::FromStr for KdmFormulation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|formulation| formulation.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| unknown_spelling("KDM formulation", s, &Self::ALL.map(Self::as_str)))
    }
}

impl Serialize for KdmFormulation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for KdmFormulation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spelling = String::deserialize(deserializer)?;
        spelling.parse().map_err(serde::de::Error::custom)
    }
}

/// Whether the picture essence keeps its forensic marking, per the ST 430-1
/// ForensicMarkFlagList. Press screenings are the usual reason to turn it off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PictureForensicMarking {
    /// Marking stays on, and no flag is written.
    #[default]
    Enabled,
    /// Marking off on the picture.
    Disabled,
}

/// Whether the audio essence keeps its forensic marking. Studios order HI/VI
/// tracks exempted by naming the channel above which marking stops.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioForensicMarking {
    /// Marking stays on for every channel, and no flag is written.
    #[default]
    Enabled,
    /// Marking off on every channel.
    Disabled,
    /// Marking off on the channels above this one, and on below it.
    DisabledAboveChannel(u32),
}

/// KDM generation configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KdmConfig {
    pub cpl_id: String,
    pub content_title: String,
    /// AnnotationText override. None derives `"<content_title> KDM for
    /// <recipient>"` (byte-identical to before this field existed).
    #[serde(default)]
    pub annotation: Option<String>,
    pub recipient_cert_file: PathBuf,
    /// Leaf certificate of the entity issuing this KDM. Its thumbprint is part
    /// of the encrypted key block and it is the certificate whose key signs the
    /// ETM ds:Signature, so a KDM cannot be built without it.
    pub signer_cert_file: PathBuf,
    /// RSA private key matching `signer_cert_file`, used to sign the message.
    pub signer_key_file: PathBuf,
    /// CA certificates above the signer leaf (intermediate(s) then root), in
    /// that order. Embedded in ds:KeyInfo after the leaf so a verifier can
    /// build the chain to a trust anchor. A self-signed signer needs none.
    pub signer_chain_files: Vec<PathBuf>,
    pub output_file: PathBuf,
    pub valid_from: String,
    pub valid_to: String,
    /// ISDCF formulation. Chooses whether a ContentAuthenticator is emitted, and
    /// has to agree with `device_cert_files`: the two device-listing
    /// formulations need certificates, the other two reject them.
    #[serde(default)]
    pub formulation: KdmFormulation,
    /// Content keys to carry, taken from the DCP's keys file so the KDM unlocks
    /// the essence that was actually encrypted. Empty makes `build_kdm` mint one
    /// fresh MDIK key (useful only for a test/DKDM with no bound DCP). Never
    /// serialized: it holds secret key material.
    #[serde(skip)]
    pub content_keys: Vec<KdmContentKey>,
    /// SMPTE (default) or legacy Interop output. Defaults to SMPTE so existing
    /// callers are byte-identical.
    #[serde(default)]
    pub format: KdmFormat,
    /// Certificates of the playback devices this KDM is restricted to, listed by
    /// thumbprint in AuthorizedDeviceInfo. Empty emits the DCI assume-trust
    /// thumbprint instead, which places no device restriction. The recipient's
    /// own certificate does not belong here: ISDCF Doc 5 deprecates the
    /// formulation that included it.
    #[serde(default)]
    pub device_cert_files: Vec<PathBuf>,
    /// Forensic marking of the picture essence. Defaults to Enabled, which
    /// writes no ForensicMarkFlagList at all.
    #[serde(default)]
    pub picture_forensic_marking: PictureForensicMarking,
    /// Forensic marking of the audio essence, likewise defaulting to Enabled.
    #[serde(default)]
    pub audio_forensic_marking: AudioForensicMarking,
}

/// A caller-supplied content key placed in a KDM, binding it to an already
/// encrypted DCP. Holds secret material and is redacted in Debug.
#[derive(Clone)]
pub struct KdmContentKey {
    /// ST 430-1 key type, e.g. `MDIK` (image) or `MDAK` (audio).
    pub key_type: [u8; 4],
    pub key_id: uuid::Uuid,
    pub content_key: [u8; 16],
}

impl std::fmt::Debug for KdmContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KdmContentKey")
            .field("key_type", &String::from_utf8_lossy(&self.key_type))
            .field("key_id", &self.key_id)
            .field("content_key", &"<redacted>")
            .finish()
    }
}

/// SMPTE ST 430-1 Table 6: identifies the KDM cipher block layout.
/// DCI CTP 3.4.16 fails any KDM carrying a different value.
const KDM_STRUCTURE_ID: [u8; 16] = [
    0xf1, 0xdc, 0x12, 0x44, 0x60, 0x16, 0x9a, 0x0e, 0x85, 0xbc, 0x30, 0x06, 0x42, 0xf8, 0x66, 0xab,
];

/// Total size of the SMPTE key block, per ST 430-1 Table 6.
const KDM_KEY_BLOCK_LEN: usize = 138;

/// Interop key block size: the SMPTE layout minus the 4-byte KeyType field,
/// matching libdcp's 134-byte case in decrypted_kdm.cc.
const KDM_KEY_BLOCK_LEN_INTEROP: usize = 134;

/// Interop (pre-SMPTE) KDMRequiredExtensions namespace, per libdcp.
const KDM_INTEROP_NS: &str = "http://www.digicine.com/PROTO-ASDCP-KDM-20040311#";

/// ST 430-1 6.3.7/6.3.8: timestamps are exactly 25 ASCII characters.
const KDM_TIMESTAMP_LEN: usize = 25;

/// XML Encryption 1.0 5.4.2, mandated by DCI CTP 3.4.12.
const KDM_ENCRYPTION_METHOD: &str = "http://www.w3.org/2001/04/xmlenc#rsa-oaep-mgf1p";

/// DCI DCSS 9.4.3.5 "assume trust" thumbprint: base64 SHA-1 of the empty string.
///
/// A DeviceList holding only this value tells the security manager the trusted
/// device requirement is already met. The rule is exclusive: put any real
/// thumbprint alongside it and assume-trust stops applying, so this value is
/// used alone or not at all.
const ASSUME_TRUST_THUMBPRINT: &str = "2jmj7l5rSw0yVb/vlWAYkK/YBwk=";

/// ST 430-1 Annex C ForensicMarkFlag URIs, as libdcp spells them in
/// `encrypted_kdm.cc`. Each one turns marking off for its essence type; the
/// element is absent when marking stays on.
const FORENSIC_MARK_PICTURE_DISABLE: &str =
    "http://www.smpte-ra.org/430-1/2006/KDM#mrkflg-picture-disable";
const FORENSIC_MARK_AUDIO_DISABLE: &str =
    "http://www.smpte-ra.org/430-1/2006/KDM#mrkflg-audio-disable";
/// Appended to the audio URI with a channel number when marking stops above
/// that channel rather than on all of them.
const FORENSIC_MARK_ABOVE_CHANNEL_SUFFIX: &str = "-above-channel-";
/// Element wrapping the marking flags, absent when marking stays on for both
/// essence types. Public so a caller can assert on a written KDM.
pub const FORENSIC_MARK_FLAG_LIST_ELEMENT: &str = "ForensicMarkFlagList";
/// Element holding one flag URI, as `forensic_mark_flag_uris` renders it.
pub const FORENSIC_MARK_FLAG_ELEMENT: &str = "ForensicMarkFlag";

// SMPTE 430-3 ETM ds:Signature profile. Every URI below is what libdcp emits
// in src/encrypted_kdm.cc / src/certificate_chain.cc for a KDM (distinct from
// the CPL/PKL signer), and is what DCI-compliant playback gear checks. The
// DSIG/c14n/signature/digest URIs live in `xmldsig`, the shared signer.
const ETM_NS: &str = "http://www.smpte-ra.org/schemas/430-3/2006/ETM";
const KDM_NS: &str = "http://www.smpte-ra.org/schemas/430-1/2006/KDM";
const ENC_NS: &str = "http://www.w3.org/2001/04/xmlenc#";
/// Element the DeviceList entries go into, one per authorized device (or the
/// single assume-trust value). Public so a caller can assert on a written KDM.
pub const CERTIFICATE_THUMBPRINT_ELEMENT: &str = "CertificateThumbprint";
/// Element naming the certificate whose key the security manager must find in
/// the CPL signer chain. Present only for the two `dci-*` formulations.
pub const CONTENT_AUTHENTICATOR_ELEMENT: &str = "ContentAuthenticator";
/// The two elements a distinguished name goes into, named for the same reason.
/// The issuer name is an XML-DSig element and carries that namespace's prefix;
/// the subject name is the KDM's own.
const X509_SUBJECT_NAME_ELEMENT: &str = "X509SubjectName";
const X509_ISSUER_NAME_ELEMENT: &str = "ds:X509IssuerName";

/// Id attribute values on the two authenticated elements. The ds:Reference
/// URIs point at these, and a verifier resolves them via the Id attribute.
const AUTH_PUBLIC_ID: &str = "ID_AuthenticatedPublic";
const AUTH_PRIVATE_ID: &str = "ID_AuthenticatedPrivate";

/// Check a validity timestamp is the exact 25-byte form ST 430-1 requires.
///
/// The key block has no room for anything else, so a bad value has to be an
/// error rather than something silently padded or truncated.
fn check_kdm_timestamp(label: &str, value: &str) -> Result<(), String> {
    if value.len() != KDM_TIMESTAMP_LEN || !value.is_ascii() {
        return Err(format!(
            "{label} must be exactly {KDM_TIMESTAMP_LEN} ASCII characters \
             (RFC 3339, e.g. 2004-05-01T13:20:00+00:00), got {} in '{value}'",
            value.len()
        ));
    }
    // ST 430-1 6.3.7: no 'Z' offset, no fractional seconds.
    if value.ends_with('Z') || value.contains('.') {
        return Err(format!(
            "{label} must use a numeric UTC offset and no fractional seconds, got '{value}'"
        ));
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|e| format!("{label} is not a valid RFC 3339 timestamp ('{value}'): {e}"))?;
    Ok(())
}

/// Build the plaintext key block. SMPTE (ST 430-1 Table 6) is 138 bytes:
/// structure id (16), signer thumbprint (20), CPL id (16), key type (4),
/// key id (16), not-valid-before (25), not-valid-after (25), content key (16).
/// Interop drops the 4-byte key type field, giving 134 bytes.
#[allow(clippy::too_many_arguments)]
fn build_kdm_key_block(
    format: KdmFormat,
    signer_thumbprint: &[u8; CERT_THUMBPRINT_LEN],
    cpl_id: &uuid::Uuid,
    key_type: &[u8; 4],
    key_id: &uuid::Uuid,
    not_before: &str,
    not_after: &str,
    content_key: &[u8; 16],
) -> Result<Vec<u8>, String> {
    check_kdm_timestamp("ContentKeysNotValidBefore", not_before)?;
    check_kdm_timestamp("ContentKeysNotValidAfter", not_after)?;

    let mut block = Vec::with_capacity(KDM_KEY_BLOCK_LEN);
    block.extend_from_slice(&KDM_STRUCTURE_ID);
    block.extend_from_slice(signer_thumbprint);
    block.extend_from_slice(cpl_id.as_bytes());
    if format == KdmFormat::Smpte {
        block.extend_from_slice(key_type);
    }
    block.extend_from_slice(key_id.as_bytes());
    block.extend_from_slice(not_before.as_bytes());
    block.extend_from_slice(not_after.as_bytes());
    block.extend_from_slice(content_key);

    // The layout is fixed; a mismatch means the code above drifted from the spec.
    let expected = match format {
        KdmFormat::Smpte => KDM_KEY_BLOCK_LEN,
        KdmFormat::Interop => KDM_KEY_BLOCK_LEN_INTEROP,
    };
    if block.len() != expected {
        return Err(format!(
            "internal error: key block is {} bytes, expected {expected}",
            block.len()
        ));
    }
    Ok(block)
}

/// Encrypt the key block to the recipient's public key.
///
/// RSAES-OAEP with MGF1, matching the `rsa-oaep-mgf1p` algorithm URI that DCI
/// CTP 3.4.12 requires. SHA-1 is the digest here because that URI fixes MGF1 to
/// SHA-1 and KDMs carry no ds:DigestMethod, so the OpenSSL default is what
/// every deployed implementation uses.
fn encrypt_key_block(public_key: &rsa::RsaPublicKey, block: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::traits::PublicKeyParts;

    // DCI DCSS 9.7.6 requires 2048-bit RSA. A shorter key is a hard error, not
    // a warning, since it would still produce a plausible-looking KDM.
    let modulus_bits = public_key.n().bits();
    if modulus_bits != 2048 {
        return Err(format!(
            "recipient RSA key is {modulus_bits} bits; DCI requires exactly 2048"
        ));
    }

    let ciphertext = public_key
        .encrypt(
            &mut rsa::rand_core::OsRng,
            rsa::Oaep::new::<sha1::Sha1>(),
            block,
        )
        .map_err(|e| format!("RSA-OAEP encryption of the key block failed: {e}"))?;

    if ciphertext.len() != modulus_bits / 8 {
        return Err(format!(
            "internal error: ciphertext is {} bytes, expected {}",
            ciphertext.len(),
            modulus_bits / 8
        ));
    }
    Ok(ciphertext)
}

/// A generated KDM plus the content key it carries.
///
/// The key is returned so callers can hand it to the MXF writer; it is never
/// written into the KDM itself.
pub struct GeneratedKdm {
    pub xml: String,
    pub content_key: [u8; 16],
    pub key_id: uuid::Uuid,
}

/// Redacts the content key so it cannot reach a log through a stray debug print.
impl std::fmt::Debug for GeneratedKdm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedKdm")
            .field("xml_len", &self.xml.len())
            .field("content_key", &"<redacted>")
            .field("key_id", &self.key_id)
            .finish()
    }
}

/// Build a SMPTE 430-1 KDM in memory, encrypting a fresh content key to the
/// recipient certificate and signing the message per SMPTE 430-3.
///
/// The returned XML carries a full ds:Signature over the AuthenticatedPublic
/// and AuthenticatedPrivate elements; it will not build if the signature cannot
/// be produced.
pub fn build_kdm(config: &KdmConfig) -> Result<GeneratedKdm, String> {
    if config.cpl_id.is_empty() {
        return Err("CPL ID is required".into());
    }

    let cpl_uuid = parse_cpl_id(&config.cpl_id)?;
    let recipient = parse_recipient(&config.recipient_cert_file)?;

    // The signer thumbprint is a required field of the key block, so a missing
    // signer certificate has to stop generation rather than be zero-filled.
    if config.signer_cert_file.as_os_str().is_empty() {
        return Err("signer certificate is required: its thumbprint is part of \
                    the SMPTE 430-1 key block"
            .into());
    }
    let signer = parse_signer(&config.signer_cert_file)?;

    let not_valid_before = resolve_valid_from(&config.valid_from);
    let not_valid_after = parse_validity_end(&config.valid_to, &not_valid_before)?;
    let window = KdmWindow::parse(&not_valid_before, &not_valid_after)?;
    check_kdm_window(
        &recipient,
        &config.recipient_cert_file,
        &config.signer_cert_file,
        &config.signer_chain_files,
        &window,
    )?;

    // Prefer the caller's keys (from the DCP's keys file) so the KDM unlocks the
    // essence that was actually encrypted; fall back to a fresh MDIK otherwise.
    let keys: Vec<KdmKey> = if config.content_keys.is_empty() {
        // MDIK: image essence key, ST 430-1 6.3.9.3 Table 1.
        vec![KdmKey {
            key_type: *b"MDIK",
            key_id: uuid::Uuid::new_v4(),
            content_key: rand_bytes()?,
        }]
    } else {
        config
            .content_keys
            .iter()
            .map(|k| KdmKey {
                key_type: k.key_type,
                key_id: k.key_id,
                content_key: k.content_key,
            })
            .collect()
    };
    let content_key = keys[0].content_key;
    let content_key_id = keys[0].key_id;

    let xml = build_kdm_xml(
        config,
        &cpl_uuid,
        &config.content_title,
        KDM_MESSAGE_TYPE,
        &not_valid_before,
        &not_valid_after,
        &recipient,
        &signer,
        &keys,
    )?;

    Ok(GeneratedKdm {
        xml,
        content_key,
        key_id: content_key_id,
    })
}

/// One content key carried by a KDM: its type (MDIK, MDAK, ...), id and value.
struct KdmKey {
    key_type: [u8; 4],
    key_id: uuid::Uuid,
    content_key: [u8; 16],
}

/// Accept either a bare UUID or the urn:uuid: form for a CPL id, rejecting
/// anything else: the key block needs the 16 raw bytes, not a free-text string.
fn parse_cpl_id(cpl_id: &str) -> Result<uuid::Uuid, String> {
    let trimmed = cpl_id
        .trim()
        .strip_prefix("urn:uuid:")
        .unwrap_or(cpl_id.trim());
    uuid::Uuid::parse_str(trimmed).map_err(|e| format!("CPL ID '{cpl_id}' is not a UUID: {e}"))
}

/// Resolve the not-valid-before value: "now"/empty means the current UTC time.
fn resolve_valid_from(valid_from: &str) -> String {
    if valid_from == "now" || valid_from.is_empty() {
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string()
    } else {
        valid_from.to_string()
    }
}

/// SMPTE ST 430-1 6.1: a KDM's MessageType is always this fixed URI. The ISDCF
/// "formulation" (modified-transitional-1, dci-any, ...) is a shorthand for a
/// combination of ContentAuthenticator presence and DeviceList contents, not a
/// MessageType, so it changes those two elements and never this one. Emitting a
/// per-formulation MessageType (the previous behaviour) produced a URI compliant
/// gear does not recognise as a KDM.
const KDM_MESSAGE_TYPE: &str = "http://www.smpte-ra.org/430-1/2006/KDM#kdm-key-type";

/// Reject a formulation that contradicts the device list, rather than silently
/// dropping certificates the caller supplied (which is what libdcp does).
fn check_formulation_devices(
    formulation: KdmFormulation,
    device_cert_files: &[PathBuf],
) -> Result<(), String> {
    let counterpart = formulation.device_list_counterpart();
    match (
        formulation.lists_supplied_devices(),
        device_cert_files.is_empty(),
    ) {
        (false, false) => Err(format!(
            "formulation {formulation} lists no devices, but {} device certificate(s) \
             were supplied; use {counterpart} to list them",
            device_cert_files.len()
        )),
        (true, true) => Err(format!(
            "formulation {formulation} needs at least one device certificate; \
             use {counterpart} for a KDM with no device restriction"
        )),
        _ => Ok(()),
    }
}

/// Assemble a signed SMPTE 430-1 KDM carrying `keys`, encrypting each key block
/// to `recipient` with `signer`'s thumbprint embedded.
///
/// `config` is used only for the signer identity handed to `build_signature`
/// (its cert, key and chain), the output format, the annotation, the
/// formulation, the authorized device list and the forensic marking flags;
/// every other field of the KDM comes from the explicit arguments so this core
/// serves both fresh generation and re-wrap.
#[allow(clippy::too_many_arguments)]
fn build_kdm_xml(
    config: &KdmConfig,
    cpl_uuid: &uuid::Uuid,
    content_title: &str,
    message_type: &str,
    not_valid_before: &str,
    not_valid_after: &str,
    recipient: &Recipient,
    signer: &Signer,
    keys: &[KdmKey],
) -> Result<String, String> {
    use base64::Engine;

    if keys.is_empty() {
        return Err("a KDM must carry at least one content key".into());
    }
    check_formulation_devices(config.formulation, &config.device_cert_files)?;

    let now = chrono::Utc::now();
    let message_id = uuid::Uuid::new_v4();

    // One TypedKeyId in KeyIdList and one EncryptedKey in AuthenticatedPrivate
    // per key, built in the same loop so their order stays paired.
    let mut typed_key_ids = String::new();
    let mut encrypted_keys = String::new();
    for key in keys {
        let key_type =
            std::str::from_utf8(&key.key_type).map_err(|_| "key type is not ASCII".to_string())?;

        let key_block = build_kdm_key_block(
            config.format,
            &signer.thumbprint,
            cpl_uuid,
            &key.key_type,
            &key.key_id,
            not_valid_before,
            not_valid_after,
            &key.content_key,
        )?;
        let ciphertext = encrypt_key_block(&recipient.public_key, &key_block)?;
        let cipher_value = base64::engine::general_purpose::STANDARD.encode(&ciphertext);

        // Interop has no KeyType, so its KeyIdList is bare KeyId elements.
        match config.format {
            KdmFormat::Smpte => typed_key_ids.push_str(&format!(
                r#"          <TypedKeyId>
            <KeyType>{key_type}</KeyType>
            <KeyId>urn:uuid:{key_id}</KeyId>
          </TypedKeyId>
"#,
                key_id = key.key_id,
            )),
            KdmFormat::Interop => typed_key_ids.push_str(&format!(
                "          <KeyId>urn:uuid:{key_id}</KeyId>\n",
                key_id = key.key_id,
            )),
        }
        encrypted_keys.push_str(&format!(
            r#"    <EncryptedKey xmlns="{ENC_NS}">
      <EncryptionMethod Algorithm="{KDM_ENCRYPTION_METHOD}"/>
      <CipherData>
        <CipherValue>{cipher_value}</CipherValue>
      </CipherData>
    </EncryptedKey>
"#,
        ));
    }

    let title = xml_escape(content_title);
    let recipient_subject = xml_escape(&recipient.subject_dn);
    let recipient_issuer = xml_escape(&recipient.issuer_dn);
    let recipient_serial = xml_escape(&recipient.serial);
    let signer_issuer = xml_escape(&signer.issuer_dn);
    let signer_serial = xml_escape(&signer.serial);

    let kdm_ns = match config.format {
        KdmFormat::Smpte => KDM_NS,
        KdmFormat::Interop => KDM_INTEROP_NS,
    };

    // AnnotationText: caller override (escaped), else the derived default.
    let annotation = match &config.annotation {
        Some(a) => xml_escape(a),
        None => format!("{title} KDM for {recipient_subject}"),
    };

    let authorized_device_info = build_authorized_device_info(&config.device_cert_files)?;
    let forensic_mark_flag_list = build_forensic_mark_flag_list(
        config.picture_forensic_marking,
        config.audio_forensic_marking,
    );

    // libdcp calls this approximate and it is: strictly the ContentAuthenticator
    // is a thumbprint of one of the CPL signer certificates, which is this
    // certificate only when the entity signing the KDM also signed the CPL.
    let content_authenticator = if config.formulation.carries_content_authenticator() {
        format!(
            "        <{CONTENT_AUTHENTICATOR_ELEMENT}>{}</{CONTENT_AUTHENTICATOR_ELEMENT}>\n",
            thumbprint_base64(&signer.thumbprint)
        )
    } else {
        String::new()
    };

    // Inner content of the two authenticated elements the signer references.
    let auth_public_inner = format!(
        r#"
    <MessageId>urn:uuid:{message_id}</MessageId>
    <MessageType>{message_type}</MessageType>
    <AnnotationText>{annotation}</AnnotationText>
    <IssueDate>{issue_date}</IssueDate>
    <Signer xmlns:ds="{DSIG_NS}">
      <{X509_ISSUER_NAME_ELEMENT}>{signer_issuer}</{X509_ISSUER_NAME_ELEMENT}>
      <ds:X509SerialNumber>{signer_serial}</ds:X509SerialNumber>
    </Signer>
    <RequiredExtensions>
      <KDMRequiredExtensions xmlns="{kdm_ns}">
        <Recipient>
          <X509IssuerSerial xmlns:ds="{DSIG_NS}">
            <{X509_ISSUER_NAME_ELEMENT}>{recipient_issuer}</{X509_ISSUER_NAME_ELEMENT}>
            <ds:X509SerialNumber>{recipient_serial}</ds:X509SerialNumber>
          </X509IssuerSerial>
          <{X509_SUBJECT_NAME_ELEMENT}>{recipient_subject}</{X509_SUBJECT_NAME_ELEMENT}>
        </Recipient>
        <CompositionPlaylistId>urn:uuid:{cpl_uuid}</CompositionPlaylistId>
        <ContentTitleText>{title}</ContentTitleText>
{content_authenticator}        <ContentKeysNotValidBefore>{not_before}</ContentKeysNotValidBefore>
        <ContentKeysNotValidAfter>{not_after}</ContentKeysNotValidAfter>
{authorized_device_info}        <KeyIdList>
{typed_key_ids}        </KeyIdList>
{forensic_mark_flag_list}      </KDMRequiredExtensions>
    </RequiredExtensions>
    <NonCriticalExtensions/>
  "#,
        issue_date = now.format("%Y-%m-%dT%H:%M:%S+00:00"),
        not_before = not_valid_before,
        not_after = not_valid_after,
    );

    let auth_private_inner = format!("\n{encrypted_keys}  ");

    // Build the unsigned message, then sign it with the shared enveloped-XML
    // signer. The root declares the ETM default namespace plus xmlns:ds, so the
    // signer reuses that ds prefix and produces a ds:Signature as the last child.
    let unsigned = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<DCinemaSecurityMessage xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}">
  <AuthenticatedPublic Id="{AUTH_PUBLIC_ID}">{auth_public_inner}</AuthenticatedPublic>
  <AuthenticatedPrivate Id="{AUTH_PRIVATE_ID}">{auth_private_inner}</AuthenticatedPrivate>
</DCinemaSecurityMessage>
"#,
    );

    let signer_identity = XmlSigner {
        cert_file: config.signer_cert_file.clone(),
        key_file: config.signer_key_file.clone(),
        chain_files: config.signer_chain_files.clone(),
    };
    crate::xmldsig::sign_enveloped(
        &unsigned,
        &[AUTH_PUBLIC_ID, AUTH_PRIVATE_ID],
        "Id",
        None,
        &signer_identity,
    )
}

/// Generate a SMPTE 430-1 Key Delivery Message (KDM) and write it to disk.
///
/// The content key is encrypted to the recipient certificate and the message is
/// signed per SMPTE 430-3 with a ds:Signature over the authenticated elements.
/// Signing is mandatory: if it cannot be produced no file is written.
pub fn generate_kdm(config: &KdmConfig) -> Result<(), String> {
    use std::io::Write;

    let kdm = build_kdm(config)?;
    let kdm_xml = kdm.xml;

    if let Some(parent) = config.output_file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create output directory: {e}"))?;
    }
    let mut file = std::fs::File::create(&config.output_file)
        .map_err(|e| format!("Cannot create KDM file: {e}"))?;
    file.write_all(kdm_xml.as_bytes())
        .map_err(|e| format!("Cannot write KDM: {e}"))?;

    tracing::info!("KDM generated: {}", config.output_file.display());
    Ok(())
}

/// Configuration for re-wrapping a DKDM to a new recipient.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RewrapConfig {
    /// Source distribution KDM (DKDM) XML, addressed to `dkdm_recipient_key`.
    pub dkdm_file: PathBuf,
    /// RSA private key of the DKDM's recipient, used to decrypt its key blocks.
    pub dkdm_recipient_key_file: PathBuf,
    /// Certificate of the new recipient the content keys are re-encrypted to.
    pub recipient_cert_file: PathBuf,
    /// Leaf certificate of the entity re-issuing this KDM. Its thumbprint goes
    /// into the new key blocks and its key signs the ETM ds:Signature.
    pub signer_cert_file: PathBuf,
    pub signer_key_file: PathBuf,
    pub signer_chain_files: Vec<PathBuf>,
    pub output_file: PathBuf,
    /// Empty to preserve the DKDM's ContentKeysNotValidBefore.
    pub valid_from: String,
    /// Empty to preserve the DKDM's ContentKeysNotValidAfter.
    pub valid_to: String,
    /// Certificates of the playback devices the re-wrapped KDM is restricted to.
    /// Empty emits the DCI assume-trust thumbprint. The source DKDM's device
    /// list is never carried over, because it names the DKDM recipient's devices
    /// rather than the new recipient's.
    #[serde(default)]
    pub device_cert_files: Vec<PathBuf>,
    /// ISDCF formulation of the re-wrapped KDM, with the same rules as
    /// `KdmConfig.formulation`. Not taken from the source DKDM, which carries no
    /// formulation of its own.
    #[serde(default)]
    pub formulation: KdmFormulation,
    /// Forensic marking of the picture essence in the re-wrapped KDM. Not taken
    /// from the source DKDM: the marking order belongs to whoever is issuing
    /// this KDM.
    #[serde(default)]
    pub picture_forensic_marking: PictureForensicMarking,
    /// Forensic marking of the audio essence, on the same terms.
    #[serde(default)]
    pub audio_forensic_marking: AudioForensicMarking,
}

/// Re-wrap a DKDM: decrypt its content keys with the DKDM recipient's private
/// key and re-encrypt them to a new recipient, then sign the result.
///
/// This is the cryptographically correct alternative to copying the source
/// CipherValue verbatim: the source bytes are RSA-encrypted to the DKDM
/// recipient, so a new recipient could never decrypt them. The recovered
/// content keys, key ids, types and CPL id are preserved; per libdcp
/// (decrypted_kdm.cc `DecryptedKDM::encrypt`) the new key blocks carry the
/// re-issuing signer's leaf thumbprint, and validity is preserved from the
/// source unless overridden. The returned GeneratedKdm surfaces the first
/// content key; every re-wrapped key lives in the returned XML.
pub fn rewrap_dkdm(config: &RewrapConfig) -> Result<GeneratedKdm, String> {
    let recipient = parse_recipient(&config.recipient_cert_file)?;

    if config.signer_cert_file.as_os_str().is_empty() {
        return Err("signer certificate is required: its thumbprint is part of \
                    the SMPTE 430-1 key block"
            .into());
    }
    let signer = parse_signer(&config.signer_cert_file)?;

    let dkdm_xml = std::fs::read_to_string(&config.dkdm_file)
        .map_err(|e| format!("cannot read DKDM {}: {e}", config.dkdm_file.display()))?;
    let parsed = parse_kdm_xml(&dkdm_xml)?;
    if parsed.cipher_values.is_empty() {
        return Err("DKDM has no EncryptedKey CipherValue in AuthenticatedPrivate".into());
    }

    let dkdm_key = load_rsa_private_key(&config.dkdm_recipient_key_file)?;

    let mut keys = Vec::with_capacity(parsed.cipher_values.len());
    let mut cpl_uuid: Option<uuid::Uuid> = None;
    let mut src_not_before: Option<String> = None;
    let mut src_not_after: Option<String> = None;
    for ciphertext in &parsed.cipher_values {
        use zeroize::Zeroize;
        let mut block = decrypt_key_block(&dkdm_key, ciphertext)?;
        let recovered = parse_kdm_key_block(&block, parsed.format)?;
        block.zeroize();

        // Re-wrap targets SMPTE key blocks, which need a key type.
        let key_type = recovered.key_type.ok_or_else(|| {
            "cannot re-wrap an Interop DKDM: its key block carries no key type".to_string()
        })?;

        // Every key in a KDM shares one CPL and one validity window.
        match cpl_uuid {
            Some(existing) if existing != recovered.cpl_id => {
                return Err("DKDM key blocks reference more than one CPL id".into());
            }
            None => cpl_uuid = Some(recovered.cpl_id),
            _ => {}
        }
        src_not_before.get_or_insert_with(|| recovered.not_before.clone());
        src_not_after.get_or_insert_with(|| recovered.not_after.clone());

        keys.push(KdmKey {
            key_type,
            key_id: recovered.key_id,
            content_key: recovered.content_key,
        });
    }
    let cpl_uuid = cpl_uuid.expect("at least one key block was decrypted");
    let src_not_before = src_not_before.expect("at least one key block was decrypted");
    let src_not_after = src_not_after.expect("at least one key block was decrypted");

    let not_valid_before = if config.valid_from.is_empty() {
        src_not_before
    } else {
        resolve_valid_from(&config.valid_from)
    };
    let not_valid_after = if config.valid_to.is_empty() {
        src_not_after
    } else {
        parse_validity_end(&config.valid_to, &not_valid_before)?
    };

    let window = KdmWindow::parse(&not_valid_before, &not_valid_after)?;
    check_kdm_window(
        &recipient,
        &config.recipient_cert_file,
        &config.signer_cert_file,
        &config.signer_chain_files,
        &window,
    )?;

    // Preserve the source MessageType and title.
    let message_type = parsed.message_type.as_deref().unwrap_or(KDM_MESSAGE_TYPE);
    let content_title = parsed.content_title.as_deref().unwrap_or("");

    // build_kdm_xml reads only the signer identity, the device list, the
    // formulation and the forensic marking flags from the config.
    let signer_config = KdmConfig {
        signer_cert_file: config.signer_cert_file.clone(),
        signer_key_file: config.signer_key_file.clone(),
        signer_chain_files: config.signer_chain_files.clone(),
        device_cert_files: config.device_cert_files.clone(),
        formulation: config.formulation,
        picture_forensic_marking: config.picture_forensic_marking,
        audio_forensic_marking: config.audio_forensic_marking,
        ..Default::default()
    };

    let xml = build_kdm_xml(
        &signer_config,
        &cpl_uuid,
        content_title,
        message_type,
        &not_valid_before,
        &not_valid_after,
        &recipient,
        &signer,
        &keys,
    )?;

    let first = &keys[0];
    Ok(GeneratedKdm {
        xml,
        content_key: first.content_key,
        key_id: first.key_id,
    })
}

/// Re-wrap a DKDM and write the resulting KDM to disk.
pub fn rewrap_dkdm_to_file(config: &RewrapConfig) -> Result<(), String> {
    use std::io::Write;

    let kdm = rewrap_dkdm(config)?;

    if let Some(parent) = config.output_file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create output directory: {e}"))?;
    }
    let mut file = std::fs::File::create(&config.output_file)
        .map_err(|e| format!("Cannot create KDM file: {e}"))?;
    file.write_all(kdm.xml.as_bytes())
        .map_err(|e| format!("Cannot write KDM: {e}"))?;

    tracing::info!("re-wrapped KDM written: {}", config.output_file.display());
    Ok(())
}

/// A KeyId from a KDM's public KeyIdList. `key_type` is None for Interop, whose
/// KeyIdList carries bare KeyId elements with no type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdmKeyId {
    pub key_type: Option<[u8; 4]>,
    pub key_id: uuid::Uuid,
}

/// Public metadata read from a KDM without the recipient key: what the KDM is
/// for and which keys it carries. The content keys themselves come only from
/// `unwrap_kdm`, which needs the recipient private key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KdmMetadata {
    pub format: KdmFormat,
    pub cpl_id: uuid::Uuid,
    pub content_title: String,
    pub annotation_text: String,
    /// ST 430-1 ContentKeysNotValidBefore, the RFC 3339 start of the window.
    pub not_valid_before: String,
    /// ST 430-1 ContentKeysNotValidAfter, the RFC 3339 end of the window.
    pub not_valid_after: String,
    pub key_ids: Vec<KdmKeyId>,
}

/// Everything read from a KDM's XML without the recipient key: the public
/// metadata plus the base64-decoded EncryptedKey ciphertexts.
struct ParsedKdmXml {
    format: KdmFormat,
    /// Base64-decoded ciphertext of every EncryptedKey under AuthenticatedPrivate.
    cipher_values: Vec<Vec<u8>>,
    content_title: Option<String>,
    message_type: Option<String>,
    annotation_text: Option<String>,
    cpl_id: Option<uuid::Uuid>,
    not_valid_before: Option<String>,
    not_valid_after: Option<String>,
    key_ids: Vec<KdmKeyId>,
}

/// Accept a `urn:uuid:` or bare UUID string, rejecting anything else.
fn parse_urn_uuid(value: &str) -> Result<uuid::Uuid, String> {
    let trimmed = value
        .trim()
        .strip_prefix("urn:uuid:")
        .unwrap_or(value.trim());
    uuid::Uuid::parse_str(trimmed).map_err(|e| format!("'{value}' is not a UUID: {e}"))
}

/// Parse a KDM's XML (SMPTE or Interop): the encrypted key blocks and the public
/// metadata. No private key is needed; nothing here decrypts a content key.
///
/// CipherValues are collected only from within AuthenticatedPrivate so nothing
/// outside the private block can be mistaken for a content key. Format is taken
/// from the KDMRequiredExtensions namespace.
fn parse_kdm_xml(xml: &str) -> Result<ParsedKdmXml, String> {
    use base64::Engine;
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let format = if xml.contains(KDM_INTEROP_NS) {
        KdmFormat::Interop
    } else {
        KdmFormat::Smpte
    };

    let mut reader = Reader::from_str(xml);
    let mut in_auth_private = false;
    let mut in_key_id_list = false;
    // Type of the current TypedKeyId (SMPTE); None until a KeyType is seen.
    let mut pending_key_type: Option<[u8; 4]> = None;
    // Set while text is being gathered for the named field.
    let mut collecting: Option<&'static str> = None;
    let mut buffer = String::new();

    let mut cipher_values = Vec::new();
    let mut content_title = None;
    let mut message_type = None;
    let mut annotation_text = None;
    let mut cpl_id = None;
    let mut not_valid_before = None;
    let mut not_valid_after = None;
    let mut key_ids = Vec::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| format!("KDM is not valid XML: {e}"))?
        {
            Event::Start(e) => match e.local_name().as_ref() {
                b"AuthenticatedPrivate" => in_auth_private = true,
                b"KeyIdList" => {
                    in_key_id_list = true;
                    pending_key_type = None;
                }
                b"CipherValue" if in_auth_private => {
                    collecting = Some("cipher");
                    buffer.clear();
                }
                b"ContentTitleText" => {
                    collecting = Some("title");
                    buffer.clear();
                }
                b"MessageType" => {
                    collecting = Some("message_type");
                    buffer.clear();
                }
                b"AnnotationText" => {
                    collecting = Some("annotation");
                    buffer.clear();
                }
                b"CompositionPlaylistId" => {
                    collecting = Some("cpl");
                    buffer.clear();
                }
                b"ContentKeysNotValidBefore" => {
                    collecting = Some("not_before");
                    buffer.clear();
                }
                b"ContentKeysNotValidAfter" => {
                    collecting = Some("not_after");
                    buffer.clear();
                }
                b"KeyType" if in_key_id_list => {
                    collecting = Some("key_type");
                    buffer.clear();
                }
                b"KeyId" if in_key_id_list => {
                    collecting = Some("key_id");
                    buffer.clear();
                }
                _ => {}
            },
            Event::Text(e) if collecting.is_some() => {
                let text = e
                    .unescape()
                    .map_err(|err| format!("KDM text is not valid XML: {err}"))?;
                buffer.push_str(&text);
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"AuthenticatedPrivate" => in_auth_private = false,
                b"KeyIdList" => in_key_id_list = false,
                b"CipherValue" if collecting == Some("cipher") => {
                    let stripped: String = buffer.split_whitespace().collect();
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(stripped.as_bytes())
                        .map_err(|e| format!("KDM CipherValue is not valid base64: {e}"))?;
                    cipher_values.push(bytes);
                    collecting = None;
                }
                b"ContentTitleText" if collecting == Some("title") => {
                    content_title = Some(buffer.trim().to_string());
                    collecting = None;
                }
                b"MessageType" if collecting == Some("message_type") => {
                    message_type = Some(buffer.trim().to_string());
                    collecting = None;
                }
                b"AnnotationText" if collecting == Some("annotation") => {
                    annotation_text = Some(buffer.trim().to_string());
                    collecting = None;
                }
                b"CompositionPlaylistId" if collecting == Some("cpl") => {
                    cpl_id = Some(parse_urn_uuid(buffer.trim())?);
                    collecting = None;
                }
                b"ContentKeysNotValidBefore" if collecting == Some("not_before") => {
                    not_valid_before = Some(buffer.trim().to_string());
                    collecting = None;
                }
                b"ContentKeysNotValidAfter" if collecting == Some("not_after") => {
                    not_valid_after = Some(buffer.trim().to_string());
                    collecting = None;
                }
                b"KeyType" if collecting == Some("key_type") => {
                    let bytes = buffer.trim().as_bytes();
                    if bytes.len() != 4 {
                        return Err(format!(
                            "KDM KeyType must be 4 ASCII characters, got '{}'",
                            buffer.trim()
                        ));
                    }
                    pending_key_type = Some([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    collecting = None;
                }
                b"KeyId" if collecting == Some("key_id") => {
                    key_ids.push(KdmKeyId {
                        key_type: pending_key_type.take(),
                        key_id: parse_urn_uuid(buffer.trim())?,
                    });
                    collecting = None;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(ParsedKdmXml {
        format,
        cipher_values,
        content_title,
        message_type,
        annotation_text,
        cpl_id,
        not_valid_before,
        not_valid_after,
        key_ids,
    })
}

/// One content key recovered from a KDM: which essence it unlocks and the raw
/// 16-byte AES-128 key. Secret material: the key is private, redacted in Debug
/// and zeroed on drop. Read it with `content_key`.
pub struct UnwrappedKey {
    pub key_id: uuid::Uuid,
    /// ST 430-1 key type (MDIK/MDAK/...); None for Interop, whose block has none.
    pub key_type: Option<[u8; 4]>,
    pub cpl_id: uuid::Uuid,
    pub not_valid_before: String,
    pub not_valid_after: String,
    content_key: [u8; 16],
}

impl UnwrappedKey {
    /// The raw 16-byte AES-128 content key this KDM entry unlocks.
    pub fn content_key(&self) -> &[u8; 16] {
        &self.content_key
    }
}

/// Redacts the content key so it cannot reach a log through a stray debug print.
impl std::fmt::Debug for UnwrappedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnwrappedKey")
            .field("key_id", &self.key_id)
            .field(
                "key_type",
                &self
                    .key_type
                    .map(|t| String::from_utf8_lossy(&t).into_owned()),
            )
            .field("cpl_id", &self.cpl_id)
            .field("not_valid_before", &self.not_valid_before)
            .field("not_valid_after", &self.not_valid_after)
            .field("content_key", &"<redacted>")
            .finish()
    }
}

impl Drop for UnwrappedKey {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.content_key.zeroize();
    }
}

/// The content keys recovered from a KDM, keyed by KeyId. Every key is zeroed on
/// drop; none is ever logged. Look one up with `content_key`.
#[derive(Debug)]
pub struct UnwrappedKdm {
    pub format: KdmFormat,
    pub cpl_id: uuid::Uuid,
    pub keys: Vec<UnwrappedKey>,
}

impl UnwrappedKdm {
    /// The 16-byte AES-128 content key for `key_id`, if this KDM carried it.
    pub fn content_key(&self, key_id: &uuid::Uuid) -> Option<&[u8; 16]> {
        self.keys
            .iter()
            .find(|k| &k.key_id == key_id)
            .map(|k| k.content_key())
    }
}

/// Read a KDM's public metadata (CPL id, validity window, KeyIds and types)
/// without decrypting anything. Works for both SMPTE and Interop KDMs and needs
/// no recipient key.
pub fn parse_kdm(kdm_xml: &str) -> Result<KdmMetadata, String> {
    let parsed = parse_kdm_xml(kdm_xml)?;
    Ok(KdmMetadata {
        format: parsed.format,
        cpl_id: parsed.cpl_id.ok_or("KDM has no CompositionPlaylistId")?,
        content_title: parsed.content_title.unwrap_or_default(),
        annotation_text: parsed.annotation_text.unwrap_or_default(),
        not_valid_before: parsed
            .not_valid_before
            .ok_or("KDM has no ContentKeysNotValidBefore")?,
        not_valid_after: parsed
            .not_valid_after
            .ok_or("KDM has no ContentKeysNotValidAfter")?,
        key_ids: parsed.key_ids,
    })
}

/// Decrypt a KDM's content keys with the recipient's RSA private key.
///
/// The inverse of `build_kdm`/`generate_kdm`: parses the KDM (SMPTE or Interop),
/// RSA-OAEP-decrypts every EncryptedKey with the recipient key, parses each
/// plaintext key block and returns the recovered KeyId -> AES-128 key map. A
/// wrong recipient key fails loud (the OAEP unpad or the key block structure-id
/// check rejects it) rather than returning garbage keys. The returned keys are
/// zeroed on drop and never logged.
pub fn unwrap_kdm(kdm_xml: &str, recipient_key_file: &Path) -> Result<UnwrappedKdm, String> {
    let parsed = parse_kdm_xml(kdm_xml)?;
    if parsed.cipher_values.is_empty() {
        return Err("KDM has no EncryptedKey CipherValue in AuthenticatedPrivate".into());
    }
    let key = load_rsa_private_key(recipient_key_file)?;

    let mut keys = Vec::with_capacity(parsed.cipher_values.len());
    let mut cpl_uuid: Option<uuid::Uuid> = None;
    for ciphertext in &parsed.cipher_values {
        use zeroize::Zeroize;
        let mut block = decrypt_key_block(&key, ciphertext)?;
        let recovered = parse_kdm_key_block(&block, parsed.format)?;
        block.zeroize();

        // Every key in one KDM shares a single CPL id.
        match cpl_uuid {
            Some(existing) if existing != recovered.cpl_id => {
                return Err("KDM key blocks reference more than one CPL id".into());
            }
            None => cpl_uuid = Some(recovered.cpl_id),
            _ => {}
        }

        keys.push(UnwrappedKey {
            key_id: recovered.key_id,
            key_type: recovered.key_type,
            cpl_id: recovered.cpl_id,
            not_valid_before: recovered.not_before.clone(),
            not_valid_after: recovered.not_after.clone(),
            content_key: recovered.content_key,
        });
    }
    let cpl_id = cpl_uuid.expect("at least one key block was decrypted");
    Ok(UnwrappedKdm {
        format: parsed.format,
        cpl_id,
        keys,
    })
}

/// Decrypt a KDM file's content keys with the recipient's RSA private key.
pub fn unwrap_kdm_file(kdm_file: &Path, recipient_key_file: &Path) -> Result<UnwrappedKdm, String> {
    let xml = std::fs::read_to_string(kdm_file)
        .map_err(|e| format!("cannot read KDM {}: {e}", kdm_file.display()))?;
    unwrap_kdm(&xml, recipient_key_file)
}

/// A content key recovered from a decrypted key block. Holds secret material:
/// `content_key` is zeroed when this value drops and never logged. `key_type` is
/// None for Interop, whose block carries no type field.
struct RecoveredKey {
    cpl_id: uuid::Uuid,
    key_type: Option<[u8; 4]>,
    key_id: uuid::Uuid,
    not_before: String,
    not_after: String,
    content_key: [u8; 16],
}

impl Drop for RecoveredKey {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.content_key.zeroize();
    }
}

/// Parse a decrypted key block back into its fields, for the layout given by
/// `format`: SMPTE (138 bytes, with a 4-byte key type) or Interop (134, none).
///
/// The layout mirrors `build_kdm_key_block`. A wrong length or a bad structure
/// id means the wrong recipient key was used or the KDM is corrupt; either is
/// fatal. The signer thumbprint at [16..36] is the original issuer's and is
/// discarded: on re-wrap the new key block carries the re-issuer's thumbprint.
fn parse_kdm_key_block(block: &[u8], format: KdmFormat) -> Result<RecoveredKey, String> {
    let expected = match format {
        KdmFormat::Smpte => KDM_KEY_BLOCK_LEN,
        KdmFormat::Interop => KDM_KEY_BLOCK_LEN_INTEROP,
    };
    if block.len() != expected {
        return Err(format!(
            "decrypted key block is {} bytes, expected {expected} \
             (wrong recipient key or corrupt KDM)",
            block.len()
        ));
    }
    if block[..16] != KDM_STRUCTURE_ID {
        return Err("decrypted key block has a bad structure id \
                    (wrong recipient key or corrupt KDM)"
            .into());
    }

    let cpl_id = uuid::Uuid::from_slice(&block[36..52])
        .map_err(|e| format!("key block has a malformed CPL id: {e}"))?;

    // SMPTE carries the 4-byte key type before the key id; Interop omits it.
    let (key_type, mut off) = match format {
        KdmFormat::Smpte => {
            let mut kt = [0u8; 4];
            kt.copy_from_slice(&block[52..56]);
            (Some(kt), 56usize)
        }
        KdmFormat::Interop => (None, 52usize),
    };

    let key_id = uuid::Uuid::from_slice(&block[off..off + 16])
        .map_err(|e| format!("key block has a malformed key id: {e}"))?;
    off += 16;
    let not_before = std::str::from_utf8(&block[off..off + KDM_TIMESTAMP_LEN])
        .map_err(|_| "key block not-valid-before is not ASCII".to_string())?
        .to_string();
    off += KDM_TIMESTAMP_LEN;
    let not_after = std::str::from_utf8(&block[off..off + KDM_TIMESTAMP_LEN])
        .map_err(|_| "key block not-valid-after is not ASCII".to_string())?
        .to_string();
    off += KDM_TIMESTAMP_LEN;
    let mut content_key = [0u8; 16];
    content_key.copy_from_slice(&block[off..off + 16]);

    Ok(RecoveredKey {
        cpl_id,
        key_type,
        key_id,
        not_before,
        not_after,
        content_key,
    })
}

/// Decrypt one RSA-OAEP-SHA1 key block with a recipient private key.
///
/// This is the inverse of `encrypt_key_block`; the SHA-1 digest matches the
/// `rsa-oaep-mgf1p` algorithm URI KDMs are fixed to.
fn decrypt_key_block(
    private_key: &rsa::RsaPrivateKey,
    ciphertext: &[u8],
) -> Result<Vec<u8>, String> {
    private_key
        .decrypt(rsa::Oaep::new::<sha1::Sha1>(), ciphertext)
        .map_err(|e| {
            format!(
                "RSA-OAEP decryption of a DKDM key block failed \
                 (wrong recipient key or corrupt DKDM): {e}"
            )
        })
}

/// Load an RSA private key (PKCS#8 or PKCS#1 PEM) without matching it to a cert.
fn load_rsa_private_key(key_path: &Path) -> Result<rsa::RsaPrivateKey, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;

    let pem = std::fs::read_to_string(key_path)
        .map_err(|e| format!("cannot read private key {}: {e}", key_path.display()))?;
    rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(&pem))
        .map_err(|e| {
            format!(
                "private key {} is not a valid RSA private key (PKCS#8 or PKCS#1 PEM): {e}",
                key_path.display()
            )
        })
}

/// Parse a validity end value: either an ISO 8601 date or a relative duration.
fn parse_validity_end(value: &str, start: &str) -> Result<String, String> {
    // If it looks like ISO 8601, use directly
    if value.contains('T') || value.len() >= 10 && value.chars().nth(4) == Some('-') {
        return Ok(value.to_string());
    }

    // Parse as relative duration from start
    let start_dt = chrono::DateTime::parse_from_rfc3339(start)
        .or_else(|_| chrono::DateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S%:z"))
        .map_err(|e| format!("Cannot parse start date '{start}': {e}"))?;

    let duration = parse_duration(value)?;
    let end = start_dt + duration;
    Ok(end.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
}

/// Parse a human-friendly duration string.
fn parse_duration(s: &str) -> Result<chrono::Duration, String> {
    let s = s.trim().to_lowercase();
    let parts: Vec<&str> = s.split_whitespace().collect();

    if parts.len() == 2 {
        let n: i64 = parts[0]
            .parse()
            .map_err(|_| format!("Invalid number in duration: '{}'", parts[0]))?;
        let unit = parts[1].trim_end_matches('s');
        return match unit {
            "second" | "sec" => Ok(chrono::Duration::seconds(n)),
            "minute" | "min" => Ok(chrono::Duration::minutes(n)),
            "hour" | "hr" => Ok(chrono::Duration::hours(n)),
            "day" => Ok(chrono::Duration::days(n)),
            "week" | "wk" => Ok(chrono::Duration::weeks(n)),
            _ => Err(format!("Unknown duration unit: '{unit}'")),
        };
    }

    // Try suffix format: 7d, 24h, 2w
    if let Some(stripped) = s.strip_suffix('h') {
        let n: i64 = stripped
            .parse()
            .map_err(|_| format!("Invalid duration: '{s}'"))?;
        return Ok(chrono::Duration::hours(n));
    }
    if let Some(stripped) = s.strip_suffix('d') {
        let n: i64 = stripped
            .parse()
            .map_err(|_| format!("Invalid duration: '{s}'"))?;
        return Ok(chrono::Duration::days(n));
    }
    if let Some(stripped) = s.strip_suffix('w') {
        let n: i64 = stripped
            .parse()
            .map_err(|_| format!("Invalid duration: '{s}'"))?;
        return Ok(chrono::Duration::weeks(n));
    }

    Err(format!("Cannot parse duration: '{s}'"))
}

/// Length of an ST 430-2 thumbprint: SHA-1 is a 160-bit digest.
const CERT_THUMBPRINT_LEN: usize = 20;

/// Certificate thumbprint per SMPTE ST 430-2: SHA-1 over the DER-encoded
/// TBSCertificate (the signed portion), not the whole certificate.
///
/// Matches libdcp's `Certificate::thumbprint()`, which hashes
/// `i2d_re_X509_tbs` output. ST 430-2 5.4 says to exclude the DER tag and
/// length, but libdcp includes them and is what deployed gear agrees with, so
/// `tbs_der` is expected to be the complete TBSCertificate encoding.
fn cert_thumbprint(tbs_der: &[u8]) -> [u8; CERT_THUMBPRINT_LEN] {
    use sha1::Digest;
    sha1::Sha1::digest(tbs_der).into()
}

/// The thumbprint as ST 430-1 spells it in XML: base64.
fn thumbprint_base64(thumbprint: &[u8; CERT_THUMBPRINT_LEN]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(thumbprint)
}

/// The same thumbprint as a trusted-device file stem: hex, because the base64
/// spelling contains '/'.
fn thumbprint_stem(thumbprint: &[u8; CERT_THUMBPRINT_LEN]) -> String {
    hex::encode(thumbprint)
}

/// Read a certificate file and compute its ST 430-2 thumbprint.
fn read_cert_thumbprint(cert_path: &Path) -> Result<[u8; CERT_THUMBPRINT_LEN], String> {
    use x509_parser::prelude::*;

    let data = std::fs::read(cert_path)
        .map_err(|e| format!("cannot read device cert {}: {e}", cert_path.display()))?;
    let (_, pem) = parse_x509_pem(&data)
        .map_err(|e| format!("device cert {} is not valid PEM: {e}", cert_path.display()))?;
    let cert = pem.parse_x509().map_err(|e| {
        format!(
            "device cert {} is not valid X.509: {e}",
            cert_path.display()
        )
    })?;
    Ok(cert_thumbprint(cert.tbs_certificate.as_ref()))
}

/// Certificate thumbprint of one authorized playback device, base64 encoded as
/// the ST 430-1 Annex B CertificateThumbprint requires.
fn read_device_thumbprint(cert_path: &Path) -> Result<String, String> {
    Ok(thumbprint_base64(&read_cert_thumbprint(cert_path)?))
}

/// Build the AuthorizedDeviceInfo element of ST 430-1 Annex B.
///
/// With no device certificates the list carries the DCI assume-trust thumbprint
/// alone, matching libdcp's unrestricted KDM. An empty DeviceList is not an
/// option: CertificateThumbprint is minOccurs="1".
fn build_authorized_device_info(device_cert_files: &[PathBuf]) -> Result<String, String> {
    let mut thumbprints = Vec::with_capacity(device_cert_files.len());
    for cert_path in device_cert_files {
        thumbprints.push(read_device_thumbprint(cert_path)?);
    }
    if thumbprints.is_empty() {
        thumbprints.push(ASSUME_TRUST_THUMBPRINT.to_string());
    }

    // base64 has no character XML would have to escape
    let entries: String = thumbprints
        .iter()
        .map(|t| {
            format!(
                "            <{CERTIFICATE_THUMBPRINT_ELEMENT}>{t}</{CERTIFICATE_THUMBPRINT_ELEMENT}>\n"
            )
        })
        .collect();

    Ok(format!(
        r#"        <AuthorizedDeviceInfo>
          <DeviceListIdentifier>urn:uuid:{device_list_id}</DeviceListIdentifier>
          <DeviceList>
{entries}          </DeviceList>
        </AuthorizedDeviceInfo>
"#,
        device_list_id = uuid::Uuid::new_v4(),
    ))
}

/// The ST 430-1 Annex C flag URIs a marking pair writes, in the order they go
/// into the KDM: picture first, matching libdcp. Empty when marking stays on
/// for both, which is the case where no ForensicMarkFlagList is written at all.
///
/// These are the `ForensicMarkFlag` elements a generated KDM ends up carrying,
/// so a caller checking its own output asks here rather than spelling the URIs
/// out again.
pub fn forensic_mark_flag_uris(
    picture: PictureForensicMarking,
    audio: AudioForensicMarking,
) -> Vec<String> {
    let picture_flag = match picture {
        PictureForensicMarking::Enabled => None,
        PictureForensicMarking::Disabled => Some(FORENSIC_MARK_PICTURE_DISABLE.to_string()),
    };
    let audio_flag = match audio {
        AudioForensicMarking::Enabled => None,
        // libdcp appends the suffix only above channel zero, so channel 0 says
        // the same thing as Disabled.
        AudioForensicMarking::Disabled | AudioForensicMarking::DisabledAboveChannel(0) => {
            Some(FORENSIC_MARK_AUDIO_DISABLE.to_string())
        }
        AudioForensicMarking::DisabledAboveChannel(channel) => Some(format!(
            "{FORENSIC_MARK_AUDIO_DISABLE}{FORENSIC_MARK_ABOVE_CHANNEL_SUFFIX}{channel}"
        )),
    };
    [picture_flag, audio_flag].into_iter().flatten().collect()
}

/// Build the ForensicMarkFlagList of ST 430-1 Annex C, empty when nothing is
/// disabled: the element is `minOccurs="0"` and libdcp omits it entirely rather
/// than writing an empty list.
fn build_forensic_mark_flag_list(
    picture: PictureForensicMarking,
    audio: AudioForensicMarking,
) -> String {
    // A URI has no character XML would have to escape.
    let flags: String = forensic_mark_flag_uris(picture, audio)
        .into_iter()
        .map(|uri| {
            format!(
                "          <{FORENSIC_MARK_FLAG_ELEMENT}>{uri}</{FORENSIC_MARK_FLAG_ELEMENT}>\n"
            )
        })
        .collect();
    if flags.is_empty() {
        return String::new();
    }
    format!(
        "        <{FORENSIC_MARK_FLAG_LIST_ELEMENT}>\n\
         {flags}        </{FORENSIC_MARK_FLAG_LIST_ELEMENT}>\n"
    )
}

/// Identity of the entity issuing a KDM. ST 430-3 types the ETM Signer as
/// `ds:X509IssuerSerialType`, which carries issuer and serial and no subject.
struct Signer {
    issuer_dn: String,
    serial: String,
    thumbprint: [u8; CERT_THUMBPRINT_LEN],
}

/// Parse the signer certificate for the identity and thumbprint the key block needs.
fn parse_signer(cert_path: &Path) -> Result<Signer, String> {
    use x509_parser::prelude::*;

    let data = std::fs::read(cert_path)
        .map_err(|e| format!("cannot read signer cert {}: {e}", cert_path.display()))?;
    let (_, pem) = parse_x509_pem(&data)
        .map_err(|e| format!("signer cert {} is not valid PEM: {e}", cert_path.display()))?;
    let cert = pem.parse_x509().map_err(|e| {
        format!(
            "signer cert {} is not valid X.509: {e}",
            cert_path.display()
        )
    })?;

    Ok(Signer {
        issuer_dn: distinguished_name(cert.issuer()),
        serial: cert.serial.to_str_radix(10),
        thumbprint: cert_thumbprint(cert.tbs_certificate.as_ref()),
    })
}

/// Identity and public key of a KDM recipient, parsed from its certificate.
struct Recipient {
    /// Subject DN in RFC 2253 form, as SMPTE 430-1 expects for X509SubjectName.
    subject_dn: String,
    /// Issuer DN in RFC 2253 form, for the X509IssuerSerial recipient reference.
    issuer_dn: String,
    serial: String,
    public_key: rsa::RsaPublicKey,
    /// Validity bounds, so a KDM window can be classified against them.
    not_before: chrono::DateTime<chrono::Utc>,
    not_after: chrono::DateTime<chrono::Utc>,
}

/// Parse a recipient certificate: identity plus the RSA key the content key is wrapped to.
///
/// Every failure here is fatal. Falling back to a placeholder identity or a
/// missing key would mean emitting a KDM nobody can use, or worse, an
/// unencrypted one.
fn parse_recipient(cert_path: &Path) -> Result<Recipient, String> {
    use rsa::pkcs8::DecodePublicKey;
    use x509_parser::prelude::*;

    let data = std::fs::read(cert_path)
        .map_err(|e| format!("cannot read recipient cert {}: {e}", cert_path.display()))?;
    let (_, pem) = parse_x509_pem(&data).map_err(|e| {
        format!(
            "recipient cert {} is not valid PEM: {e}",
            cert_path.display()
        )
    })?;
    let cert = pem.parse_x509().map_err(|e| {
        format!(
            "recipient cert {} is not valid X.509: {e}",
            cert_path.display()
        )
    })?;

    let spki = cert.public_key();
    // Reject non-RSA up front so the OAEP step can't be reached with a key it cannot use.
    match spki.parsed() {
        Ok(x509_parser::public_key::PublicKey::RSA(_)) => {}
        Ok(_) => {
            return Err(format!(
                "recipient cert {} does not hold an RSA key; SMPTE 430-1 KDMs require RSA",
                cert_path.display()
            ));
        }
        Err(e) => {
            return Err(format!(
                "cannot parse public key from {}: {e}",
                cert_path.display()
            ));
        }
    }

    let public_key = rsa::RsaPublicKey::from_public_key_der(spki.raw).map_err(|e| {
        format!(
            "cannot load RSA public key from {}: {e}",
            cert_path.display()
        )
    })?;

    let not_before = certificate_validity_timestamp(cert.validity().not_before)?;
    let not_after = certificate_validity_timestamp(cert.validity().not_after)?;

    Ok(Recipient {
        subject_dn: distinguished_name(cert.subject()),
        issuer_dn: distinguished_name(cert.issuer()),
        // X509SerialNumber is a decimal integer in XML-DSig
        serial: cert.serial.to_str_radix(10),
        public_key,
        not_before,
        not_after,
    })
}

/// Fill a buffer from the OS CSPRNG.
///
/// There is deliberately no fallback: anything other than a real CSPRNG here
/// yields a guessable content key, so RNG failure has to be fatal.
fn rand_bytes<const N: usize>() -> Result<[u8; N], String> {
    use ring::rand::SecureRandom;

    let mut buf = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| "CSPRNG unavailable, refusing to generate a content key".to_string())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmldsig::{C14N_METHOD, DIGEST_METHOD, SIG_METHOD, c14n};
    use base64::Engine;
    use std::sync::OnceLock;

    /// A real certificate chain plus a second root that shares the real root's
    /// CN but has a different key. Generated once, RSA keygen is expensive.
    struct Fixtures {
        _dir: tempfile::TempDir,
        root: PathBuf,
        root_key: PathBuf,
        intermediate: PathBuf,
        signer: PathBuf,
        signer_key: PathBuf,
        /// The genuine root's whole distinguished name, dnQualifier included,
        /// over a different key. Used to prove chain validation checks
        /// signatures and not just names.
        impostor_root: PathBuf,
    }

    fn fixtures() -> &'static Fixtures {
        static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
        FIXTURES.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let p = dir.path();
            assert_eq!(generate_chain("Acme", p), 0, "chain generation failed");

            // The dnQualifier is derived from the key, so an impostor cannot be
            // built by asking for the same subject: its DN is copied off the
            // genuine root and only the key underneath is swapped.
            let impostor_root = p.join("impostor_root.pem");
            let genuine_pem = std::fs::read_to_string(p.join("root.pem")).expect("read root");
            let params =
                rcgen::CertificateParams::from_ca_cert_pem(&genuine_pem).expect("parse root");
            let key = generate_rsa_keypair(CertOptions::default().key_bits).expect("impostor key");
            let cert = params.self_signed(&key).expect("self-sign impostor");
            std::fs::write(&impostor_root, cert.pem()).expect("write impostor");

            Fixtures {
                root: p.join("root.pem"),
                root_key: p.join("root.key"),
                intermediate: p.join("intermediate.pem"),
                signer: p.join("signer.pem"),
                signer_key: p.join("signer.key"),
                impostor_root,
                _dir: dir,
            }
        })
    }

    /// A KDM start time one day out. The fixture chain is minted now, and
    /// libdcp refuses a KDM that starts on the day its signer certificate does,
    /// so a window starting "now" would be rejected.
    fn tomorrow() -> String {
        in_days(1)
    }

    /// An ST 430-1 timestamp that many days from now.
    fn in_days(days: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string()
    }

    // Signs with the self-signed root, so KeyInfo needs no chain and the
    // recipient stays the signer leaf (its key decrypts the CipherValue in the
    // cipher round-trip tests).
    fn test_config(f: &Fixtures, out: PathBuf) -> KdmConfig {
        KdmConfig {
            cpl_id: "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string(),
            content_title: "Test Feature".to_string(),
            annotation: None,
            recipient_cert_file: f.signer.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: out,
            valid_from: tomorrow(),
            valid_to: "7 days".to_string(),
            formulation: KdmFormulation::DciAny,
            content_keys: Vec::new(),
            format: KdmFormat::Smpte,
            device_cert_files: vec![],
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        }
    }

    /// A config for one formulation, with the device list that formulation
    /// requires: the two device-listing ones need certificates, the other two
    /// reject them.
    fn formulation_config(f: &Fixtures, formulation: KdmFormulation) -> KdmConfig {
        let mut config = test_config(f, PathBuf::from("unused"));
        config.formulation = formulation;
        config.device_cert_files = if formulation.lists_supplied_devices() {
            vec![f.signer.clone(), f.intermediate.clone()]
        } else {
            vec![]
        };
        config
    }

    // Realistic signer: the leaf signs, KeyInfo embeds leaf + intermediate +
    // root, and a verifier trusts the root. Recipient is the root cert (any
    // 2048-bit RSA cert works, its key is not needed here).
    fn chain_signed_config(f: &Fixtures, out: PathBuf) -> KdmConfig {
        KdmConfig {
            cpl_id: "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string(),
            content_title: "Test Feature".to_string(),
            annotation: None,
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.signer.clone(),
            signer_key_file: f.signer_key.clone(),
            signer_chain_files: vec![f.intermediate.clone(), f.root.clone()],
            output_file: out,
            valid_from: tomorrow(),
            valid_to: "7 days".to_string(),
            formulation: KdmFormulation::DciAny,
            content_keys: Vec::new(),
            format: KdmFormat::Smpte,
            device_cert_files: vec![],
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        }
    }

    fn xmlsec1_available() -> bool {
        std::process::Command::new("xmlsec1")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run `xmlsec1 --verify` against a signed KDM, returning whether it passed.
    fn xmlsec1_verify(kdm: &Path, trusted_pem: &Path) -> bool {
        std::process::Command::new("xmlsec1")
            .arg("--verify")
            .arg("--trusted-pem")
            .arg(trusted_pem)
            .args(["--id-attr:Id", "AuthenticatedPublic"])
            .args(["--id-attr:Id", "AuthenticatedPrivate"])
            .arg(kdm)
            .output()
            .expect("run xmlsec1")
            .status
            .success()
    }

    fn xmllint_available() -> bool {
        std::process::Command::new("xmllint")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Reference inclusive c14n via libxml2, the same engine xmlsec1 uses.
    fn xmllint_c14n(fragment: &str) -> Vec<u8> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("xmllint")
            .arg("--c14n")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn xmllint");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(fragment.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("run xmllint");
        assert!(
            out.status.success(),
            "xmllint --c14n failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// The pure-Rust c14n must equal libxml2 byte-for-byte for each fragment
    /// shape build_signature emits. Covers: the default namespace redefined on a
    /// descendant (KDMRequiredExtensions, EncryptedKey), the ds: prefix, an
    /// unused-but-declared default on ds:SignedInfo, self-closing tags expanded
    /// to explicit start+end, and &/</" escaping in element text (a DN and a
    /// title). If this drifts from libxml2 the signature stops verifying.
    #[test]
    fn c14n_matches_xmllint_for_each_fragment_shape() {
        // Windows xmllint writes stdout in text mode (LF -> CRLF), so a raw byte
        // comparison against its output is meaningless there. The xmlsec1 tests
        // prove byte-exact correctness on Windows instead.
        if cfg!(windows) {
            eprintln!("skipping on windows: xmllint stdout is crlf-translated");
            return;
        }
        if !xmllint_available() {
            eprintln!("skipping: xmllint not installed");
            return;
        }

        let public = format!(
            r#"<AuthenticatedPublic xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}" Id="{AUTH_PUBLIC_ID}">
    <MessageId>urn:uuid:11111111-2222-3333-4444-555555555555</MessageId>
    <AnnotationText>Acme &amp; Co &quot;Feature&quot; &lt;draft&gt; KDM</AnnotationText>
    <RequiredExtensions>
      <KDMRequiredExtensions xmlns="{KDM_NS}">
        <Recipient>
          <X509SubjectName>CN=A &amp; B,O=&quot;X&lt;Y&quot;,C=US</X509SubjectName>
        </Recipient>
        <KeyIdList>
          <TypedKeyId>
            <KeyType>MDIK</KeyType>
          </TypedKeyId>
        </KeyIdList>
      </KDMRequiredExtensions>
    </RequiredExtensions>
  </AuthenticatedPublic>"#
        );

        let private = format!(
            r#"<AuthenticatedPrivate xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}" Id="{AUTH_PRIVATE_ID}">
    <EncryptedKey xmlns="{ENC_NS}">
      <EncryptionMethod Algorithm="{KDM_ENCRYPTION_METHOD}"/>
      <CipherData>
        <CipherValue>YWJjZGVm</CipherValue>
      </CipherData>
    </EncryptedKey>
  </AuthenticatedPrivate>"#
        );

        let signed_info = format!(
            r##"<ds:SignedInfo xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}">
      <ds:CanonicalizationMethod Algorithm="{C14N_METHOD}"/>
      <ds:SignatureMethod Algorithm="{SIG_METHOD}"/>
      <ds:Reference URI="#{AUTH_PUBLIC_ID}">
        <ds:DigestMethod Algorithm="{DIGEST_METHOD}"/>
        <ds:DigestValue>3q2+7w==</ds:DigestValue>
      </ds:Reference>
    </ds:SignedInfo>"##
        );

        for (label, fragment) in [
            ("AuthenticatedPublic", &public),
            ("AuthenticatedPrivate", &private),
            ("ds:SignedInfo", &signed_info),
        ] {
            let ours = c14n(fragment).expect("pure-Rust c14n");
            let reference = xmllint_c14n(fragment);
            assert_eq!(
                ours,
                reference,
                "{label} c14n differs from xmllint\nours:      {}\nreference: {}",
                String::from_utf8_lossy(&ours),
                String::from_utf8_lossy(&reference)
            );
        }
    }

    fn cipher_value(xml: &str) -> Vec<u8> {
        let start = xml.find("<CipherValue>").expect("no CipherValue") + "<CipherValue>".len();
        let end = xml.find("</CipherValue>").expect("no closing CipherValue");
        base64::engine::general_purpose::STANDARD
            .decode(xml[start..end].trim())
            .expect("CipherValue is not base64")
    }

    fn recipient_private_key(f: &Fixtures) -> rsa::RsaPrivateKey {
        use rsa::pkcs8::DecodePrivateKey;
        let pem = std::fs::read_to_string(&f.signer_key).expect("read signer key");
        rsa::RsaPrivateKey::from_pkcs8_pem(&pem).expect("parse signer key")
    }

    #[test]
    fn cipher_value_is_not_the_plaintext_key() {
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build kdm");

        let ct = cipher_value(&kdm.xml);
        assert_eq!(ct.len(), 256, "2048-bit RSA must give a 256-byte block");
        assert_ne!(
            ct.as_slice(),
            kdm.content_key.as_slice(),
            "CipherValue is the raw content key"
        );

        // The old bug wrote the key as hex into the XML. Make sure neither the
        // hex nor the raw bytes appear anywhere in the message.
        let hex_key: String = kdm.content_key.iter().map(|b| format!("{b:02x}")).collect();
        assert!(
            !kdm.xml.contains(&hex_key),
            "content key leaked into the KDM as hex"
        );
        assert!(
            !ct.windows(16).any(|w| w == kdm.content_key),
            "content key appears verbatim inside the ciphertext"
        );
    }

    #[test]
    fn key_block_decrypts_to_the_original_key_and_matches_smpte_layout() {
        let f = fixtures();
        let config = test_config(f, PathBuf::from("unused"));
        let kdm = build_kdm(&config).expect("build kdm");

        let block = recipient_private_key(f)
            .decrypt(rsa::Oaep::new::<sha1::Sha1>(), &cipher_value(&kdm.xml))
            .expect("recipient private key must decrypt the CipherValue");

        // SMPTE ST 430-1 Table 6 offsets.
        assert_eq!(block.len(), KDM_KEY_BLOCK_LEN);
        assert_eq!(&block[0..16], &KDM_STRUCTURE_ID, "structure id");

        let signer = parse_signer(&f.root).expect("parse signer");
        assert_eq!(&block[16..36], &signer.thumbprint, "signer thumbprint");

        let cpl = uuid::Uuid::parse_str(&config.cpl_id).unwrap();
        assert_eq!(&block[36..52], cpl.as_bytes(), "cpl id");
        assert_eq!(&block[52..56], b"MDIK", "key type");
        assert_eq!(&block[56..72], kdm.key_id.as_bytes(), "key id");

        let not_before = std::str::from_utf8(&block[72..97]).expect("not-before ascii");
        let not_after = std::str::from_utf8(&block[97..122]).expect("not-after ascii");
        check_kdm_timestamp("not_before", not_before).expect("valid not-before");
        check_kdm_timestamp("not_after", not_after).expect("valid not-after");
        assert!(not_before < not_after);

        assert_eq!(&block[122..138], &kdm.content_key, "content key roundtrip");
    }

    #[test]
    fn interop_kdm_key_block_is_134_bytes_and_omits_key_type() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.format = KdmFormat::Interop;
        let kdm = build_kdm(&config).expect("build interop kdm");

        // digicine namespace and bare KeyId, no TypedKeyId/KeyType wrapper
        assert!(
            kdm.xml.contains(KDM_INTEROP_NS),
            "interop namespace missing"
        );
        assert!(
            !kdm.xml.contains("<TypedKeyId>"),
            "interop must not use TypedKeyId"
        );
        assert!(
            !kdm.xml.contains("<KeyType>"),
            "interop KeyIdList must omit KeyType"
        );
        assert!(
            kdm.xml
                .contains(&format!("<KeyId>urn:uuid:{}</KeyId>", kdm.key_id)),
            "interop KeyIdList must carry a bare KeyId"
        );

        let block = recipient_private_key(f)
            .decrypt(rsa::Oaep::new::<sha1::Sha1>(), &cipher_value(&kdm.xml))
            .expect("recipient private key must decrypt the interop CipherValue");

        // Interop 134-byte layout: SMPTE Table 6 minus the 4-byte KeyType, so the
        // key id follows the CPL id directly (libdcp decrypted_kdm.cc 134 case).
        assert_eq!(block.len(), KDM_KEY_BLOCK_LEN_INTEROP);
        assert_eq!(&block[0..16], &KDM_STRUCTURE_ID, "structure id");

        let signer = parse_signer(&f.root).expect("parse signer");
        assert_eq!(&block[16..36], &signer.thumbprint, "signer thumbprint");

        let cpl = uuid::Uuid::parse_str(&config.cpl_id).unwrap();
        assert_eq!(&block[36..52], cpl.as_bytes(), "cpl id");
        assert_eq!(
            &block[52..68],
            kdm.key_id.as_bytes(),
            "key id (no key type)"
        );

        let not_before = std::str::from_utf8(&block[68..93]).expect("not-before ascii");
        let not_after = std::str::from_utf8(&block[93..118]).expect("not-after ascii");
        check_kdm_timestamp("not_before", not_before).expect("valid not-before");
        check_kdm_timestamp("not_after", not_after).expect("valid not-after");
        assert!(not_before < not_after);

        assert_eq!(&block[118..134], &kdm.content_key, "content key roundtrip");
    }

    /// A default (SMPTE) KDM must be byte-identical to before the format field
    /// existed: it still uses the SMPTE namespace and TypedKeyId.
    #[test]
    fn smpte_is_the_default_and_unchanged() {
        assert_eq!(KdmFormat::default(), KdmFormat::Smpte);
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build");
        assert!(
            kdm.xml.contains(KDM_NS),
            "default must use the SMPTE namespace"
        );
        assert!(
            kdm.xml.contains("<TypedKeyId>"),
            "default must use TypedKeyId"
        );
        assert!(!kdm.xml.contains(KDM_INTEROP_NS));
    }

    #[test]
    fn interop_kdm_signature_verifies_with_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("interop.kdm.xml");
        let mut config = chain_signed_config(f, out.clone());
        config.format = KdmFormat::Interop;
        generate_kdm(&config).expect("generate interop kdm");
        assert!(
            xmlsec1_verify(&out, &f.root),
            "interop KDM signature must verify"
        );
    }

    #[test]
    fn each_kdm_gets_a_fresh_content_key() {
        let f = fixtures();
        let a = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("a");
        let b = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("b");
        assert_ne!(a.content_key, b.content_key, "content key is not random");
    }

    #[test]
    fn missing_recipient_cert_errors() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.recipient_cert_file = PathBuf::from("/nonexistent/recipient.pem");
        let err = build_kdm(&config).expect_err("must not build without a recipient cert");
        assert!(err.contains("cannot read recipient cert"), "got: {err}");
    }

    #[test]
    fn signer_thumbprint_is_sha1_over_tbs_not_full_cert() {
        use sha1::Digest;
        use x509_parser::prelude::*;

        let f = fixtures();
        let signer = parse_signer(&f.root).expect("parse signer");

        let data = std::fs::read(&f.root).unwrap();
        let (_, pem) = parse_x509_pem(&data).unwrap();
        let cert = pem.parse_x509().unwrap();

        let over_tbs: [u8; 20] = sha1::Sha1::digest(cert.tbs_certificate.as_ref()).into();
        let over_full: [u8; 20] = sha1::Sha1::digest(&pem.contents).into();

        assert_eq!(signer.thumbprint, over_tbs, "thumbprint must be over TBS");
        assert_ne!(
            over_tbs, over_full,
            "TBS and full-cert hashes must differ, else the test proves nothing"
        );
    }

    /// ST 430-2 wants Basic Constraints and Key Usage on every certificate in the
    /// chain, the leaf included. rcgen writes no extensions at all for
    /// `IsCa::NoCa`, so the leaf came out bare and validators rejected it.
    #[test]
    fn every_generated_certificate_carries_basic_constraints_and_key_usage() {
        use x509_parser::prelude::*;

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(generate_chain("Acme", dir.path()), 0, "chain generation");

        for name in ["root.pem", "intermediate.pem", "signer.pem"] {
            let data = std::fs::read(dir.path().join(name)).unwrap();
            let (_, pem) = parse_x509_pem(&data).unwrap();
            let cert = pem.parse_x509().unwrap();

            let basic = cert
                .basic_constraints()
                .unwrap_or_else(|e| panic!("{name} basic constraints: {e}"))
                .unwrap_or_else(|| panic!("{name} has no Basic Constraints extension"));
            let usage = cert
                .key_usage()
                .unwrap_or_else(|e| panic!("{name} key usage: {e}"))
                .unwrap_or_else(|| panic!("{name} has no Key Usage extension"));

            if name == "signer.pem" {
                assert!(!basic.value.ca, "the leaf must say CA:FALSE");
                assert!(
                    usage.value.digital_signature(),
                    "the leaf signs documents, so it needs digitalSignature"
                );
            } else {
                assert!(basic.value.ca, "{name} must say CA:TRUE");
                assert!(usage.value.key_cert_sign(), "{name} must sign certificates");
            }
        }
    }

    /// The dnQualifier already in each DCP-o-matic certificate must come back
    /// out of postkit's own hashing of that same certificate's public key.
    /// Nothing here is a transcribed value.
    #[test]
    fn postkit_computes_the_dn_qualifier_dcp_o_matic_wrote() {
        use x509_parser::prelude::*;

        for name in ["root", "intermediate", "leaf"] {
            let data = std::fs::read(dcp_o_matic_cert(name)).unwrap();
            let (_, pem) = parse_x509_pem(&data).unwrap();
            let cert = pem.parse_x509().unwrap();

            let computed = public_key_digest_base64(cert.public_key().raw).expect("digest");
            assert_eq!(
                subject_dn_qualifier(&cert),
                Some(computed),
                "{name}: the dnQualifier is the base64 SHA-1 of the public key"
            );
        }
    }

    /// The dnQualifier value from a certificate's subject, if it carries one.
    fn subject_dn_qualifier(cert: &x509_parser::certificate::X509Certificate) -> Option<String> {
        let oid = x509_parser::der_parser::Oid::from(&DN_QUALIFIER_OID).ok()?;
        cert.subject()
            .iter_attributes()
            .find(|attr| *attr.attr_type() == oid)
            .and_then(|attr| attr.as_str().ok())
            .map(str::to_string)
    }

    /// The ASN.1 tag of every value in a distinguished name, in order.
    fn dn_value_tags(
        cert: &x509_parser::certificate::X509Certificate,
    ) -> Vec<x509_parser::der_parser::asn1_rs::Tag> {
        cert.subject()
            .iter_attributes()
            .map(|attr| attr.attr_value().tag())
            .collect()
    }

    /// Every tier postkit generates has to have the shape the DCP-o-matic
    /// certificates have: the same ASN.1 string type throughout the DN, a
    /// dnQualifier holding the public key digest, the same basicConstraints and
    /// keyUsage, and both key identifiers.
    #[test]
    fn generated_certificates_have_the_structure_the_dcp_o_matic_fixtures_have() {
        use x509_parser::prelude::*;

        let f = fixtures();
        for (generated, reference) in [
            (&f.root, "root"),
            (&f.intermediate, "intermediate"),
            (&f.signer, "leaf"),
        ] {
            let ours_data = std::fs::read(generated).unwrap();
            let (_, ours_pem) = parse_x509_pem(&ours_data).unwrap();
            let ours = ours_pem.parse_x509().unwrap();

            let theirs_data = std::fs::read(dcp_o_matic_cert(reference)).unwrap();
            let (_, theirs_pem) = parse_x509_pem(&theirs_data).unwrap();
            let theirs = theirs_pem.parse_x509().unwrap();

            let reference_tag = dn_value_tags(&theirs)[0];
            for tag in dn_value_tags(&ours) {
                assert_eq!(
                    tag, reference_tag,
                    "{reference}: every DN value must use the ASN.1 string type \
                     DCP-o-matic uses"
                );
            }

            assert_eq!(
                subject_dn_qualifier(&ours),
                Some(public_key_digest_base64(ours.public_key().raw).expect("digest")),
                "{reference}: dnQualifier must hold this certificate's own key digest"
            );

            let ours_basic = ours.basic_constraints().unwrap().unwrap().value;
            let theirs_basic = theirs.basic_constraints().unwrap().unwrap().value;
            assert_eq!(ours_basic.ca, theirs_basic.ca, "{reference}: CA flag");
            assert_eq!(
                ours_basic.path_len_constraint, theirs_basic.path_len_constraint,
                "{reference}: basicConstraints path length"
            );

            let ours_usage = ours.key_usage().unwrap().unwrap().value;
            let theirs_usage = theirs.key_usage().unwrap().unwrap().value;
            assert_eq!(
                ours_usage.flags, theirs_usage.flags,
                "{reference}: keyUsage bits"
            );

            let ski = ours
                .get_extension_unique(
                    &x509_parser::oid_registry::OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
                )
                .unwrap()
                .unwrap_or_else(|| panic!("{reference} has no subjectKeyIdentifier"));
            let ParsedExtension::SubjectKeyIdentifier(ski) = ski.parsed_extension() else {
                panic!("{reference} subjectKeyIdentifier did not parse");
            };
            assert_eq!(
                ski.0,
                public_key_digest(ours.public_key().raw).expect("digest"),
                "{reference}: the subjectKeyIdentifier is the same digest as the dnQualifier"
            );
            assert!(
                ours.get_extension_unique(
                    &x509_parser::oid_registry::OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER
                )
                .unwrap()
                .is_some(),
                "{reference} has no authorityKeyIdentifier"
            );
        }
    }

    /// ST 430-2 5.3.1 puts a role token before the first '.' of a CommonName,
    /// and a signer's has to differ from its CAs'. DCP-o-matic's certificates
    /// are the reference for which tokens those are.
    #[test]
    fn generated_common_names_carry_the_role_token_dcp_o_matic_uses() {
        let f = fixtures();
        for (generated, reference) in [
            (&f.root, "root"),
            (&f.intermediate, "intermediate"),
            (&f.signer, "leaf"),
        ] {
            assert_eq!(
                cn_role(&read_certificate(generated).subject_cn),
                cn_role(&read_certificate(&dcp_o_matic_cert(reference)).subject_cn),
                "{reference}: CommonName role token"
            );
        }
        assert_ne!(
            cn_role(&read_certificate(&f.signer).subject_cn),
            cn_role(&read_certificate(&f.root).subject_cn),
            "the signer's role must be distinct from its CAs'"
        );
    }

    /// A ST 430-2 CommonName's role token: everything before the first '.'.
    fn cn_role(common_name: &str) -> &str {
        common_name.split('.').next().unwrap_or("")
    }

    /// A fresh self-signed certificate valid for `validity_days` from now, so a
    /// KDM window can be placed inside, across or beyond its validity.
    fn short_lived_certificate(dir: &Path, name: &str, validity_days: u32) -> (PathBuf, PathBuf) {
        let cert = dir.join(format!("{name}.{CERTIFICATE_EXTENSION}"));
        let key = dir.join(format!("{name}.key"));
        let opts = CertOptions {
            cert_type: CertType::Root,
            common_name: common_name(CN_ROLE_CERTIFICATE_AUTHORITY, name, CN_TIER_ROOT),
            organization: name.to_string(),
            validity_days,
            output_cert: cert.clone(),
            output_key: key.clone(),
            ..Default::default()
        };
        assert_eq!(generate_certificate(&opts), 0, "{name} generation");
        (cert, key)
    }

    #[test]
    fn a_window_inside_the_recipient_certificate_is_within() {
        let f = fixtures();
        assert_eq!(
            classify_kdm_window(&f.signer, &tomorrow(), "7 days").expect("classify"),
            KdmWindowOverlap::WithinCertificate
        );
    }

    #[test]
    fn a_window_straddling_the_recipient_certificate_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = short_lived_certificate(dir.path(), "straddled", 3);
        assert_eq!(
            classify_kdm_window(&cert, &tomorrow(), "7 days").expect("classify"),
            KdmWindowOverlap::OverlapsCertificate,
            "a window that outlives the certificate covers only part of itself"
        );
    }

    #[test]
    fn a_window_outside_the_recipient_certificate_is_outside_and_will_not_build() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = short_lived_certificate(dir.path(), "expired-first", 1);

        assert_eq!(
            classify_kdm_window(&cert, &in_days(10), "7 days").expect("classify"),
            KdmWindowOverlap::OutsideCertificate
        );

        let mut config = test_config(f, PathBuf::from("unused"));
        config.recipient_cert_file = cert;
        config.valid_from = in_days(10);
        let err = build_kdm(&config).expect_err("a KDM that can never open must not be built");
        assert!(err.contains("could never open"), "got: {err}");
    }

    /// libdcp refuses to sign a KDM the signer chain does not outlive.
    #[test]
    fn a_signer_chain_that_expires_mid_window_is_rejected() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = short_lived_certificate(dir.path(), "short-signer", 3);

        let mut config = test_config(f, PathBuf::from("unused"));
        config.signer_cert_file = cert;
        config.signer_key_file = key;
        let err = build_kdm(&config).expect_err("a signer that expires mid-window must be refused");
        assert!(
            err.contains("the signer chain cannot issue this KDM") && err.contains("expires on"),
            "got: {err}"
        );
    }

    /// The same day-granularity strictness libdcp has: a signer minted today
    /// cannot issue a KDM that starts today.
    #[test]
    fn a_window_starting_the_day_the_signer_does_is_rejected() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.valid_from = "now".to_string();
        let err = build_kdm(&config).expect_err("a same-day start must be refused");
        assert!(err.contains("starts on"), "got: {err}");
    }

    #[test]
    fn invalid_recipient_cert_errors() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("bogus.pem");
        std::fs::write(&bogus, b"not a certificate at all").unwrap();

        let mut config = test_config(f, PathBuf::from("unused"));
        config.recipient_cert_file = bogus;
        let err = build_kdm(&config).expect_err("must not build from a non-certificate");
        assert!(err.contains("not valid PEM"), "got: {err}");
    }

    #[test]
    fn missing_signer_cert_errors() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.signer_cert_file = PathBuf::new();
        let err = build_kdm(&config).expect_err("must not build without a signer cert");
        assert!(err.contains("signer certificate is required"), "got: {err}");
    }

    #[test]
    fn non_uuid_cpl_id_errors() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.cpl_id = "not-a-uuid".to_string();
        let err = build_kdm(&config).expect_err("must reject a non-UUID CPL id");
        assert!(err.contains("is not a UUID"), "got: {err}");
    }

    #[test]
    fn undersized_rsa_key_is_rejected() {
        // DCI mandates 2048-bit RSA; a smaller key must not produce a KDM.
        let weak = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).expect("gen 1024");
        let err = encrypt_key_block(&weak.to_public_key(), &[0u8; KDM_KEY_BLOCK_LEN])
            .expect_err("1024-bit key must be rejected");
        assert!(err.contains("1024"), "got: {err}");
    }

    #[test]
    fn malformed_timestamps_are_rejected() {
        // Wrong length, 'Z' offset and fractional seconds all break the fixed
        // 25-byte key block fields.
        assert!(check_kdm_timestamp("t", "2024-01-01T00:00:00Z").is_err());
        assert!(check_kdm_timestamp("t", "2024-01-01T00:00:00.5+00:00").is_err());
        assert!(check_kdm_timestamp("t", "2024-01-01").is_err());
        check_kdm_timestamp("t", "2004-05-01T13:20:00+00:00").expect("spec example is valid");
    }

    #[test]
    fn content_title_cannot_inject_xml() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.content_title = "</ContentTitleText><Evil>x</Evil>".to_string();
        let kdm = build_kdm(&config).expect("build kdm");
        assert!(
            !kdm.xml.contains("<Evil>"),
            "content title injected raw XML"
        );
        assert!(kdm.xml.contains("&lt;/ContentTitleText&gt;"));
    }

    #[test]
    fn generate_kdm_writes_a_file_with_the_required_algorithm() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested").join("test.kdm.xml");
        generate_kdm(&test_config(f, out.clone())).expect("generate kdm");

        let xml = std::fs::read_to_string(&out).expect("kdm written");
        assert!(
            xml.contains(&format!("Algorithm=\"{KDM_ENCRYPTION_METHOD}\"")),
            "missing the rsa-oaep-mgf1p algorithm URI required by DCI CTP 3.4.12"
        );
    }

    #[test]
    fn message_type_is_the_standard_uri_for_every_formulation() {
        // SMPTE ST 430-1: MessageType is a fixed URI; formulation must not
        // change it. Regression guard against emitting #kdm-key-type-dci-any etc.
        let f = fixtures();
        for formulation in KdmFormulation::ALL {
            let cfg = formulation_config(f, formulation);
            let kdm = build_kdm(&cfg).expect("build kdm");
            assert!(
                kdm.xml
                    .contains(&format!("<MessageType>{KDM_MESSAGE_TYPE}</MessageType>")),
                "formulation {formulation} must still emit the standard MessageType"
            );
        }
    }

    #[test]
    fn kdm_signature_verifies_with_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("signed.kdm.xml");
        generate_kdm(&chain_signed_config(f, out.clone())).expect("generate signed kdm");

        assert!(
            xmlsec1_verify(&out, &f.root),
            "xmlsec1 must verify the signed KDM against the trusted root"
        );
    }

    /// ContentAuthenticator sits inside the signed AuthenticatedPublic, so the
    /// signature has to still verify with it there.
    #[test]
    fn a_kdm_carrying_a_content_authenticator_verifies_with_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        for formulation in [KdmFormulation::DciAny, KdmFormulation::DciSpecific] {
            let out = dir.path().join(format!("{formulation}.kdm.xml"));
            let mut config = chain_signed_config(f, out.clone());
            config.formulation = formulation;
            config.device_cert_files = if formulation.lists_supplied_devices() {
                vec![f.intermediate.clone()]
            } else {
                vec![]
            };
            generate_kdm(&config).expect("generate signed kdm");

            let xml = std::fs::read_to_string(&out).expect("kdm written");
            assert_eq!(
                content_authenticators_in(&xml),
                vec![expected_thumbprint(&config.signer_cert_file)],
                "{formulation} must authenticate with the signer leaf's thumbprint"
            );
            assert!(
                xmlsec1_verify(&out, &f.root),
                "{formulation} must still verify against the trusted root"
            );
        }
    }

    #[test]
    fn tampered_authenticated_public_fails_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("signed.kdm.xml");
        generate_kdm(&chain_signed_config(f, out.clone())).expect("generate signed kdm");

        // Flip one byte inside AuthenticatedPublic: the MDIK key type.
        let xml = std::fs::read_to_string(&out).unwrap();
        let tampered = xml.replacen("<KeyType>MDIK</KeyType>", "<KeyType>MDAK</KeyType>", 1);
        assert_ne!(xml, tampered, "tamper must actually change the file");
        std::fs::write(&out, tampered).unwrap();

        assert!(
            !xmlsec1_verify(&out, &f.root),
            "xmlsec1 must reject a KDM whose AuthenticatedPublic was altered"
        );
    }

    #[test]
    fn self_signed_signer_verifies_with_xmlsec1() {
        // The default test_config signs with the self-signed root.
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("signed.kdm.xml");
        generate_kdm(&test_config(f, out.clone())).expect("generate signed kdm");
        assert!(xmlsec1_verify(&out, &f.root), "self-signed KDM must verify");
    }

    #[test]
    fn signed_kdm_has_a_real_signature_block() {
        let f = fixtures();
        let kdm = build_kdm(&chain_signed_config(f, PathBuf::from("unused"))).expect("build");
        assert!(kdm.xml.contains("<ds:Signature>"), "no ds:Signature");
        assert!(
            kdm.xml.contains(&format!("Algorithm=\"{SIG_METHOD}\"")),
            "missing rsa-sha256 SignatureMethod"
        );
        assert!(
            kdm.xml.contains(&format!("Algorithm=\"{C14N_METHOD}\"")),
            "missing inclusive-with-comments c14n method"
        );
        // Full chain embedded: leaf + intermediate + root.
        assert_eq!(
            kdm.xml.matches("<ds:X509Certificate>").count(),
            3,
            "KeyInfo must embed the full signer chain"
        );
        assert!(
            !kdm.xml.contains("<ds:SignatureValue></ds:SignatureValue>"),
            "SignatureValue must not be empty"
        );
    }

    #[test]
    fn missing_signer_key_errors() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.signer_key_file = PathBuf::new();
        let err = build_kdm(&config).expect_err("must not build without a signer key");
        assert!(err.contains("signer private key is required"), "got: {err}");
    }

    #[test]
    fn signer_key_not_matching_cert_errors() {
        // Sign with the root's key but claim the leaf as the signer cert.
        let f = fixtures();
        let mut config = chain_signed_config(f, PathBuf::from("unused"));
        config.signer_key_file = f.root_key.clone();
        let err = build_kdm(&config).expect_err("must reject a mismatched key");
        assert!(err.contains("does not match"), "got: {err}");
    }

    #[test]
    fn valid_chain_passes() {
        let f = fixtures();
        let chain = vec![f.signer.clone(), f.intermediate.clone(), f.root.clone()];
        assert_eq!(validate_chain(&chain), 0, "genuine chain must validate");
    }

    #[test]
    fn chain_with_impostor_root_is_rejected() {
        // The impostor's subject is byte-identical to the real root's, so the
        // name comparison passes it. Signature verification must reject it.
        let f = fixtures();
        let chain = vec![
            f.signer.clone(),
            f.intermediate.clone(),
            f.impostor_root.clone(),
        ];
        let err = validate_chain_inner(&chain, None)
            .expect_err("a root that did not sign the intermediate must be rejected");
        assert!(
            err.contains("signature verification failed"),
            "the impostor must fail the signature check, not a name comparison: {err}"
        );
    }

    #[test]
    fn out_of_order_chain_is_rejected() {
        let f = fixtures();
        let chain = vec![f.root.clone(), f.intermediate.clone(), f.signer.clone()];
        assert_eq!(
            validate_chain(&chain),
            -1,
            "reversed chain must be rejected"
        );
    }

    #[test]
    fn empty_chain_is_rejected() {
        assert_eq!(validate_chain(&[]), -1);
    }

    /// Build a signed multi-key KDM to `recipient_cert`, signed by the root, to
    /// stand in as a DKDM in re-wrap tests. Content keys are caller-chosen so a
    /// round-trip can assert on exact bytes.
    fn build_stand_in_dkdm(f: &Fixtures, recipient_cert: &Path, keys: &[KdmKey]) -> String {
        let recipient = parse_recipient(recipient_cert).expect("recipient");
        let signer = parse_signer(&f.root).expect("signer");
        let config = KdmConfig {
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            ..Default::default()
        };
        let cpl = uuid::Uuid::parse_str("8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9").unwrap();
        build_kdm_xml(
            &config,
            &cpl,
            "Multi Key Feature",
            "http://www.smpte-ra.org/430-1/2006/KDM#kdm-key-type-dci-any",
            &tomorrow(),
            &in_days(30),
            &recipient,
            &signer,
            keys,
        )
        .expect("build stand-in dkdm")
    }

    fn load_private_key(path: &Path) -> rsa::RsaPrivateKey {
        use rsa::pkcs8::DecodePrivateKey;
        rsa::RsaPrivateKey::from_pkcs8_pem(&std::fs::read_to_string(path).unwrap())
            .expect("parse private key")
    }

    #[test]
    fn rewrap_roundtrip_recovers_multiple_content_keys() {
        let f = fixtures();

        // Stand-in DKDM addressed to recipient A (the signer leaf, whose key is
        // signer_key). Two keys so the N-key path is exercised.
        let mdik_id = uuid::Uuid::new_v4();
        let mdak_id = uuid::Uuid::new_v4();
        let mdik_key = [0x11u8; 16];
        let mdak_key = [0x22u8; 16];
        let src_keys = vec![
            KdmKey {
                key_type: *b"MDIK",
                key_id: mdik_id,
                content_key: mdik_key,
            },
            KdmKey {
                key_type: *b"MDAK",
                key_id: mdak_id,
                content_key: mdak_key,
            },
        ];
        let dkdm_xml = build_stand_in_dkdm(f, &f.signer, &src_keys);

        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(&dkdm_path, &dkdm_xml).unwrap();
        let out = dir.path().join("rewrapped.kdm.xml");

        // Re-wrap to recipient B (the root cert, whose key is root_key).
        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.signer_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: out.clone(),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![],
            formulation: KdmFormulation::default(),
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        rewrap_dkdm_to_file(&config).expect("rewrap");

        // Decrypt B's CipherValues and confirm the content keys survived.
        let xml = std::fs::read_to_string(&out).unwrap();
        let b_key = load_private_key(&f.root_key);
        let cvs = parse_kdm_xml(&xml).expect("parse rewrapped").cipher_values;
        assert_eq!(cvs.len(), 2, "both keys must be re-wrapped");

        let mut recovered = std::collections::HashMap::new();
        for ct in cvs {
            let block = b_key
                .decrypt(rsa::Oaep::new::<sha1::Sha1>(), &ct)
                .expect("recipient B must decrypt the re-wrapped key");
            let rk = parse_kdm_key_block(&block, KdmFormat::Smpte).expect("valid key block");
            recovered.insert(rk.key_id, (rk.key_type, rk.content_key));
        }
        assert_eq!(
            recovered.get(&mdik_id),
            Some(&(Some(*b"MDIK"), mdik_key)),
            "MDIK key id/type/value must round-trip"
        );
        assert_eq!(
            recovered.get(&mdak_id),
            Some(&(Some(*b"MDAK"), mdak_key)),
            "MDAK key id/type/value must round-trip"
        );
    }

    #[test]
    fn rewrapped_cipher_differs_from_source() {
        // The whole point of re-wrap: the new CipherValue is not the source's.
        let f = fixtures();
        let src_keys = vec![KdmKey {
            key_type: *b"MDIK",
            key_id: uuid::Uuid::new_v4(),
            content_key: [0x33u8; 16],
        }];
        let dkdm_xml = build_stand_in_dkdm(f, &f.signer, &src_keys);
        let src_ct = parse_kdm_xml(&dkdm_xml).unwrap().cipher_values.remove(0);

        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(&dkdm_path, &dkdm_xml).unwrap();
        let out = dir.path().join("rewrapped.kdm.xml");
        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.signer_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: out.clone(),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![],
            formulation: KdmFormulation::default(),
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        rewrap_dkdm_to_file(&config).expect("rewrap");
        let new_ct = parse_kdm_xml(&std::fs::read_to_string(&out).unwrap())
            .unwrap()
            .cipher_values
            .remove(0);
        assert_ne!(src_ct, new_ct, "re-wrap must re-encrypt, not copy");
    }

    #[test]
    fn rewrap_with_wrong_dkdm_key_errors() {
        let f = fixtures();
        let src_keys = vec![KdmKey {
            key_type: *b"MDIK",
            key_id: uuid::Uuid::new_v4(),
            content_key: [0x44u8; 16],
        }];
        // DKDM addressed to A (signer leaf).
        let dkdm_xml = build_stand_in_dkdm(f, &f.signer, &src_keys);
        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(&dkdm_path, &dkdm_xml).unwrap();

        // Attempt to decrypt with B's key: must fail, not silently mis-wrap.
        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.root_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: dir.path().join("out.xml"),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![],
            formulation: KdmFormulation::default(),
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        let err = rewrap_dkdm(&config).expect_err("wrong recipient key must fail");
        assert!(
            err.contains("wrong recipient key") || err.contains("decryption"),
            "got: {err}"
        );
    }

    #[test]
    fn rewrapped_kdm_verifies_with_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let f = fixtures();
        let src_keys = vec![
            KdmKey {
                key_type: *b"MDIK",
                key_id: uuid::Uuid::new_v4(),
                content_key: [0x55u8; 16],
            },
            KdmKey {
                key_type: *b"MDAK",
                key_id: uuid::Uuid::new_v4(),
                content_key: [0x66u8; 16],
            },
        ];
        // DKDM to A (signer leaf).
        let dkdm_xml = build_stand_in_dkdm(f, &f.signer, &src_keys);
        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(&dkdm_path, &dkdm_xml).unwrap();
        let out = dir.path().join("rewrapped.kdm.xml");

        // Re-issue signed by the leaf chain, recipient B = root.
        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.signer_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.signer.clone(),
            signer_key_file: f.signer_key.clone(),
            signer_chain_files: vec![f.intermediate.clone(), f.root.clone()],
            output_file: out.clone(),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![],
            formulation: KdmFormulation::default(),
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        rewrap_dkdm_to_file(&config).expect("rewrap");
        assert!(
            xmlsec1_verify(&out, &f.root),
            "xmlsec1 must verify the re-wrapped KDM against the trusted root"
        );
    }

    #[test]
    fn read_certificate_reports_the_real_key_size() {
        let f = fixtures();
        let info = read_certificate(&f.root);
        assert_eq!(
            info.key_bits, 2048,
            "key size must be in bits, not bits * 8"
        );
        assert!(info.is_ca);
        assert!(!info.is_expired);
    }

    // Build a KDM to the signer-leaf recipient carrying caller-chosen keys, then
    // unwrap it with that recipient's private key. Non-vacuous: the keys are
    // fixed bytes so the round-trip asserts on exact values, not just success.
    #[test]
    fn unwrap_recovers_every_wrapped_key_smpte() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        let mdik_id = uuid::Uuid::new_v4();
        let mdak_id = uuid::Uuid::new_v4();
        let mdik_key = [0xA1u8; 16];
        let mdak_key = [0xB2u8; 16];
        config.content_keys = vec![
            KdmContentKey {
                key_type: *b"MDIK",
                key_id: mdik_id,
                content_key: mdik_key,
            },
            KdmContentKey {
                key_type: *b"MDAK",
                key_id: mdak_id,
                content_key: mdak_key,
            },
        ];
        let kdm = build_kdm(&config).expect("build kdm");

        let unwrapped = unwrap_kdm(&kdm.xml, &f.signer_key).expect("unwrap");
        assert_eq!(unwrapped.format, KdmFormat::Smpte);
        assert_eq!(unwrapped.keys.len(), 2, "both wrapped keys must come back");
        assert_eq!(
            unwrapped.cpl_id,
            uuid::Uuid::parse_str(&config.cpl_id).unwrap()
        );
        assert_eq!(unwrapped.content_key(&mdik_id), Some(&mdik_key));
        assert_eq!(unwrapped.content_key(&mdak_id), Some(&mdak_key));

        let mdik = unwrapped.keys.iter().find(|k| k.key_id == mdik_id).unwrap();
        assert_eq!(mdik.key_type, Some(*b"MDIK"), "SMPTE key type preserved");
        assert_eq!(mdik.cpl_id, unwrapped.cpl_id);
        assert!(mdik.not_valid_before < mdik.not_valid_after);
    }

    // Interop blocks are 134 bytes and carry no key type.
    #[test]
    fn unwrap_recovers_wrapped_key_interop() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.format = KdmFormat::Interop;
        let key_id = uuid::Uuid::new_v4();
        let content = [0xC3u8; 16];
        config.content_keys = vec![KdmContentKey {
            key_type: *b"MDIK",
            key_id,
            content_key: content,
        }];
        let kdm = build_kdm(&config).expect("build interop kdm");

        let unwrapped = unwrap_kdm(&kdm.xml, &f.signer_key).expect("unwrap interop");
        assert_eq!(unwrapped.format, KdmFormat::Interop);
        assert_eq!(unwrapped.content_key(&key_id), Some(&content));
        assert_eq!(
            unwrapped.keys[0].key_type, None,
            "interop key block carries no key type"
        );
    }

    // The wrong recipient key must fail the OAEP unpad (or the structure-id
    // check), never return a plausible-looking but wrong key.
    #[test]
    fn unwrap_with_wrong_recipient_key_fails_loud() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        let key_id = uuid::Uuid::new_v4();
        config.content_keys = vec![KdmContentKey {
            key_type: *b"MDIK",
            key_id,
            content_key: [0xD4u8; 16],
        }];
        let kdm = build_kdm(&config).expect("build kdm");

        // root_key is not the recipient (signer leaf) key.
        let err = unwrap_kdm(&kdm.xml, &f.root_key).expect_err("wrong key must fail");
        assert!(
            err.contains("wrong recipient key")
                || err.contains("decryption")
                || err.contains("structure id"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_kdm_reads_public_metadata_without_a_key() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        let key_id = uuid::Uuid::new_v4();
        config.content_keys = vec![KdmContentKey {
            key_type: *b"MDIK",
            key_id,
            content_key: [0u8; 16],
        }];
        let kdm = build_kdm(&config).expect("build");

        let meta = parse_kdm(&kdm.xml).expect("parse metadata");
        assert_eq!(meta.format, KdmFormat::Smpte);
        assert_eq!(meta.cpl_id, uuid::Uuid::parse_str(&config.cpl_id).unwrap());
        assert_eq!(meta.content_title, "Test Feature");
        assert_eq!(meta.key_ids.len(), 1);
        assert_eq!(meta.key_ids[0].key_id, key_id);
        assert_eq!(meta.key_ids[0].key_type, Some(*b"MDIK"));
        assert!(meta.not_valid_before < meta.not_valid_after);
    }

    #[test]
    fn annotation_override_replaces_derived_text() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));

        // None: byte-identical to before, derived "<title> KDM for <recipient>".
        let default_kdm = build_kdm(&config).expect("build default");
        let default_meta = parse_kdm(&default_kdm.xml).expect("parse default");
        assert!(
            default_meta
                .annotation_text
                .starts_with("Test Feature KDM for "),
            "got: {}",
            default_meta.annotation_text
        );

        // Some: the exact override, escaped in the XML and read back verbatim.
        config.annotation = Some("Release KDM <v2> & final".to_string());
        let kdm = build_kdm(&config).expect("build annotated");
        assert!(
            kdm.xml
                .contains("<AnnotationText>Release KDM &lt;v2&gt; &amp; final</AnnotationText>"),
            "annotation must be escaped in the KDM XML"
        );
        let meta = parse_kdm(&kdm.xml).expect("parse annotated");
        assert_eq!(meta.annotation_text, "Release KDM <v2> & final");
    }

    // Key hygiene: the content key must never surface through Debug.
    #[test]
    fn unwrapped_key_debug_redacts_the_content_key() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        let key_id = uuid::Uuid::new_v4();
        let content = [0x7Eu8; 16];
        config.content_keys = vec![KdmContentKey {
            key_type: *b"MDIK",
            key_id,
            content_key: content,
        }];
        let kdm = build_kdm(&config).expect("build");
        let unwrapped = unwrap_kdm(&kdm.xml, &f.signer_key).expect("unwrap");

        let dump = format!("{unwrapped:?}");
        assert!(dump.contains("<redacted>"), "content key not redacted");
        let hex: String = content.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!dump.contains(&hex), "content key leaked into Debug");
    }

    /// The KDMRequiredExtensions element on its own, as a standalone document
    /// the ST 430-1 schema can be pointed at directly.
    fn required_extensions_fragment(kdm_xml: &str) -> String {
        const END_TAG: &str = "</KDMRequiredExtensions>";
        let start = kdm_xml
            .find("<KDMRequiredExtensions")
            .expect("KDMRequiredExtensions start");
        let end = kdm_xml.find(END_TAG).expect("KDMRequiredExtensions end") + END_TAG.len();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{}\n",
            &kdm_xml[start..end]
        )
    }

    /// Text of every occurrence of one element, in document order.
    fn elements_in(xml: &str, name: &str) -> Vec<String> {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        xml.match_indices(&open)
            .map(|(at, tag)| {
                let rest = &xml[at + tag.len()..];
                let end = rest
                    .find(&close)
                    .unwrap_or_else(|| panic!("unterminated {name}"));
                rest[..end].to_string()
            })
            .collect()
    }

    fn thumbprints_in(kdm_xml: &str) -> Vec<String> {
        elements_in(kdm_xml, CERTIFICATE_THUMBPRINT_ELEMENT)
    }

    fn content_authenticators_in(kdm_xml: &str) -> Vec<String> {
        elements_in(kdm_xml, CONTENT_AUTHENTICATOR_ELEMENT)
    }

    #[test]
    fn assume_trust_thumbprint_is_the_base64_sha1_of_the_empty_string() {
        use sha1::Digest;
        let digest: [u8; 20] = sha1::Sha1::digest(b"").into();
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(digest),
            ASSUME_TRUST_THUMBPRINT
        );
    }

    #[test]
    fn the_certificate_thumbprint_covers_the_der_header_as_libdcp_does() {
        use sha1::Digest;
        use x509_parser::prelude::*;

        let f = fixtures();
        let data = std::fs::read(&f.root).unwrap();
        let (_, pem) = parse_x509_pem(&data).unwrap();
        let cert = pem.parse_x509().unwrap();
        let tbs = cert.tbs_certificate.as_ref();

        // ST 430-2 5.4 says to exclude the DER tag and length; libdcp includes
        // them and deployed gear agrees with libdcp, so the slice hashed here
        // must be the complete TBSCertificate encoding.
        assert_eq!(tbs[0], 0x30, "hashed slice must start at the SEQUENCE tag");
        assert_eq!(tbs[1] & 0x80, 0x80, "expected a long-form DER length");
        let length_bytes = (tbs[1] & 0x7f) as usize;
        let header_len = 2 + length_bytes;
        let body_len = tbs[2..header_len]
            .iter()
            .fold(0usize, |acc, byte| (acc << 8) | *byte as usize);
        assert_eq!(
            tbs.len(),
            header_len + body_len,
            "hashed slice must be header plus body, not the body alone"
        );

        let listed = base64::engine::general_purpose::STANDARD
            .decode(read_device_thumbprint(&f.root).expect("device thumbprint"))
            .expect("base64");
        assert_eq!(
            listed,
            parse_signer(&f.root).expect("parse signer").thumbprint,
            "the device list and the 138-byte key block must carry one thumbprint"
        );

        let without_header: [u8; 20] = sha1::Sha1::digest(&tbs[header_len..]).into();
        assert_ne!(
            listed.as_slice(),
            without_header.as_slice(),
            "the two readings must differ, else this test proves nothing"
        );
    }

    #[test]
    fn cert_info_carries_the_thumbprint_a_kdm_lists() {
        use sha1::Digest;
        use x509_parser::prelude::*;

        let f = fixtures();
        let info = read_certificate(&f.intermediate);

        let mut config = formulation_config(f, KdmFormulation::MultipleModifiedTransitional1);
        config.device_cert_files = vec![f.intermediate.clone()];
        let kdm = build_kdm(&config).expect("build");
        assert_eq!(
            thumbprints_in(&kdm.xml),
            vec![info.thumbprint.clone()],
            "the displayed thumbprint and the one a KDM lists must be one value"
        );

        let data = std::fs::read(&f.intermediate).unwrap();
        let (_, pem) = parse_x509_pem(&data).unwrap();
        let whole_der: [u8; CERT_THUMBPRINT_LEN] = sha1::Sha1::digest(&pem.contents).into();
        assert_ne!(
            info.thumbprint,
            thumbprint_base64(&whole_der),
            "a hash over the whole certificate is not the ST 430-2 thumbprint"
        );
    }

    /// Downstream code reads an empty thumbprint as "not a valid certificate".
    #[test]
    fn an_unparsable_certificate_has_an_empty_thumbprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-cert.pem");
        std::fs::write(&path, b"not a certificate").unwrap();
        assert!(read_certificate(&path).thumbprint.is_empty());
    }

    #[test]
    fn every_kdm_carries_an_authorized_device_info() {
        let f = fixtures();
        for format in [KdmFormat::Smpte, KdmFormat::Interop] {
            let mut config = test_config(f, PathBuf::from("unused"));
            config.format = format;
            let kdm = build_kdm(&config).expect("build");

            assert!(
                kdm.xml.contains("<AuthorizedDeviceInfo>"),
                "{format:?} KDM must carry AuthorizedDeviceInfo"
            );
            assert!(
                kdm.xml.contains("<DeviceListIdentifier>urn:uuid:"),
                "{format:?} DeviceListIdentifier must be a urn:uuid"
            );
            assert!(
                !thumbprints_in(&kdm.xml).is_empty(),
                "{format:?} DeviceList must not be empty"
            );
        }
    }

    #[test]
    fn authorized_device_info_sits_between_the_validity_window_and_the_key_list() {
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build");

        let not_after = kdm
            .xml
            .find("<ContentKeysNotValidAfter>")
            .expect("not after");
        let device_info = kdm.xml.find("<AuthorizedDeviceInfo>").expect("device info");
        let key_id_list = kdm.xml.find("<KeyIdList>").expect("key id list");
        assert!(
            not_after < device_info && device_info < key_id_list,
            "ST 430-1 fixes this sequence order"
        );
    }

    #[test]
    fn the_default_device_list_is_the_assume_trust_thumbprint_alone() {
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build");
        assert_eq!(
            thumbprints_in(&kdm.xml),
            vec![ASSUME_TRUST_THUMBPRINT.to_string()],
            "assume-trust only works when nothing else is listed"
        );
    }

    /// The ST 430-2 thumbprint of a certificate, computed here rather than
    /// through the code under test, so a hash over the wrong bytes fails.
    fn expected_thumbprint(cert_path: &Path) -> String {
        use sha1::Digest;
        use x509_parser::prelude::*;

        let data = std::fs::read(cert_path).expect("read cert");
        let (_, pem) = parse_x509_pem(&data).expect("parse PEM");
        let cert = pem.parse_x509().expect("parse X.509");
        let digest: [u8; 20] = sha1::Sha1::digest(cert.tbs_certificate.as_ref()).into();
        base64::engine::general_purpose::STANDARD.encode(digest)
    }

    #[test]
    fn supplied_devices_replace_the_assume_trust_thumbprint() {
        let f = fixtures();
        let config = formulation_config(f, KdmFormulation::MultipleModifiedTransitional1);
        assert_eq!(
            config.device_cert_files,
            vec![f.signer.clone(), f.intermediate.clone()]
        );
        let kdm = build_kdm(&config).expect("build");

        let listed = thumbprints_in(&kdm.xml);
        let expected = vec![
            expected_thumbprint(&f.signer),
            expected_thumbprint(&f.intermediate),
        ];
        assert_eq!(
            listed, expected,
            "each device contributes its own thumbprint"
        );
        assert!(
            !listed.contains(&ASSUME_TRUST_THUMBPRINT.to_string()),
            "mixing assume-trust with a real device disables the device restriction"
        );
    }

    #[test]
    fn a_device_cert_that_cannot_be_read_fails_loud() {
        let f = fixtures();
        let mut config = formulation_config(f, KdmFormulation::DciSpecific);
        config.device_cert_files = vec![PathBuf::from("/nonexistent/device.pem")];
        let err = build_kdm(&config).expect_err("must not build with an unreadable device cert");
        assert!(err.contains("cannot read device cert"), "got: {err}");
    }

    #[test]
    fn a_rewrapped_kdm_carries_its_own_device_list() {
        let f = fixtures();
        let src_keys = vec![KdmKey {
            key_type: *b"MDIK",
            key_id: uuid::Uuid::new_v4(),
            content_key: [7u8; 16],
        }];
        let dkdm_xml = build_stand_in_dkdm(f, &f.signer, &src_keys);

        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(&dkdm_path, &dkdm_xml).unwrap();

        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.signer_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: dir.path().join("rewrapped.kdm.xml"),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![f.intermediate.clone()],
            formulation: KdmFormulation::MultipleModifiedTransitional1,
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        let rewrapped = rewrap_dkdm(&config).expect("rewrap");
        assert_eq!(
            thumbprints_in(&rewrapped.xml),
            vec![expected_thumbprint(&f.intermediate)]
        );
    }

    /// KDMs written by DCP-o-matic 2.18.39 (libdcp), one per formulation, with
    /// `dcpomatic2_kdm_cli -F <formulation> -T certs/intermediate.pem`. Signer
    /// and recipient are both certs/leaf.pem. A second implementation's output
    /// is the oracle here: nothing in these tests is a transcribed value.
    fn dcp_o_matic_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("dcp-o-matic-2.18.39")
    }

    fn dcp_o_matic_cert(name: &str) -> PathBuf {
        dcp_o_matic_dir()
            .join("certs")
            .join(format!("{name}.{CERTIFICATE_EXTENSION}"))
    }

    fn dcp_o_matic_kdm(formulation: KdmFormulation) -> String {
        let path = dcp_o_matic_dir().join(format!("kdm-{formulation}.xml"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Every thumbprint DCP-o-matic wrote must come back out of postkit's own
    /// hashing of the same certificates, and each formulation must carry the
    /// elements libdcp gives it.
    #[test]
    fn postkit_hashes_certificates_the_way_dcp_o_matic_does() {
        for formulation in KdmFormulation::ALL {
            let reference = dcp_o_matic_kdm(formulation);

            let expected_devices = if formulation.lists_supplied_devices() {
                vec![read_device_thumbprint(&dcp_o_matic_cert("intermediate")).unwrap()]
            } else {
                vec![ASSUME_TRUST_THUMBPRINT.to_string()]
            };
            assert_eq!(
                thumbprints_in(&reference),
                expected_devices,
                "{formulation} device list"
            );

            let expected_authenticator = if formulation.carries_content_authenticator() {
                vec![read_device_thumbprint(&dcp_o_matic_cert("leaf")).unwrap()]
            } else {
                vec![]
            };
            assert_eq!(
                content_authenticators_in(&reference),
                expected_authenticator,
                "{formulation} ContentAuthenticator: it is the signer leaf's thumbprint, \
                 present only for the dci formulations"
            );
        }
    }

    /// The subject and issuer of a fixture certificate, as postkit spells them
    /// into a KDM: rendered, then XML-escaped exactly as the writer does, so
    /// the comparison is against the bytes in the file.
    fn dcp_o_matic_names(name: &str) -> (String, String) {
        use x509_parser::prelude::*;

        let data = std::fs::read(dcp_o_matic_cert(name)).unwrap();
        let (_, pem) = parse_x509_pem(&data).unwrap();
        let cert = pem.parse_x509().unwrap();
        (
            xml_escape(&distinguished_name(cert.subject())),
            xml_escape(&distinguished_name(cert.issuer())),
        )
    }

    /// libdcp renders both names with OpenSSL's `XN_FLAG_RFC2253`, so postkit's
    /// rendering of the very certificates DCP-o-matic held has to equal what it
    /// wrote, byte for byte. The KDM names the leaf's subject as the recipient,
    /// and the issuer of each of the three certificates in its ds:KeyInfo.
    #[test]
    fn distinguished_names_are_spelled_the_way_dcp_o_matic_wrote_them() {
        let (leaf_subject, leaf_issuer) = dcp_o_matic_names("leaf");
        let (_, intermediate_issuer) = dcp_o_matic_names("intermediate");
        let (_, root_issuer) = dcp_o_matic_names("root");

        // The DER order this starts from is a different string, so the equality
        // below is discriminating and not just any rendering passing.
        {
            use x509_parser::prelude::*;
            let data = std::fs::read(dcp_o_matic_cert("leaf")).unwrap();
            let (_, pem) = parse_x509_pem(&data).unwrap();
            let cert = pem.parse_x509().unwrap();
            assert_ne!(
                cert.subject().to_string(),
                leaf_subject,
                "x509-parser's DER-order Display must not already be the answer"
            );
        }

        for formulation in KdmFormulation::ALL {
            let reference = dcp_o_matic_kdm(formulation);

            assert_eq!(
                elements_in(&reference, X509_SUBJECT_NAME_ELEMENT),
                vec![leaf_subject.clone()],
                "{formulation} recipient subject name"
            );

            let mut written = elements_in(&reference, X509_ISSUER_NAME_ELEMENT);
            written.sort();
            written.dedup();
            let mut expected = vec![
                leaf_issuer.clone(),
                intermediate_issuer.clone(),
                root_issuer.clone(),
            ];
            expected.sort();
            expected.dedup();
            assert_eq!(written, expected, "{formulation} issuer names");
        }
    }

    /// RFC 4514 escaping is not exercised by any fixture certificate: they hold
    /// nothing but PrintableString values with no reserved character in them.
    #[test]
    fn a_distinguished_name_value_is_escaped_the_way_rfc_4514_requires() {
        use x509_parser::prelude::*;

        let f = fixtures();
        let key_pem = std::fs::read_to_string(&f.root_key).expect("read root key");
        let key = rcgen::KeyPair::from_pem(&key_pem).expect("parse root key");

        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("#a,b+c\"d\\e<f>g;h ".to_string()),
        );
        let cert = params.self_signed(&key).expect("self-sign");
        let pem_text = cert.pem();
        let (_, pem) = parse_x509_pem(pem_text.as_bytes()).unwrap();
        let parsed = pem.parse_x509().unwrap();

        assert_eq!(
            distinguished_name(parsed.subject()),
            r#"CN=\#a\,b\+c\"d\\e\<f\>g\;h\ "#,
            "a leading '#', the separators, and a trailing space all take a backslash"
        );
    }

    /// DCP-o-matic refuses a signer chain holding a certificate that spans more
    /// than `MAX_CERTIFICATE_VALIDITY_YEARS`, which is what its
    /// `Config::check_certificates` measures on the year fields alone.
    #[test]
    fn no_generated_tier_spans_longer_than_dcp_o_matic_accepts() {
        use x509_parser::prelude::*;

        let f = fixtures();
        for path in [&f.root, &f.intermediate, &f.signer] {
            let data = std::fs::read(path).unwrap();
            let (_, pem) = parse_x509_pem(&data).unwrap();
            let cert = pem.parse_x509().unwrap();

            let span = cert.validity().not_after.to_datetime().year()
                - cert.validity().not_before.to_datetime().year();
            assert!(
                span <= MAX_CERTIFICATE_VALIDITY_YEARS,
                "{} spans {span} years, more than the {MAX_CERTIFICATE_VALIDITY_YEARS} \
                 DCP-o-matic accepts",
                path.display()
            );
        }
    }

    /// A caller asking for a longer validity by hand has to be refused too, and
    /// before any key is generated for it.
    #[test]
    fn an_over_long_validity_is_refused_rather_than_minted() {
        let dir = tempfile::tempdir().unwrap();
        let opts = CertOptions {
            cert_type: CertType::Root,
            common_name: common_name(CN_ROLE_CERTIFICATE_AUTHORITY, "Acme", CN_TIER_ROOT),
            validity_days: (MAX_CERTIFICATE_VALIDITY_YEARS as u32 + 1) * DAYS_PER_YEAR,
            output_cert: dir.path().join("root.pem"),
            output_key: dir.path().join("root.key"),
            ..Default::default()
        };
        assert_eq!(generate_certificate(&opts), -1);
        assert!(
            !opts.output_cert.exists(),
            "nothing may be written for a validity that will be rejected"
        );
    }

    /// The same comparison the other way round: postkit's KDM for the same
    /// inputs lists what DCP-o-matic listed. The fixtures ship no private key,
    /// so postkit signs with its own leaf and only the presence of
    /// ContentAuthenticator can be compared, its value being that other signer.
    #[test]
    fn postkit_writes_the_device_list_dcp_o_matic_wrote() {
        let f = fixtures();
        for formulation in KdmFormulation::ALL {
            let reference = dcp_o_matic_kdm(formulation);

            let mut config = test_config(f, PathBuf::from("unused"));
            config.formulation = formulation;
            config.recipient_cert_file = dcp_o_matic_cert("leaf");
            config.device_cert_files = if formulation.lists_supplied_devices() {
                vec![dcp_o_matic_cert("intermediate")]
            } else {
                vec![]
            };
            let kdm = build_kdm(&config).expect("build");

            assert_eq!(
                thumbprints_in(&kdm.xml),
                thumbprints_in(&reference),
                "{formulation} device list must match DCP-o-matic's"
            );

            let expected_authenticator = if formulation.carries_content_authenticator() {
                vec![expected_thumbprint(&config.signer_cert_file)]
            } else {
                vec![]
            };
            assert_eq!(
                content_authenticators_in(&kdm.xml),
                expected_authenticator,
                "{formulation} ContentAuthenticator"
            );
            assert_eq!(
                content_authenticators_in(&kdm.xml).len(),
                content_authenticators_in(&reference).len(),
                "{formulation} must carry a ContentAuthenticator exactly when DCP-o-matic does"
            );
        }
    }

    #[test]
    fn a_formulation_that_contradicts_the_device_list_is_rejected() {
        let f = fixtures();
        for formulation in KdmFormulation::ALL {
            let mut config = formulation_config(f, formulation);
            // The device list the formulation forbids: none where it needs one,
            // one where it allows none.
            config.device_cert_files = if formulation.lists_supplied_devices() {
                vec![]
            } else {
                vec![f.intermediate.clone()]
            };

            let err = build_kdm(&config)
                .expect_err("a formulation contradicting the device list must not build");
            assert!(err.contains(formulation.as_str()), "got: {err}");
            assert!(
                err.contains(formulation.device_list_counterpart().as_str()),
                "the error must name the formulation to use instead, got: {err}"
            );
        }
    }

    #[test]
    fn a_rewrap_formulation_that_contradicts_the_device_list_is_rejected() {
        let f = fixtures();
        let src_keys = vec![KdmKey {
            key_type: *b"MDIK",
            key_id: uuid::Uuid::new_v4(),
            content_key: [9u8; 16],
        }];
        let dkdm_xml = build_stand_in_dkdm(f, &f.signer, &src_keys);
        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(&dkdm_path, &dkdm_xml).unwrap();

        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.signer_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: dir.path().join("rewrapped.kdm.xml"),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![f.intermediate.clone()],
            formulation: KdmFormulation::ModifiedTransitional1,
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        let err = rewrap_dkdm(&config).expect_err("re-wrap must not drop the device list either");
        assert!(
            err.contains("multiple-modified-transitional-1"),
            "got: {err}"
        );
    }

    /// Write a store entry in the layout used before the ST 430-2 thumbprint:
    /// the file stem and the record both hex SHA-1 over the whole DER.
    fn write_old_layout_device(store: &Path, cert_path: &Path, name: &str) -> String {
        use sha1::Digest;
        use x509_parser::prelude::*;

        let data = std::fs::read(cert_path).unwrap();
        let (_, pem) = parse_x509_pem(&data).unwrap();
        let old_thumbprint = hex::encode(sha1::Sha1::digest(&pem.contents));

        std::fs::create_dir_all(store).unwrap();
        std::fs::copy(
            cert_path,
            store.join(format!("{old_thumbprint}.{CERTIFICATE_EXTENSION}")),
        )
        .unwrap();
        let device = TrustedDevice {
            name: name.to_string(),
            thumbprint: old_thumbprint.clone(),
            certificate_path: cert_path.to_path_buf(),
        };
        std::fs::write(
            store.join(format!("{old_thumbprint}.{DEVICE_RECORD_EXTENSION}")),
            serde_json::to_string_pretty(&device).unwrap(),
        )
        .unwrap();
        old_thumbprint
    }

    /// File name and content of everything in the store, for comparing one pass
    /// against the next.
    fn store_contents(store: &Path) -> Vec<(String, String)> {
        let mut files: Vec<(String, String)> = std::fs::read_dir(store)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(&path).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn the_store_migrates_old_thumbprints_and_then_leaves_them_alone() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join(TRUSTED_DEVICES_DIR_NAME);
        let old_thumbprint = write_old_layout_device(&store, &f.signer, "Screen 1");

        let devices = list_trusted_devices_in(&store);
        assert_eq!(devices.len(), 1, "the record must survive migration");
        assert_eq!(
            devices[0].thumbprint,
            expected_thumbprint(&f.signer),
            "the record must carry the ST 430-2 thumbprint"
        );
        assert_eq!(
            devices[0].name, "Screen 1",
            "migration must not lose fields"
        );

        let expected_stem = hex::encode(
            base64::engine::general_purpose::STANDARD
                .decode(expected_thumbprint(&f.signer))
                .unwrap(),
        );
        let migrated = store_contents(&store);
        assert_eq!(
            migrated.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            vec![
                &format!("{expected_stem}.{DEVICE_RECORD_EXTENSION}"),
                &format!("{expected_stem}.{CERTIFICATE_EXTENSION}"),
            ],
            "both files must be renamed to the hex spelling of the new thumbprint"
        );
        assert_ne!(
            expected_stem, old_thumbprint,
            "else this test proves nothing"
        );

        list_trusted_devices_in(&store);
        assert_eq!(
            store_contents(&store),
            migrated,
            "a second pass must change nothing"
        );
    }

    #[test]
    fn a_record_with_no_certificate_beside_it_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join(TRUSTED_DEVICES_DIR_NAME);
        std::fs::create_dir_all(&store).unwrap();
        let device = TrustedDevice {
            name: "Orphan".to_string(),
            thumbprint: "whatever-was-stored".to_string(),
            certificate_path: PathBuf::from("/gone.pem"),
        };
        let path = store.join(format!("orphan.{DEVICE_RECORD_EXTENSION}"));
        std::fs::write(&path, serde_json::to_string_pretty(&device).unwrap()).unwrap();
        let before = store_contents(&store);

        let listed = list_trusted_devices_in(&store);
        assert_eq!(listed.len(), 1, "the record is still listed");
        assert_eq!(listed[0].thumbprint, "whatever-was-stored");
        assert_eq!(store_contents(&store), before, "nothing may be rewritten");
    }

    #[test]
    fn a_device_is_added_and_removed_by_its_st_430_2_thumbprint() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join(TRUSTED_DEVICES_DIR_NAME);

        assert_eq!(add_trusted_device_in(&store, &f.signer, "Screen 2"), 0);
        let thumbprint = expected_thumbprint(&f.signer);
        assert_eq!(
            list_trusted_devices_in(&store)
                .iter()
                .map(|d| d.thumbprint.clone())
                .collect::<Vec<_>>(),
            vec![thumbprint.clone()]
        );

        assert_eq!(remove_trusted_device_in(&store, &thumbprint), 0);
        assert!(list_trusted_devices_in(&store).is_empty());
        assert!(
            store_contents(&store).is_empty(),
            "the certificate copy must go with the record"
        );
        assert_eq!(
            remove_trusted_device_in(&store, &thumbprint),
            -1,
            "removing what is not there must fail"
        );
    }

    #[test]
    fn every_isdcf_formulation_spelling_round_trips() {
        for formulation in KdmFormulation::ALL {
            assert_eq!(
                formulation.as_str().parse::<KdmFormulation>().unwrap(),
                formulation
            );
        }
        for formulation in KdmFormulation::ALL {
            assert_eq!(
                formulation
                    .as_str()
                    .to_uppercase()
                    .parse::<KdmFormulation>()
                    .unwrap(),
                formulation,
                "a command line may spell the formulation in any case"
            );
        }

        let err = "dci-anything".parse::<KdmFormulation>().unwrap_err();
        for formulation in KdmFormulation::ALL {
            assert!(err.contains(formulation.as_str()), "got: {err}");
        }
        assert!(
            "".parse::<KdmFormulation>().is_err(),
            "an empty value must not default"
        );
    }

    /// Path to the vendored ST 430-1 KDM schema and the catalog that maps the
    /// two W3C imports to their local copies.
    fn kdm_schema() -> (PathBuf, PathBuf) {
        let schemas = Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
        (
            schemas.join("SMPTE-430-1-2006-KDM.xsd"),
            schemas.join("catalog.xml"),
        )
    }

    /// Validate one KDMRequiredExtensions document against the vendored schema.
    fn xmllint_kdm_schema(fragment_path: &Path) -> std::process::Output {
        let (schema, catalog) = kdm_schema();
        std::process::Command::new("xmllint")
            .env("XML_CATALOG_FILES", &catalog)
            .args(["--nonet", "--noout", "--schema"])
            .arg(&schema)
            .arg(fragment_path)
            .output()
            .expect("run xmllint")
    }

    /// The generated KDMRequiredExtensions against the vendored ST 430-1 schema,
    /// once per formulation, so both device-list forms and both
    /// ContentAuthenticator forms are covered.
    #[test]
    fn kdm_required_extensions_pass_the_st_430_1_xsd() {
        if !xmllint_available() {
            eprintln!("skipping: xmllint not installed");
            return;
        }

        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        for formulation in KdmFormulation::ALL {
            let label = formulation.as_str();
            let kdm = build_kdm(&formulation_config(f, formulation)).expect("build");

            let path = dir.path().join(format!("{label}.xml"));
            std::fs::write(&path, required_extensions_fragment(&kdm.xml)).unwrap();
            let out = xmllint_kdm_schema(&path);
            assert!(
                out.status.success(),
                "the {label} KDM must pass the ST 430-1 XSD:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The whole message against the vendored ST 430-1 schema, which imports
    /// ST 430-3, so the envelope around the extensions is checked too:
    /// dcpdoctor validates a KDM this way.
    #[test]
    fn a_whole_kdm_passes_the_st_430_1_xsd() {
        if !xmllint_available() {
            eprintln!("skipping: xmllint not installed");
            return;
        }

        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        for formulation in KdmFormulation::ALL {
            let label = formulation.as_str();
            let kdm = build_kdm(&formulation_config(f, formulation)).expect("build");
            let path = dir.path().join(format!("{label}-whole.xml"));
            std::fs::write(&path, &kdm.xml).unwrap();
            let out = xmllint_kdm_schema(&path);
            assert!(
                out.status.success(),
                "the whole {label} KDM must pass the ST 430-1 XSD:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The three states that write a flag, and what each one must put in the
    /// element.
    fn forensic_marking_states() -> Vec<(
        &'static str,
        PictureForensicMarking,
        AudioForensicMarking,
        Vec<String>,
    )> {
        vec![
            (
                "picture-disabled",
                PictureForensicMarking::Disabled,
                AudioForensicMarking::Enabled,
                vec![FORENSIC_MARK_PICTURE_DISABLE.to_string()],
            ),
            (
                "audio-disabled",
                PictureForensicMarking::Enabled,
                AudioForensicMarking::Disabled,
                vec![FORENSIC_MARK_AUDIO_DISABLE.to_string()],
            ),
            (
                "audio-disabled-above-channel",
                PictureForensicMarking::Enabled,
                AudioForensicMarking::DisabledAboveChannel(HI_VI_CHANNEL),
                vec![format!(
                    "{FORENSIC_MARK_AUDIO_DISABLE}{FORENSIC_MARK_ABOVE_CHANNEL_SUFFIX}{HI_VI_CHANNEL}"
                )],
            ),
        ]
    }

    /// The channel a 5.1 mix ends on, so marking off above it exempts the HI/VI
    /// tracks that follow. Any number would do here; this is a realistic order.
    const HI_VI_CHANNEL: u32 = 6;

    #[test]
    fn a_kdm_with_marking_left_on_has_no_forensic_mark_flag_list() {
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build");
        assert!(
            !kdm.xml.contains(FORENSIC_MARK_FLAG_LIST_ELEMENT),
            "the element is minOccurs=0 and must be absent when nothing is disabled"
        );
    }

    #[test]
    fn each_forensic_marking_state_writes_the_flags_libdcp_writes() {
        let f = fixtures();
        for (label, picture, audio, expected) in forensic_marking_states() {
            let mut config = test_config(f, PathBuf::from("unused"));
            config.picture_forensic_marking = picture;
            config.audio_forensic_marking = audio;
            let kdm = build_kdm(&config).expect("build");
            assert_eq!(
                elements_in(&kdm.xml, FORENSIC_MARK_FLAG_ELEMENT),
                expected,
                "{label} flags"
            );
        }
    }

    /// Both flags at once, in libdcp's order: picture first.
    #[test]
    fn picture_and_audio_flags_sit_in_libdcps_order() {
        let f = fixtures();
        let mut config = test_config(f, PathBuf::from("unused"));
        config.picture_forensic_marking = PictureForensicMarking::Disabled;
        config.audio_forensic_marking = AudioForensicMarking::DisabledAboveChannel(HI_VI_CHANNEL);
        let kdm = build_kdm(&config).expect("build");
        assert_eq!(
            elements_in(&kdm.xml, FORENSIC_MARK_FLAG_ELEMENT),
            vec![
                FORENSIC_MARK_PICTURE_DISABLE.to_string(),
                format!(
                    "{FORENSIC_MARK_AUDIO_DISABLE}{FORENSIC_MARK_ABOVE_CHANNEL_SUFFIX}{HI_VI_CHANNEL}"
                ),
            ]
        );
    }

    /// Each state through the same ST 430-1 schema the other KDM tests use, so
    /// the element's position after KeyIdList is checked and not assumed.
    #[test]
    fn every_forensic_marking_state_passes_the_st_430_1_xsd() {
        if !xmllint_available() {
            eprintln!("skipping: xmllint not installed");
            return;
        }

        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        for (label, picture, audio, _) in forensic_marking_states() {
            let mut config = test_config(f, PathBuf::from("unused"));
            config.picture_forensic_marking = picture;
            config.audio_forensic_marking = audio;
            let kdm = build_kdm(&config).expect("build");

            let path = dir.path().join(format!("{label}.xml"));
            std::fs::write(&path, required_extensions_fragment(&kdm.xml)).unwrap();
            let out = xmllint_kdm_schema(&path);
            assert!(
                out.status.success(),
                "the {label} KDM must pass the ST 430-1 XSD:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// A re-wrapped KDM carries the re-issuer's marking order, not the source
    /// DKDM's absence of one.
    #[test]
    fn a_rewrapped_kdm_carries_its_own_forensic_marking() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let dkdm_path = dir.path().join("dkdm.xml");
        std::fs::write(
            &dkdm_path,
            build_stand_in_dkdm(
                f,
                &f.signer,
                &[KdmKey {
                    key_type: *b"MDIK",
                    key_id: uuid::Uuid::new_v4(),
                    content_key: [0x33u8; 16],
                }],
            ),
        )
        .unwrap();

        let config = RewrapConfig {
            dkdm_file: dkdm_path,
            dkdm_recipient_key_file: f.signer_key.clone(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: PathBuf::from("unused"),
            valid_from: String::new(),
            valid_to: String::new(),
            device_cert_files: vec![],
            formulation: KdmFormulation::default(),
            picture_forensic_marking: PictureForensicMarking::Disabled,
            audio_forensic_marking: AudioForensicMarking::default(),
        };
        let kdm = rewrap_dkdm(&config).expect("rewrap");
        assert_eq!(
            elements_in(&kdm.xml, FORENSIC_MARK_FLAG_ELEMENT),
            vec![FORENSIC_MARK_PICTURE_DISABLE.to_string()]
        );
    }

    /// Real Doremi-signed KDMs through the same extraction and schema, which is
    /// what proves the schema handling is not merely self-consistent. Gated on
    /// POSTKIT_SAMPLE_KDMS, a directory of .xml KDMs.
    #[test]
    fn real_kdms_pass_the_same_schema() {
        let Ok(sample_dir) = std::env::var("POSTKIT_SAMPLE_KDMS") else {
            eprintln!("skipping: set POSTKIT_SAMPLE_KDMS to a directory of real KDMs");
            return;
        };
        if !xmllint_available() {
            eprintln!("skipping: xmllint not installed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let mut checked = 0;
        for entry in std::fs::read_dir(&sample_dir).expect("read sample dir") {
            let source = entry.expect("dir entry").path();
            if source.extension().is_none_or(|e| e != "xml") {
                continue;
            }
            let xml = std::fs::read_to_string(&source).expect("read sample KDM");
            if !xml.contains("<KDMRequiredExtensions") {
                continue;
            }

            let path = dir.path().join(format!("sample{checked}.xml"));
            std::fs::write(&path, required_extensions_fragment(&xml)).unwrap();
            let out = xmllint_kdm_schema(&path);
            assert!(
                out.status.success(),
                "real KDM {} must pass the ST 430-1 XSD:\n{}",
                source.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            checked += 1;
        }
        assert!(checked > 0, "POSTKIT_SAMPLE_KDMS held no KDM to check");
    }

    #[test]
    fn generated_certificate_serials_fit_the_st_430_2_cap() {
        use x509_parser::prelude::*;

        let f = fixtures();
        for cert_path in [&f.root, &f.intermediate, &f.signer] {
            let data = std::fs::read(cert_path).unwrap();
            let (_, pem) = parse_x509_pem(&data).unwrap();
            let cert = pem.parse_x509().unwrap();
            let decimal = cert.serial.to_str_radix(10);

            // ST 430-2 5.2 caps the serial at an unsigned 64-bit value and DCI
            // CTP 2.1.4 fails anything larger.
            decimal.parse::<u64>().unwrap_or_else(|e| {
                panic!(
                    "{} has serial {decimal}, which does not fit 64 bits: {e}",
                    cert_path.display()
                )
            });
            assert_ne!(decimal, "0", "{} has a zero serial", cert_path.display());
        }
    }

    #[test]
    fn the_signer_is_an_issuer_and_serial_pair_with_no_subject() {
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build");
        let signer_start = kdm.xml.find("<Signer ").expect("Signer");
        let signer_end = kdm.xml.find("</Signer>").expect("Signer end");
        let signer = &kdm.xml[signer_start..signer_end];

        assert!(signer.contains("<ds:X509IssuerName>"));
        assert!(signer.contains("<ds:X509SerialNumber>"));
        assert!(
            !signer.contains("X509SubjectName"),
            "ST 430-3 types Signer as ds:X509IssuerSerialType, which has no subject: {signer}"
        );
    }

    #[test]
    fn the_recipient_keeps_its_subject_name_beside_the_issuer_serial() {
        let f = fixtures();
        let kdm = build_kdm(&test_config(f, PathBuf::from("unused"))).expect("build");
        let issuer_serial_end = kdm
            .xml
            .find("</X509IssuerSerial>")
            .expect("X509IssuerSerial end");
        let subject = kdm
            .xml
            .find("<X509SubjectName>")
            .expect("recipient X509SubjectName");
        assert!(
            subject > issuer_serial_end,
            "X509SubjectName is a sibling of X509IssuerSerial, not a child of it"
        );
    }

    /// The four ISDCF names a command line offers under `--formulation`. Spelled
    /// out here because this is the value the table has to hold, not something
    /// derived from it.
    #[test]
    fn the_public_formulation_table_holds_every_isdcf_name() {
        let spellings: Vec<&str> = KdmFormulation::ALL.iter().map(|f| f.as_str()).collect();
        assert_eq!(
            spellings,
            vec![
                "modified-transitional-1",
                "multiple-modified-transitional-1",
                "dci-any",
                "dci-specific",
            ]
        );
    }

    /// A caller validates the formulation against its device list before doing
    /// any work, so the public predicates have to reach the verdict `build_kdm`
    /// reaches later. The paths are never opened by this check.
    #[test]
    fn the_device_list_predicates_agree_with_what_generation_enforces() {
        let no_devices: Vec<PathBuf> = vec![];
        let one_device = vec![PathBuf::from("device.pem")];

        for formulation in KdmFormulation::ALL {
            let takes_devices = formulation.lists_supplied_devices();
            assert_eq!(
                check_formulation_devices(formulation, &one_device).is_ok(),
                takes_devices,
                "{formulation} with a device list"
            );
            assert_eq!(
                check_formulation_devices(formulation, &no_devices).is_ok(),
                !takes_devices,
                "{formulation} with no device list"
            );

            let counterpart = formulation.device_list_counterpart();
            assert_ne!(counterpart, formulation);
            assert_eq!(
                counterpart.lists_supplied_devices(),
                !takes_devices,
                "the counterpart named in an error must take the device list {formulation} cannot"
            );
        }
    }

    #[test]
    fn both_kdm_format_spellings_round_trip() {
        assert_eq!(KdmFormat::Smpte.as_str(), "smpte");
        assert_eq!(KdmFormat::Interop.as_str(), "interop");
        for format in KdmFormat::ALL {
            assert_eq!(format.as_str().parse::<KdmFormat>().unwrap(), format);
            assert_eq!(format.to_string(), format.as_str());
            assert_eq!(
                format.as_str().to_uppercase().parse::<KdmFormat>().unwrap(),
                format,
                "a command line may spell the format in any case"
            );
        }

        let err = "smtpe".parse::<KdmFormat>().unwrap_err();
        for format in KdmFormat::ALL {
            assert!(err.contains(format.as_str()), "got: {err}");
        }
        assert!(
            "".parse::<KdmFormat>().is_err(),
            "an empty value must not default"
        );
    }

    /// The command line spelling is a separate vocabulary from the stored one:
    /// a preferences file written before `FromStr` existed still reads back.
    #[test]
    fn kdm_format_serde_still_uses_the_variant_names() {
        assert_eq!(
            serde_json::to_string(&KdmFormat::Interop).unwrap(),
            "\"Interop\""
        );
        assert_eq!(
            serde_json::from_str::<KdmFormat>("\"Interop\"").unwrap(),
            KdmFormat::Interop
        );
    }

    /// A caller checking its own output asks for the URIs rather than spelling
    /// them out, so every marking state has to render what the KDM carries.
    #[test]
    fn forensic_mark_flag_uris_are_what_a_written_kdm_carries() {
        let f = fixtures();
        let states = forensic_marking_states()
            .into_iter()
            .map(|(label, picture, audio, _)| (label, picture, audio))
            .chain([(
                "picture-and-audio-disabled",
                PictureForensicMarking::Disabled,
                AudioForensicMarking::DisabledAboveChannel(HI_VI_CHANNEL),
            )]);
        for (label, picture, audio) in states {
            let mut config = test_config(f, PathBuf::from("unused"));
            config.picture_forensic_marking = picture;
            config.audio_forensic_marking = audio;
            let kdm = build_kdm(&config).expect("build");
            assert_eq!(
                forensic_mark_flag_uris(picture, audio),
                elements_in(&kdm.xml, FORENSIC_MARK_FLAG_ELEMENT),
                "{label}"
            );
        }

        assert!(
            forensic_mark_flag_uris(
                PictureForensicMarking::Enabled,
                AudioForensicMarking::Enabled
            )
            .is_empty(),
            "marking left on writes no flag at all"
        );
    }

    /// Each public element name against a KDM built to contain it.
    #[test]
    fn the_public_element_names_find_what_a_kdm_is_written_with() {
        let f = fixtures();
        let mut config = formulation_config(f, KdmFormulation::DciSpecific);
        config.picture_forensic_marking = PictureForensicMarking::Disabled;
        let kdm = build_kdm(&config).expect("build");

        assert_eq!(
            content_authenticators_in(&kdm.xml),
            vec![expected_thumbprint(&config.signer_cert_file)],
            "ContentAuthenticator must find the element dci-specific writes"
        );
        assert_eq!(
            thumbprints_in(&kdm.xml).len(),
            config.device_cert_files.len(),
            "CertificateThumbprint must find one element per listed device"
        );
        assert_eq!(
            elements_in(&kdm.xml, FORENSIC_MARK_FLAG_LIST_ELEMENT).len(),
            1,
            "ForensicMarkFlagList must find the list a disabled marking writes"
        );
        assert_eq!(
            elements_in(&kdm.xml, FORENSIC_MARK_FLAG_ELEMENT),
            forensic_mark_flag_uris(
                config.picture_forensic_marking,
                config.audio_forensic_marking
            ),
            "ForensicMarkFlag must find one element per rendered URI"
        );
    }

    /// The validity bounds of a generated certificate.
    struct CertificateValidity {
        not_before: chrono::DateTime<chrono::Utc>,
        not_after: chrono::DateTime<chrono::Utc>,
    }

    fn certificate_validity(cert_path: &Path) -> CertificateValidity {
        use x509_parser::prelude::*;
        let data = std::fs::read(cert_path).expect("read certificate");
        let (_, pem) = parse_x509_pem(&data).expect("parse PEM");
        let cert = pem.parse_x509().expect("parse X.509");
        CertificateValidity {
            not_before: certificate_validity_timestamp(cert.validity().not_before).unwrap(),
            not_after: certificate_validity_timestamp(cert.validity().not_after).unwrap(),
        }
    }

    /// Each tier is minted after the one above it, so equal spans would leave a
    /// child outliving the issuer that vouches for it.
    #[test]
    fn each_certificate_tier_expires_inside_its_issuer() {
        let f = fixtures();
        let root = certificate_validity(&f.root);
        let intermediate = certificate_validity(&f.intermediate);
        let leaf = certificate_validity(&f.signer);

        for (child, issuer, label) in [
            (&intermediate, &root, "the intermediate under the root"),
            (&leaf, &intermediate, "the leaf under the intermediate"),
        ] {
            assert!(
                child.not_after < issuer.not_after,
                "{label} must expire first, got {} against {}",
                child.not_after,
                issuer.not_after
            );
            assert!(
                child.not_before >= issuer.not_before,
                "{label} must not start before its issuer, got {} against {}",
                child.not_before,
                issuer.not_before
            );
        }
    }
}
