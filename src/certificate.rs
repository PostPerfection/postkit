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
    /// Validity in days (default 10 years)
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
            validity_days: 3650,
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
    pub thumbprint_sha1: String,
}

/// A trusted device entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub name: String,
    pub thumbprint: String,
    pub certificate_path: PathBuf,
}

/// Get the trusted devices directory (XDG data or fallback).
fn trusted_devices_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("postkit").join("trusted_devices")
}

/// Compute SHA-1 thumbprint of DER-encoded certificate bytes.
fn sha1_thumbprint(der_bytes: &[u8]) -> String {
    use sha1::Digest;
    let hash = sha1::Sha1::digest(der_bytes);
    hash.iter().map(|b| format!("{b:02x}")).collect()
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

/// Generate a new X.509 certificate + private key.
pub fn generate_certificate(opts: &CertOptions) -> i32 {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, DnValue, IsCa, KeyPair, KeyUsagePurpose,
    };

    let mut params = CertificateParams::default();
    params.distinguished_name.push(
        DnType::CommonName,
        DnValue::Utf8String(opts.common_name.clone()),
    );
    if !opts.organization.is_empty() {
        params.distinguished_name.push(
            DnType::OrganizationName,
            DnValue::Utf8String(opts.organization.clone()),
        );
    }
    if !opts.organizational_unit.is_empty() {
        params.distinguished_name.push(
            DnType::OrganizationalUnitName,
            DnValue::Utf8String(opts.organizational_unit.clone()),
        );
    }
    if !opts.country.is_empty() {
        params.distinguished_name.push(
            DnType::CountryName,
            DnValue::Utf8String(opts.country.clone()),
        );
    }

    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(opts.validity_days as i64);

    match opts.cert_type {
        CertType::Root => {
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        }
        CertType::Intermediate => {
            params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        }
        CertType::Leaf | CertType::Signer => {
            params.is_ca = IsCa::NoCa;
            params.key_usages = vec![
                KeyUsagePurpose::DigitalSignature,
                KeyUsagePurpose::ContentCommitment,
            ];
        }
    }

    let key_pair = match generate_rsa_keypair(opts.key_bits) {
        Ok(kp) => kp,
        Err(e) => {
            tracing::error!("failed to generate RSA key pair: {e}");
            return -1;
        }
    };

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
        common_name: format!("{organization} Root CA"),
        organization: organization.to_string(),
        organizational_unit: "Digital Cinema".to_string(),
        validity_days: 3650 * 3, // 30 years
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
        common_name: format!("{organization} Intermediate CA"),
        organization: organization.to_string(),
        organizational_unit: "Digital Cinema".to_string(),
        validity_days: 3650 * 2, // 20 years
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
        common_name: format!("{organization} Signer"),
        organization: organization.to_string(),
        organizational_unit: "Digital Cinema".to_string(),
        validity_days: 3650,
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

    let thumbprint = sha1_thumbprint(&pem.contents);

    CertInfo {
        subject_cn,
        issuer_cn,
        serial,
        not_before,
        not_after,
        key_bits,
        is_ca,
        is_expired,
        thumbprint_sha1: thumbprint,
    }
}

/// Validate a certificate chain, leaf first, root last.
///
/// Verifies the issuer signature on every certificate cryptographically. A
/// signature algorithm that x509-parser/ring cannot check is reported as a
/// failure, never as a pass.
pub fn validate_chain(chain: &[PathBuf]) -> i32 {
    match validate_chain_inner(chain) {
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

fn validate_chain_inner(chain: &[PathBuf]) -> Result<usize, String> {
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

    // Each cert must be signed by the next one up; the last must be self-signed.
    for i in 0..certs.len() {
        let issuer = certs.get(i + 1).unwrap_or(&certs[i]);
        let is_root = i + 1 == certs.len();

        if certs[i].issuer() != issuer.subject() {
            return Err(if is_root {
                format!(
                    "root cert is not self-issued: {} (subject '{}', issuer '{}')",
                    chain[i].display(),
                    certs[i].subject(),
                    certs[i].issuer()
                )
            } else {
                format!(
                    "chain broken: issuer of {} ('{}') does not match subject of {} ('{}')",
                    chain[i].display(),
                    certs[i].issuer(),
                    chain[i + 1].display(),
                    issuer.subject()
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
    let dir = trusted_devices_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!("failed to create trusted devices dir: {e}");
        return -1;
    }

    let info = read_certificate(cert_path);
    if info.thumbprint_sha1.is_empty() {
        tracing::error!("failed to read certificate for trusted device");
        return -1;
    }

    let device = TrustedDevice {
        name: name.to_string(),
        thumbprint: info.thumbprint_sha1.clone(),
        certificate_path: cert_path.to_path_buf(),
    };

    // Copy cert to trusted devices dir
    let dest = dir.join(format!("{}.pem", info.thumbprint_sha1));
    if let Err(e) = std::fs::copy(cert_path, &dest) {
        tracing::error!("failed to copy certificate: {e}");
        return -1;
    }

    // Write metadata JSON
    let meta_path = dir.join(format!("{}.json", info.thumbprint_sha1));
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

    tracing::info!("added trusted device '{}' ({})", name, info.thumbprint_sha1);
    0
}

/// List all trusted devices.
pub fn list_trusted_devices() -> Vec<TrustedDevice> {
    let dir = trusted_devices_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json")
            && let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(device) = serde_json::from_str::<TrustedDevice>(&data)
        {
            devices.push(device);
        }
    }
    devices
}

/// Remove a trusted device by thumbprint.
pub fn remove_trusted_device(thumbprint: &str) -> i32 {
    let dir = trusted_devices_dir();
    let pem_path = dir.join(format!("{thumbprint}.pem"));
    let json_path = dir.join(format!("{thumbprint}.json"));

    let mut removed = false;
    if pem_path.exists() {
        if let Err(e) = std::fs::remove_file(&pem_path) {
            tracing::error!("failed to remove {}: {e}", pem_path.display());
            return -1;
        }
        removed = true;
    }
    if json_path.exists() {
        if let Err(e) = std::fs::remove_file(&json_path) {
            tracing::error!("failed to remove {}: {e}", json_path.display());
            return -1;
        }
        removed = true;
    }

    if removed {
        tracing::info!("removed trusted device {thumbprint}");
        0
    } else {
        tracing::warn!("trusted device not found: {thumbprint}");
        -1
    }
}

/// KDM generation configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KdmConfig {
    pub cpl_id: String,
    pub content_title: String,
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
    pub formulation: String,
}

/// SMPTE ST 430-1 Table 6: identifies the KDM cipher block layout.
/// DCI CTP 3.4.16 fails any KDM carrying a different value.
const KDM_STRUCTURE_ID: [u8; 16] = [
    0xf1, 0xdc, 0x12, 0x44, 0x60, 0x16, 0x9a, 0x0e, 0x85, 0xbc, 0x30, 0x06, 0x42, 0xf8, 0x66, 0xab,
];

/// Total size of the SMPTE key block, per ST 430-1 Table 6.
const KDM_KEY_BLOCK_LEN: usize = 138;

/// ST 430-1 6.3.7/6.3.8: timestamps are exactly 25 ASCII characters.
const KDM_TIMESTAMP_LEN: usize = 25;

/// XML Encryption 1.0 5.4.2, mandated by DCI CTP 3.4.12.
const KDM_ENCRYPTION_METHOD: &str = "http://www.w3.org/2001/04/xmlenc#rsa-oaep-mgf1p";

// SMPTE 430-3 ETM ds:Signature profile. Every URI below is what libdcp emits
// in src/encrypted_kdm.cc / src/certificate_chain.cc for a KDM (distinct from
// the CPL/PKL signer), and is what DCI-compliant playback gear checks.
const ETM_NS: &str = "http://www.smpte-ra.org/schemas/430-3/2006/ETM";
const KDM_NS: &str = "http://www.smpte-ra.org/schemas/430-1/2006/KDM";
const DSIG_NS: &str = "http://www.w3.org/2000/09/xmldsig#";
const ENC_NS: &str = "http://www.w3.org/2001/04/xmlenc#";
/// Inclusive Canonical XML 1.0, WithComments. libdcp KDM uses exactly this URI
/// (not exclusive c14n, not the plain variant the CPL signer uses).
const C14N_METHOD: &str = "http://www.w3.org/TR/2001/REC-xml-c14n-20010315#WithComments";
/// RSASSA-PKCS1-v1_5 over SHA-256.
const SIG_METHOD: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
const DIGEST_METHOD: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
/// Id attribute values on the two authenticated elements. The ds:Reference
/// URIs point at these, and a verifier resolves them via the Id attribute.
const AUTH_PUBLIC_ID: &str = "ID_AuthenticatedPublic";
const AUTH_PRIVATE_ID: &str = "ID_AuthenticatedPrivate";

/// Escape text before it goes into the KDM XML.
///
/// Content titles come from user input, so without this a title containing
/// markup could rewrite the surrounding KDM elements.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

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

/// Build the 138-byte plaintext key block defined by SMPTE ST 430-1 Table 6.
///
/// Field order and lengths are fixed by the standard: structure id (16),
/// signer thumbprint (20), CPL id (16), key type (4), key id (16),
/// not-valid-before (25), not-valid-after (25), content key (16).
fn build_kdm_key_block(
    signer_thumbprint: &[u8; 20],
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
    block.extend_from_slice(key_type);
    block.extend_from_slice(key_id.as_bytes());
    block.extend_from_slice(not_before.as_bytes());
    block.extend_from_slice(not_after.as_bytes());
    block.extend_from_slice(content_key);

    // The layout is fixed; a mismatch means the code above drifted from the spec.
    if block.len() != KDM_KEY_BLOCK_LEN {
        return Err(format!(
            "internal error: key block is {} bytes, expected {KDM_KEY_BLOCK_LEN}",
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
    use base64::Engine;

    if config.cpl_id.is_empty() {
        return Err("CPL ID is required".into());
    }

    // Accept either a bare UUID or the urn:uuid: form, and reject anything
    // else: the key block needs the 16 raw bytes, not a free-text string.
    let cpl_uuid = {
        let trimmed = config
            .cpl_id
            .trim()
            .strip_prefix("urn:uuid:")
            .unwrap_or(config.cpl_id.trim());
        uuid::Uuid::parse_str(trimmed)
            .map_err(|e| format!("CPL ID '{}' is not a UUID: {e}", config.cpl_id))?
    };

    let recipient = parse_recipient(&config.recipient_cert_file)?;

    // The signer thumbprint is a required field of the key block, so a missing
    // signer certificate has to stop generation rather than be zero-filled.
    if config.signer_cert_file.as_os_str().is_empty() {
        return Err("signer certificate is required: its thumbprint is part of \
                    the SMPTE 430-1 key block"
            .into());
    }
    let signer = parse_signer(&config.signer_cert_file)?;

    let now = chrono::Utc::now();
    let not_valid_before = if config.valid_from == "now" || config.valid_from.is_empty() {
        now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        config.valid_from.clone()
    };
    let not_valid_after = parse_validity_end(&config.valid_to, &not_valid_before)?;

    let message_id = uuid::Uuid::new_v4();

    let formulation_uri = match config.formulation.to_lowercase().replace(' ', "-").as_str() {
        "dci-any" => "http://www.smpte-ra.org/430-1/2006/KDM#kdm-key-type-dci-any",
        "dci-specific" => "http://www.smpte-ra.org/430-1/2006/KDM#kdm-key-type-dci-specific",
        _ => "http://www.smpte-ra.org/430-1/2006/KDM#kdm-key-type",
    };

    let content_key: [u8; 16] = rand_bytes()?;
    let content_key_id = uuid::Uuid::new_v4();

    // MDIK: image essence key, ST 430-1 6.3.9.3 Table 1.
    let key_block = build_kdm_key_block(
        &signer.thumbprint,
        &cpl_uuid,
        b"MDIK",
        &content_key_id,
        &not_valid_before,
        &not_valid_after,
        &content_key,
    )?;

    let ciphertext = encrypt_key_block(&recipient.public_key, &key_block)?;
    let cipher_value = base64::engine::general_purpose::STANDARD.encode(&ciphertext);

    let title = xml_escape(&config.content_title);
    let recipient_subject = xml_escape(&recipient.subject_dn);
    let recipient_issuer = xml_escape(&recipient.issuer_dn);
    let recipient_serial = xml_escape(&recipient.serial);
    let signer_subject = xml_escape(&signer.subject_dn);
    let signer_issuer = xml_escape(&signer.issuer_dn);
    let signer_serial = xml_escape(&signer.serial);

    // Inner content of the two authenticated elements. Each is reused twice:
    // once inside the document, and once (in build_signature) inside a
    // standalone fragment that carries the document root's namespaces so its
    // canonical form is byte-identical to the subtree the verifier digests.
    let auth_public_inner = format!(
        r#"
    <MessageId>urn:uuid:{message_id}</MessageId>
    <MessageType>{formulation_uri}</MessageType>
    <AnnotationText>{title} KDM for {recipient_subject}</AnnotationText>
    <IssueDate>{issue_date}</IssueDate>
    <Signer>
      <X509IssuerName>{signer_issuer}</X509IssuerName>
      <X509SerialNumber>{signer_serial}</X509SerialNumber>
      <X509SubjectName>{signer_subject}</X509SubjectName>
    </Signer>
    <RequiredExtensions>
      <KDMRequiredExtensions xmlns="{KDM_NS}">
        <Recipient>
          <X509IssuerSerial>
            <X509IssuerName>{recipient_issuer}</X509IssuerName>
            <X509SerialNumber>{recipient_serial}</X509SerialNumber>
          </X509IssuerSerial>
          <X509SubjectName>{recipient_subject}</X509SubjectName>
        </Recipient>
        <CompositionPlaylistId>urn:uuid:{cpl_uuid}</CompositionPlaylistId>
        <ContentTitleText>{title}</ContentTitleText>
        <ContentKeysNotValidBefore>{not_before}</ContentKeysNotValidBefore>
        <ContentKeysNotValidAfter>{not_after}</ContentKeysNotValidAfter>
        <KeyIdList>
          <TypedKeyId>
            <KeyType>MDIK</KeyType>
            <KeyId>urn:uuid:{key_id}</KeyId>
          </TypedKeyId>
        </KeyIdList>
      </KDMRequiredExtensions>
    </RequiredExtensions>
  "#,
        issue_date = now.format("%Y-%m-%dT%H:%M:%S+00:00"),
        not_before = not_valid_before,
        not_after = not_valid_after,
        key_id = content_key_id,
    );

    let auth_private_inner = format!(
        r#"
    <EncryptedKey xmlns="{ENC_NS}">
      <EncryptionMethod Algorithm="{KDM_ENCRYPTION_METHOD}"/>
      <CipherData>
        <CipherValue>{cipher_value}</CipherValue>
      </CipherData>
    </EncryptedKey>
  "#,
    );

    let signature = build_signature(config, &auth_public_inner, &auth_private_inner)?;

    // The root declares the ETM default namespace plus xmlns:ds, exactly the
    // set the standalone fragments in build_signature reproduce.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<DCinemaSecurityMessage xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}">
  <AuthenticatedPublic Id="{AUTH_PUBLIC_ID}">{auth_public_inner}</AuthenticatedPublic>
  <AuthenticatedPrivate Id="{AUTH_PRIVATE_ID}">{auth_private_inner}</AuthenticatedPrivate>
{signature}
</DCinemaSecurityMessage>
"#,
    );

    Ok(GeneratedKdm {
        xml,
        content_key,
        key_id: content_key_id,
    })
}

/// Build the ETM ds:Signature block (indented as the last child of
/// DCinemaSecurityMessage), signing over the two authenticated elements.
///
/// Canonicalization uses `xmllint --c14n` (libxml2, the same engine xmlsec1
/// verifies with) so the digested bytes match to the byte. Signing itself uses
/// the `rsa` crate. Every failure is fatal: a KDM must never be written with an
/// absent, empty or placeholder signature.
fn build_signature(
    config: &KdmConfig,
    auth_public_inner: &str,
    auth_private_inner: &str,
) -> Result<String, String> {
    use base64::Engine;

    if config.signer_key_file.as_os_str().is_empty() {
        return Err("signer private key is required to sign the KDM".into());
    }

    // Load the signing key and prove it belongs to the signer certificate.
    let leaf_public_key = cert_rsa_public_key(&config.signer_cert_file)?;
    let private_key = load_signer_key(&config.signer_key_file, &leaf_public_key)?;

    // Digest each authenticated element over its canonical form. The fragment
    // apex carries the root's in-scope namespaces (ETM default + ds); under
    // inclusive c14n those are rendered on the apex of every digested subtree,
    // so this matches what the verifier computes in place.
    let auth_public_fragment = format!(
        r#"<AuthenticatedPublic xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}" Id="{AUTH_PUBLIC_ID}">{auth_public_inner}</AuthenticatedPublic>"#,
    );
    let auth_private_fragment = format!(
        r#"<AuthenticatedPrivate xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}" Id="{AUTH_PRIVATE_ID}">{auth_private_inner}</AuthenticatedPrivate>"#,
    );
    let public_digest =
        base64::engine::general_purpose::STANDARD.encode(sha256(&c14n(&auth_public_fragment)?));
    let private_digest =
        base64::engine::general_purpose::STANDARD.encode(sha256(&c14n(&auth_private_fragment)?));

    // SignedInfo inner content, reused for the digest input and the document.
    let signed_info_inner = format!(
        r##"
      <ds:CanonicalizationMethod Algorithm="{C14N_METHOD}"/>
      <ds:SignatureMethod Algorithm="{SIG_METHOD}"/>
      <ds:Reference URI="#{AUTH_PUBLIC_ID}">
        <ds:DigestMethod Algorithm="{DIGEST_METHOD}"/>
        <ds:DigestValue>{public_digest}</ds:DigestValue>
      </ds:Reference>
      <ds:Reference URI="#{AUTH_PRIVATE_ID}">
        <ds:DigestMethod Algorithm="{DIGEST_METHOD}"/>
        <ds:DigestValue>{private_digest}</ds:DigestValue>
      </ds:Reference>
    "##,
    );

    // Canonicalize SignedInfo in its in-scope namespace context and sign it.
    let signed_info_fragment = format!(
        r#"<ds:SignedInfo xmlns="{ETM_NS}" xmlns:ds="{DSIG_NS}">{signed_info_inner}</ds:SignedInfo>"#,
    );
    let signed_info_c14n = c14n(&signed_info_fragment)?;
    let signature_bytes = private_key
        .sign(
            rsa::Pkcs1v15Sign::new::<sha2::Sha256>(),
            &sha256(&signed_info_c14n),
        )
        .map_err(|e| format!("RSA signing of SignedInfo failed: {e}"))?;
    let signature_value = base64::engine::general_purpose::STANDARD.encode(&signature_bytes);

    // KeyInfo: one X509Data per certificate, leaf first up to the root, each
    // carrying both the issuer/serial reference and the certificate itself.
    let mut chain = vec![config.signer_cert_file.clone()];
    chain.extend(config.signer_chain_files.iter().cloned());
    let mut key_info = String::new();
    for cert_path in &chain {
        let meta = cert_key_info(cert_path)?;
        key_info.push_str(&format!(
            r#"
      <ds:X509Data>
        <ds:X509IssuerSerial>
          <ds:X509IssuerName>{issuer}</ds:X509IssuerName>
          <ds:X509SerialNumber>{serial}</ds:X509SerialNumber>
        </ds:X509IssuerSerial>
        <ds:X509Certificate>{cert}</ds:X509Certificate>
      </ds:X509Data>"#,
            issuer = xml_escape(&meta.issuer_dn),
            serial = meta.serial,
            cert = meta.der_base64,
        ));
    }

    Ok(format!(
        r#"  <ds:Signature>
    <ds:SignedInfo>{signed_info_inner}</ds:SignedInfo>
    <ds:SignatureValue>{signature_value}</ds:SignatureValue>
    <ds:KeyInfo>{key_info}
    </ds:KeyInfo>
  </ds:Signature>"#,
    ))
}

/// SHA-256 digest.
fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(data).to_vec()
}

/// Inclusive Canonical XML 1.0 of a fragment, via `xmllint`.
///
/// libxml2 is the engine xmlsec1 canonicalizes with, so its output matches to
/// the byte. Missing or failing xmllint is fatal: without correct c14n the
/// signature cannot be made verifiable, and a KDM must never be emitted with a
/// signature that only looks valid.
fn c14n(fragment: &str) -> Result<Vec<u8>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("xmllint")
        .arg("--c14n")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "cannot run xmllint (libxml2) for XML canonicalization, required to sign the KDM: {e}"
            )
        })?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(fragment.as_bytes())
        .map_err(|e| format!("failed to send XML to xmllint: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("xmllint did not complete: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "xmllint canonicalization failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// Load an RSA private key (PKCS#8 or PKCS#1 PEM) and confirm it matches the
/// signer certificate's public key.
///
/// A key that is missing, unreadable, not RSA, or belonging to a different
/// certificate is fatal: signing with the wrong key yields a KDM that no
/// verifier will accept.
fn load_signer_key(
    key_path: &Path,
    cert_public_key: &rsa::RsaPublicKey,
) -> Result<rsa::RsaPrivateKey, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;

    let pem = std::fs::read_to_string(key_path)
        .map_err(|e| format!("cannot read signer private key {}: {e}", key_path.display()))?;
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(&pem))
        .map_err(|e| {
            format!(
                "signer private key {} is not a valid RSA private key (PKCS#8 or PKCS#1 PEM): {e}",
                key_path.display()
            )
        })?;
    if &key.to_public_key() != cert_public_key {
        return Err(format!(
            "signer private key {} does not match the public key in the signer certificate",
            key_path.display()
        ));
    }
    Ok(key)
}

/// Certificate fields needed for one ds:KeyInfo/X509Data entry.
struct CertKeyInfo {
    issuer_dn: String,
    serial: String,
    der_base64: String,
}

/// Parse a certificate for its issuer, serial and DER, for ds:KeyInfo.
fn cert_key_info(cert_path: &Path) -> Result<CertKeyInfo, String> {
    use base64::Engine;
    use x509_parser::prelude::*;

    let data = std::fs::read(cert_path)
        .map_err(|e| format!("cannot read certificate {}: {e}", cert_path.display()))?;
    let (_, pem) = parse_x509_pem(&data)
        .map_err(|e| format!("certificate {} is not valid PEM: {e}", cert_path.display()))?;
    let cert = pem.parse_x509().map_err(|e| {
        format!(
            "certificate {} is not valid X.509: {e}",
            cert_path.display()
        )
    })?;

    Ok(CertKeyInfo {
        issuer_dn: cert.issuer().to_string(),
        // X509SerialNumber is a decimal integer in XML-DSig.
        serial: cert.serial.to_str_radix(10),
        der_base64: base64::engine::general_purpose::STANDARD.encode(&pem.contents),
    })
}

/// Extract the RSA public key from a certificate, rejecting non-RSA keys.
fn cert_rsa_public_key(cert_path: &Path) -> Result<rsa::RsaPublicKey, String> {
    use rsa::pkcs8::DecodePublicKey;
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

    match cert.public_key().parsed() {
        Ok(x509_parser::public_key::PublicKey::RSA(_)) => {}
        Ok(_) => {
            return Err(format!(
                "signer cert {} does not hold an RSA key; SMPTE 430-3 signatures require RSA",
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
    rsa::RsaPublicKey::from_public_key_der(cert.public_key().raw).map_err(|e| {
        format!(
            "cannot load RSA public key from {}: {e}",
            cert_path.display()
        )
    })
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

/// Certificate thumbprint per SMPTE ST 430-2: SHA-1 over the DER-encoded
/// TBSCertificate (the signed portion), not the whole certificate.
///
/// Matches libdcp's `Certificate::thumbprint()`, which hashes
/// `i2d_re_X509_tbs` output.
fn cert_thumbprint(tbs_der: &[u8]) -> [u8; 20] {
    use sha1::Digest;
    sha1::Sha1::digest(tbs_der).into()
}

/// Identity of the entity issuing a KDM.
struct Signer {
    subject_dn: String,
    issuer_dn: String,
    serial: String,
    thumbprint: [u8; 20],
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
        subject_dn: cert.subject().to_string(),
        issuer_dn: cert.issuer().to_string(),
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

    Ok(Recipient {
        subject_dn: cert.subject().to_string(),
        issuer_dn: cert.issuer().to_string(),
        // X509SerialNumber is a decimal integer in XML-DSig
        serial: cert.serial.to_str_radix(10),
        public_key,
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
        /// Same subject CN as `root`, different key. Used to prove chain
        /// validation checks signatures and not just names.
        impostor_root: PathBuf,
    }

    fn fixtures() -> &'static Fixtures {
        static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
        FIXTURES.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let p = dir.path();
            assert_eq!(generate_chain("Acme", p), 0, "chain generation failed");

            let impostor_root = p.join("impostor_root.pem");
            let opts = CertOptions {
                cert_type: CertType::Root,
                // identical CN to the genuine root
                common_name: "Acme Root CA".to_string(),
                organization: "Acme".to_string(),
                organizational_unit: "Digital Cinema".to_string(),
                output_cert: impostor_root.clone(),
                output_key: p.join("impostor_root.key"),
                ..Default::default()
            };
            assert_eq!(generate_certificate(&opts), 0, "impostor root failed");

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

    // Signs with the self-signed root, so KeyInfo needs no chain and the
    // recipient stays the signer leaf (its key decrypts the CipherValue in the
    // cipher round-trip tests).
    fn test_config(f: &Fixtures, out: PathBuf) -> KdmConfig {
        KdmConfig {
            cpl_id: "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string(),
            content_title: "Test Feature".to_string(),
            recipient_cert_file: f.signer.clone(),
            signer_cert_file: f.root.clone(),
            signer_key_file: f.root_key.clone(),
            signer_chain_files: vec![],
            output_file: out,
            valid_from: "now".to_string(),
            valid_to: "7 days".to_string(),
            formulation: "dci-any".to_string(),
        }
    }

    // Realistic signer: the leaf signs, KeyInfo embeds leaf + intermediate +
    // root, and a verifier trusts the root. Recipient is the root cert (any
    // 2048-bit RSA cert works, its key is not needed here).
    fn chain_signed_config(f: &Fixtures, out: PathBuf) -> KdmConfig {
        KdmConfig {
            cpl_id: "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string(),
            content_title: "Test Feature".to_string(),
            recipient_cert_file: f.root.clone(),
            signer_cert_file: f.signer.clone(),
            signer_key_file: f.signer_key.clone(),
            signer_chain_files: vec![f.intermediate.clone(), f.root.clone()],
            output_file: out,
            valid_from: "now".to_string(),
            valid_to: "7 days".to_string(),
            formulation: "dci-any".to_string(),
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
        // The impostor shares the real root's CN, so the old name-comparison
        // check passed this. Signature verification must reject it.
        let f = fixtures();
        let chain = vec![
            f.signer.clone(),
            f.intermediate.clone(),
            f.impostor_root.clone(),
        ];
        assert_eq!(
            validate_chain(&chain),
            -1,
            "a root that did not sign the intermediate must be rejected"
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
}
