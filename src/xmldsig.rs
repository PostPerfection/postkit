//! Reusable enveloped XML digital signatures.
//!
//! One signing implementation for the whole crate, using the SMPTE 430-3 KDM
//! profile: inclusive Canonical XML 1.0, RSASSA-PKCS1-v1_5 over SHA-256, and
//! SHA-256 reference digests. The KDM signer in `certificate.rs` builds its
//! document body then calls `sign_enveloped`, so there is no second signer.
//!
//! Verification reads the declared DigestMethod (per Reference) and
//! SignatureMethod (in SignedInfo) and dispatches on them, so it also accepts
//! real SHA-1/rsa-sha1 DCPs (Interop and older SMPTE tools), not only the
//! SHA-256 we sign with. An unsupported algorithm is a hard error.
//!
//! Digests and SignedInfo are canonicalized by extracting the target element's
//! subtree from the document and injecting the namespaces in scope at that
//! element onto its apex. Under inclusive c14n a subtree node-set renders all
//! in-scope namespaces on its apex, so this matches byte-for-byte what a
//! verifier (xmlsec1/libxml2) computes in place. Referenced elements must not
//! contain the inserted ds:Signature, which is the standard enveloped rule; the
//! signature goes in as the last child of the root or a caller-named parent, a
//! sibling of the referenced elements.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// XML-DSig namespace.
pub(crate) const DSIG_NS: &str = "http://www.w3.org/2000/09/xmldsig#";
/// Inclusive Canonical XML 1.0, WithComments.
pub(crate) const C14N_METHOD: &str = "http://www.w3.org/TR/2001/REC-xml-c14n-20010315#WithComments";
/// Enveloped-signature transform: digest the document with ds:Signature removed.
pub(crate) const ENVELOPED_TRANSFORM: &str =
    "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
/// RSASSA-PKCS1-v1_5 over SHA-256.
pub(crate) const SIG_METHOD: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
/// SHA-256 digest.
pub(crate) const DIGEST_METHOD: &str = "http://www.w3.org/2001/04/xmlenc#sha256";

/// The identity that signs: leaf certificate, its RSA private key, and the CA
/// certificates above the leaf (intermediate(s) then root). All are embedded in
/// ds:KeyInfo, leaf first, so a verifier can build the chain to a trust anchor.
pub struct XmlSigner {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub chain_files: Vec<PathBuf>,
}

/// Sign an enveloped XML document, inserting a ds:Signature over the elements
/// named by `reference_ids` (matched on their `id_attr` attribute).
///
/// The signature is inserted as the last child of the document root, or of the
/// element whose `id_attr` equals `parent_id` when given. Every failure is
/// fatal: a signed document is never returned with an absent or placeholder
/// signature.
pub fn sign_enveloped(
    xml: &str,
    reference_ids: &[&str],
    id_attr: &str,
    parent_id: Option<&str>,
    signer: &XmlSigner,
) -> Result<String, String> {
    // Public API signs with the SMPTE 430-3 profile (SHA-256 throughout).
    sign_enveloped_with(
        xml,
        reference_ids,
        id_attr,
        parent_id,
        signer,
        DigestAlg::Sha256,
        RsaSigAlg::Sha256,
    )
}

/// Internal by-Id signer parameterized by digest and signature algorithm. The
/// public `sign_enveloped` fixes both at SHA-256; tests exercise other pairs.
fn sign_enveloped_with(
    xml: &str,
    reference_ids: &[&str],
    id_attr: &str,
    parent_id: Option<&str>,
    signer: &XmlSigner,
    digest_alg: DigestAlg,
    sig_alg: RsaSigAlg,
) -> Result<String, String> {
    if reference_ids.is_empty() {
        return Err("at least one reference id is required to sign".into());
    }
    if signer.cert_file.as_os_str().is_empty() {
        return Err("signer certificate is required to sign".into());
    }
    if signer.key_file.as_os_str().is_empty() {
        return Err("signer private key is required to sign".into());
    }

    // One ds:Reference per referenced element, digested over its canonical form.
    let mut references = String::new();
    for id in reference_ids {
        let fragment = find_element_fragment(xml, &|e| element_has_id(e, id_attr, id))?
            .ok_or_else(|| format!("no element with {id_attr}=\"{id}\" to sign"))?;
        let digest = b64(&digest_alg.digest(&c14n(&fragment)?));
        references.push_str(&format!(
            r##"
      <ds:Reference URI="#{id}">
        <ds:DigestMethod Algorithm="{}"/>
        <ds:DigestValue>{digest}</ds:DigestValue>
      </ds:Reference>"##,
            digest_alg.uri(),
        ));
    }

    let (element, insert_offset) =
        build_signature_element(xml, &references, parent_id, id_attr, signer, sig_alg)?;

    // Insert as the last child of the parent, indented like its siblings. The
    // by-Id digests are over the referenced elements, so this surrounding
    // whitespace does not affect them.
    let mut out = String::with_capacity(xml.len() + element.len() + 3);
    out.push_str(&xml[..insert_offset]);
    out.push_str("  ");
    out.push_str(&element);
    out.push('\n');
    out.push_str(&xml[insert_offset..]);
    Ok(out)
}

/// Sign an XML document with the standard whole-document enveloped signature
/// profile (W3C XML-DSig, SMPTE 2067-3 IMF CPL/PKL): one `<ds:Reference URI="">`
/// whose transforms are enveloped-signature then inclusive C14N, digesting the
/// entire document with the ds:Signature removed.
///
/// No Id attributes are added, so a verifier needs no `--id-attr` hints. The
/// ds:Signature is inserted as the last child of the document root with no
/// surrounding whitespace: since it is the last child, "document minus Signature"
/// then equals the original unsigned document, and the reference digest is over
/// `c14n(unsigned document root)`.
pub fn sign_document_enveloped(xml: &str, signer: &XmlSigner) -> Result<String, String> {
    sign_document_enveloped_with(xml, signer, DigestAlg::Sha256, RsaSigAlg::Sha256)
}

/// Internal whole-document signer parameterized by digest and signature
/// algorithm. The public `sign_document_enveloped` fixes both at SHA-256.
fn sign_document_enveloped_with(
    xml: &str,
    signer: &XmlSigner,
    digest_alg: DigestAlg,
    sig_alg: RsaSigAlg,
) -> Result<String, String> {
    // The reference digest is over the whole document with ds:Signature removed.
    // Before signing there is no ds:Signature, and c14n drops the XML
    // declaration and whitespace outside the root, so this is c14n of the root.
    let root_fragment =
        find_element_fragment(xml, &|_| Ok(true))?.ok_or("document has no root element to sign")?;
    let digest = b64(&digest_alg.digest(&c14n(&root_fragment)?));

    let references = format!(
        r##"
      <ds:Reference URI="">
        <ds:Transforms>
          <ds:Transform Algorithm="{ENVELOPED_TRANSFORM}"/>
          <ds:Transform Algorithm="{C14N_METHOD}"/>
        </ds:Transforms>
        <ds:DigestMethod Algorithm="{}"/>
        <ds:DigestValue>{digest}</ds:DigestValue>
      </ds:Reference>"##,
        digest_alg.uri(),
    );

    let (element, insert_offset) =
        build_signature_element(xml, &references, None, "Id", signer, sig_alg)?;

    // Insert with no surrounding whitespace so removing the Signature element
    // restores the original document byte-for-byte, matching the digest input.
    let mut out = String::with_capacity(xml.len() + element.len());
    out.push_str(&xml[..insert_offset]);
    out.push_str(&element);
    out.push_str(&xml[insert_offset..]);
    Ok(out)
}

/// Build the ds:Signature element (no surrounding whitespace) and the byte offset
/// where it is inserted, given the caller-built ds:Reference block(s). Shared by
/// the by-Id and whole-document enveloped signers.
fn build_signature_element(
    xml: &str,
    references: &str,
    parent_id: Option<&str>,
    id_attr: &str,
    signer: &XmlSigner,
    sig_alg: RsaSigAlg,
) -> Result<(String, usize), String> {
    if signer.cert_file.as_os_str().is_empty() {
        return Err("signer certificate is required to sign".into());
    }
    if signer.key_file.as_os_str().is_empty() {
        return Err("signer private key is required to sign".into());
    }

    // Load the signing key and prove it belongs to the signer certificate.
    let leaf_public_key = cert_rsa_public_key(&signer.cert_file)?;
    let private_key = load_signer_key(&signer.key_file, &leaf_public_key)?;

    // Where the signature is inserted, and the namespaces in scope there.
    let (parent_scope, insert_offset) = find_parent(xml, parent_id, id_attr)?;

    // Reuse an in-scope ds prefix if present, otherwise declare it on Signature.
    let ds_in_scope = parent_scope.iter().any(|(p, u)| p == "ds" && u == DSIG_NS);
    let signature_open = if ds_in_scope {
        "<ds:Signature>".to_string()
    } else {
        format!(r#"<ds:Signature xmlns:ds="{DSIG_NS}">"#)
    };

    let signed_info_inner = format!(
        r#"
      <ds:CanonicalizationMethod Algorithm="{C14N_METHOD}"/>
      <ds:SignatureMethod Algorithm="{}"/>{references}
    "#,
        sig_alg.uri(),
    );

    // SignedInfo is canonicalized in the namespace context it will have in the
    // document: the parent's in-scope namespaces plus the ds prefix. Under
    // inclusive c14n those all render on the SignedInfo apex, matching what the
    // verifier digests in place.
    let mut context = parent_scope.clone();
    if !ds_in_scope {
        context.push(("ds".to_string(), DSIG_NS.to_string()));
    }
    context.sort_by(|a, b| a.0.cmp(&b.0));
    let mut ns_decls = String::new();
    for (prefix, uri) in &context {
        if prefix.is_empty() {
            ns_decls.push_str(&format!(r#" xmlns="{}""#, escape_attr(uri)));
        } else {
            ns_decls.push_str(&format!(r#" xmlns:{prefix}="{}""#, escape_attr(uri)));
        }
    }
    let signed_info_fragment =
        format!("<ds:SignedInfo{ns_decls}>{signed_info_inner}</ds:SignedInfo>");
    let signed_info_c14n = c14n(&signed_info_fragment)?;
    let signature_bytes = sig_alg
        .sign(&private_key, &signed_info_c14n)
        .map_err(|e| format!("RSA signing of SignedInfo failed: {e}"))?;
    let signature_value = b64(&signature_bytes);

    // KeyInfo: one X509Data per certificate, leaf first up to the root.
    let mut chain = vec![signer.cert_file.clone()];
    chain.extend(signer.chain_files.iter().cloned());
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
            issuer = crate::packaging::escape_xml(&meta.issuer_dn),
            serial = meta.serial,
            cert = meta.der_base64,
        ));
    }

    let element = format!(
        r#"{signature_open}
    <ds:SignedInfo>{signed_info_inner}</ds:SignedInfo>
    <ds:SignatureValue>{signature_value}</ds:SignatureValue>
    <ds:KeyInfo>{key_info}
    </ds:KeyInfo>
  </ds:Signature>"#,
    );
    Ok((element, insert_offset))
}

/// Verify an enveloped ds:Signature: recompute every reference digest, then
/// check the RSA signature over SignedInfo against the signing certificate.
///
/// The verifying key is the embedded leaf certificate's, unless `trusted_cert`
/// is given, in which case the embedded leaf must equal it and its key is used.
/// A digest mismatch, a bad signature, or a mismatched trusted cert is an error;
/// this checks the cryptography, not certificate validity dates.
pub fn verify_enveloped(
    xml: &str,
    id_attr: &str,
    trusted_cert: Option<&Path>,
) -> Result<(), String> {
    let signed_info_fragment =
        find_element_fragment(xml, &|e| Ok(e.local_name().as_ref() == b"SignedInfo"))?
            .ok_or("document has no ds:SignedInfo")?;
    let signed_info_c14n = c14n(&signed_info_fragment)?;

    let parsed = parse_signature(xml)?;
    if parsed.references.is_empty() {
        return Err("signature has no ds:Reference".into());
    }

    // Every referenced element must digest, under its declared DigestMethod, to
    // the value in its Reference.
    for reference in &parsed.references {
        let id = &reference.id;
        let fragment = find_element_fragment(xml, &|e| element_has_id(e, id_attr, id))?
            .ok_or_else(|| format!("reference #{id} resolves to no element"))?;
        let actual = b64(&reference.digest_alg.digest(&c14n(&fragment)?));
        if actual != reference.digest_b64 {
            return Err(format!("digest mismatch for reference #{id}"));
        }
    }

    verify_signed_info(&parsed, &signed_info_c14n, trusted_cert)
}

/// Verify a standard whole-document enveloped ds:Signature (`URI=""`,
/// enveloped-signature + C14N transforms): detach the ds:Signature, recompute the
/// document digest over the remaining canonical document, then check the RSA
/// signature over SignedInfo. Needs no `--id-attr` hints, unlike the by-Id mode.
pub fn verify_document_enveloped(xml: &str, trusted_cert: Option<&Path>) -> Result<(), String> {
    let signed_info_fragment =
        find_element_fragment(xml, &|e| Ok(e.local_name().as_ref() == b"SignedInfo"))?
            .ok_or("document has no ds:SignedInfo")?;
    let signed_info_c14n = c14n(&signed_info_fragment)?;

    let parsed = parse_signature(xml)?;
    if parsed.references.is_empty() {
        return Err("signature has no ds:Reference".into());
    }

    // Enveloped-signature transform: digest the document with ds:Signature gone.
    // It is the last child inserted with no surrounding whitespace, so removing
    // it restores the original document, whose c14n is the c14n of its root.
    let (start, end) = find_signature_span(xml)?;
    let mut unsigned = String::with_capacity(xml.len() - (end - start));
    unsigned.push_str(&xml[..start]);
    unsigned.push_str(&xml[end..]);
    let root_fragment =
        find_element_fragment(&unsigned, &|_| Ok(true))?.ok_or("document has no root element")?;
    let reference = &parsed.references[0];
    let actual = b64(&reference.digest_alg.digest(&c14n(&root_fragment)?));
    if actual != reference.digest_b64 {
        return Err("digest mismatch for the enveloped document reference".into());
    }

    verify_signed_info(&parsed, &signed_info_c14n, trusted_cert)
}

/// Check the RSA signature over the canonical SignedInfo against the embedded
/// leaf certificate, or the pinned trusted certificate when given.
fn verify_signed_info(
    parsed: &ParsedSignature,
    signed_info_c14n: &[u8],
    trusted_cert: Option<&Path>,
) -> Result<(), String> {
    let embedded_key = rsa_public_key_from_der(&parsed.leaf_cert_der)?;
    let verify_key = match trusted_cert {
        Some(path) => {
            let trusted_der = cert_der(path)?;
            if trusted_der != parsed.leaf_cert_der {
                return Err(
                    "embedded signing certificate does not match the trusted certificate".into(),
                );
            }
            embedded_key
        }
        None => embedded_key,
    };

    parsed
        .signature_method
        .verify(&verify_key, signed_info_c14n, &parsed.signature_value)
        .map_err(|e| format!("signature verification failed: {e}"))?;
    Ok(())
}

/// Byte range of the ds:Signature element (from '<' through its end tag).
fn find_signature_span(xml: &str) -> Result<(usize, usize), String> {
    let mut reader = Reader::from_str(xml);
    loop {
        let start = reader.buffer_position() as usize;
        match reader
            .read_event()
            .map_err(|e| format!("document is not valid XML: {e}"))?
        {
            Event::Start(e) if e.local_name().as_ref() == b"Signature" => {
                reader
                    .read_to_end(e.name().to_owned())
                    .map_err(|err| format!("cannot read ds:Signature subtree: {err}"))?;
                let end = reader.buffer_position() as usize;
                return Ok((start, end));
            }
            Event::Eof => return Err("document has no ds:Signature".into()),
            _ => {}
        }
    }
}

/// Namespace declarations (prefix, uri) an element carries; "" is the default.
fn element_namespace_decls(e: &BytesStart) -> Result<Vec<(String, String)>, String> {
    let mut decls = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|err| format!("cannot read an attribute: {err}"))?;
        let key = std::str::from_utf8(attr.key.as_ref())
            .map_err(|err| format!("attribute name is not UTF-8: {err}"))?;
        let value = attr
            .unescape_value()
            .map_err(|err| format!("cannot unescape an attribute value: {err}"))?
            .into_owned();
        if key == "xmlns" {
            decls.push((String::new(), value));
        } else if let Some(prefix) = key.strip_prefix("xmlns:") {
            decls.push((prefix.to_string(), value));
        }
    }
    Ok(decls)
}

/// True if `e` carries `id_attr` equal to `id`.
fn element_has_id(e: &BytesStart, id_attr: &str, id: &str) -> Result<bool, String> {
    for attr in e.attributes() {
        let attr = attr.map_err(|err| format!("cannot read an attribute: {err}"))?;
        if attr.key.as_ref() == id_attr.as_bytes() {
            let value = attr
                .unescape_value()
                .map_err(|err| format!("cannot unescape an attribute value: {err}"))?;
            return Ok(value == id);
        }
    }
    Ok(false)
}

/// The namespaces in scope for a child of the elements in `stack` (innermost
/// last), with unset defaults (xmlns="") removed.
fn effective_scope(stack: &[Vec<(String, String)>]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for frame in stack {
        for (prefix, uri) in frame {
            if uri.is_empty() {
                map.remove(prefix);
            } else {
                map.insert(prefix.clone(), uri.clone());
            }
        }
    }
    map
}

/// Namespaces inherited from ancestors at an element (those in `stack`, which
/// excludes the element itself), minus prefixes the element redeclares.
fn inherited_scope(
    stack: &[Vec<(String, String)>],
    own: &[(String, String)],
) -> Vec<(String, String)> {
    let mut map = effective_scope(stack);
    for (prefix, _) in own {
        map.remove(prefix);
    }
    map.into_iter().collect()
}

/// Inject `inherited` namespace declarations onto the apex of a serialized
/// subtree so it canonicalizes as an in-place node-set would.
fn inject_namespaces(subtree: &str, inherited: &[(String, String)]) -> Result<String, String> {
    let bytes = subtree.as_bytes();
    if bytes.first() != Some(&b'<') {
        return Err("subtree does not start with an element".into());
    }
    // Element name runs from just after '<' to the first whitespace, '/' or '>'.
    let mut i = 1;
    while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>') {
        i += 1;
    }
    let mut decls = inherited.to_vec();
    decls.sort_by(|a, b| a.0.cmp(&b.0));
    let mut injection = String::new();
    for (prefix, uri) in &decls {
        if prefix.is_empty() {
            injection.push_str(&format!(r#" xmlns="{}""#, escape_attr(uri)));
        } else {
            injection.push_str(&format!(r#" xmlns:{prefix}="{}""#, escape_attr(uri)));
        }
    }
    Ok(format!("{}{}{}", &subtree[..i], injection, &subtree[i..]))
}

/// Serialize the first element matching `matcher` as a c14n-ready fragment: its
/// subtree from the source with in-scope namespaces injected on the apex.
fn find_element_fragment(
    xml: &str,
    matcher: &dyn Fn(&BytesStart) -> Result<bool, String>,
) -> Result<Option<String>, String> {
    let mut reader = Reader::from_str(xml);
    let mut ns_stack: Vec<Vec<(String, String)>> = Vec::new();

    loop {
        let start = reader.buffer_position() as usize;
        match reader
            .read_event()
            .map_err(|e| format!("document is not valid XML: {e}"))?
        {
            Event::Start(e) => {
                if matcher(&e)? {
                    let inherited = inherited_scope(&ns_stack, &element_namespace_decls(&e)?);
                    reader
                        .read_to_end(e.name().to_owned())
                        .map_err(|err| format!("cannot read element subtree: {err}"))?;
                    let end = reader.buffer_position() as usize;
                    return Ok(Some(inject_namespaces(&xml[start..end], &inherited)?));
                }
                ns_stack.push(element_namespace_decls(&e)?);
            }
            Event::Empty(e) => {
                if matcher(&e)? {
                    let inherited = inherited_scope(&ns_stack, &element_namespace_decls(&e)?);
                    let end = reader.buffer_position() as usize;
                    return Ok(Some(inject_namespaces(&xml[start..end], &inherited)?));
                }
            }
            Event::End(_) => {
                ns_stack.pop();
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

/// The in-scope namespaces of the insertion parent and the byte offset just
/// before its end tag, where the signature is inserted as its last child.
fn find_parent(
    xml: &str,
    parent_id: Option<&str>,
    id_attr: &str,
) -> Result<(Vec<(String, String)>, usize), String> {
    let mut reader = Reader::from_str(xml);
    let mut ns_stack: Vec<Vec<(String, String)>> = Vec::new();
    let mut found_depth: Option<usize> = None;
    let mut parent_scope: Vec<(String, String)> = Vec::new();

    loop {
        let start = reader.buffer_position() as usize;
        match reader
            .read_event()
            .map_err(|e| format!("document is not valid XML: {e}"))?
        {
            Event::Start(e) => {
                ns_stack.push(element_namespace_decls(&e)?);
                if found_depth.is_none() {
                    let is_parent = match parent_id {
                        None => true,
                        Some(pid) => element_has_id(&e, id_attr, pid)?,
                    };
                    if is_parent {
                        found_depth = Some(ns_stack.len());
                        parent_scope = effective_scope(&ns_stack).into_iter().collect();
                    }
                }
            }
            Event::Empty(e) => {
                if found_depth.is_none() {
                    let is_parent = match parent_id {
                        None => true,
                        Some(pid) => element_has_id(&e, id_attr, pid)?,
                    };
                    if is_parent {
                        return Err("cannot insert a signature into a self-closing element".into());
                    }
                }
            }
            Event::End(_) => {
                if found_depth == Some(ns_stack.len()) {
                    return Ok((parent_scope, start));
                }
                ns_stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Err(match parent_id {
        None => "document has no root element".into(),
        Some(pid) => format!("no element with {id_attr}=\"{pid}\" to hold the signature"),
    })
}

/// One ds:Reference a verifier must recompute.
struct ParsedReference {
    /// Reference id without the leading '#' (empty for a whole-document URI="").
    id: String,
    /// Base64 digest value declared in the reference.
    digest_b64: String,
    /// The reference's declared DigestMethod.
    digest_alg: DigestAlg,
}

/// The pieces of a ds:Signature a verifier needs.
struct ParsedSignature {
    signature_value: Vec<u8>,
    /// The SignedInfo's declared SignatureMethod.
    signature_method: RsaSigAlg,
    references: Vec<ParsedReference>,
    /// DER of the first embedded X509Certificate (the signing leaf).
    leaf_cert_der: Vec<u8>,
}

/// Parse the ds:Signature block: its references, SignatureValue and leaf cert.
fn parse_signature(xml: &str) -> Result<ParsedSignature, String> {
    let mut reader = Reader::from_str(xml);
    let mut in_signature = false;
    let mut current_ref: Option<String> = None;
    let mut collecting: Option<&'static str> = None;
    let mut buffer = String::new();

    let mut references = Vec::new();
    let mut signature_value = None;
    let mut signature_method = None;
    let mut current_digest_alg: Option<DigestAlg> = None;
    let mut leaf_cert = None;

    loop {
        match reader
            .read_event()
            .map_err(|e| format!("document is not valid XML: {e}"))?
        {
            // SignatureMethod and DigestMethod are empty elements carrying only
            // an Algorithm attribute; handle both the self-closing and the rare
            // explicit-end form.
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"Signature" => in_signature = true,
                b"SignatureMethod" if in_signature => {
                    signature_method = Some(RsaSigAlg::from_uri(&read_algorithm_attr(&e)?)?);
                }
                b"Reference" if in_signature => {
                    let mut uri = None;
                    for attr in e.attributes() {
                        let attr =
                            attr.map_err(|err| format!("cannot read an attribute: {err}"))?;
                        if attr.key.as_ref() == b"URI" {
                            uri = Some(
                                attr.unescape_value()
                                    .map_err(|err| format!("cannot unescape URI: {err}"))?
                                    .into_owned(),
                            );
                        }
                    }
                    current_ref = uri.map(|u| u.trim().trim_start_matches('#').to_string());
                    current_digest_alg = None;
                }
                b"DigestMethod" if in_signature => {
                    current_digest_alg = Some(DigestAlg::from_uri(&read_algorithm_attr(&e)?)?);
                }
                b"DigestValue" if in_signature => {
                    collecting = Some("digest");
                    buffer.clear();
                }
                b"SignatureValue" if in_signature => {
                    collecting = Some("signature");
                    buffer.clear();
                }
                b"X509Certificate" if in_signature && leaf_cert.is_none() => {
                    collecting = Some("cert");
                    buffer.clear();
                }
                _ => {}
            },
            Event::Text(e) if collecting.is_some() => {
                let text = e
                    .unescape()
                    .map_err(|err| format!("signature text is not valid XML: {err}"))?;
                buffer.push_str(&text);
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"Signature" => in_signature = false,
                b"Reference" => current_ref = None,
                b"DigestValue" if collecting == Some("digest") => {
                    let id = current_ref
                        .clone()
                        .ok_or("ds:DigestValue outside a Reference with a URI")?;
                    let digest_alg = current_digest_alg
                        .ok_or("ds:Reference has no ds:DigestMethod before its DigestValue")?;
                    references.push(ParsedReference {
                        id,
                        digest_b64: buffer.split_whitespace().collect(),
                        digest_alg,
                    });
                    collecting = None;
                }
                b"SignatureValue" if collecting == Some("signature") => {
                    let text: String = buffer.split_whitespace().collect();
                    signature_value = Some(b64d(&text)?);
                    collecting = None;
                }
                b"X509Certificate" if collecting == Some("cert") => {
                    let text: String = buffer.split_whitespace().collect();
                    leaf_cert = Some(b64d(&text)?);
                    collecting = None;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(ParsedSignature {
        signature_value: signature_value.ok_or("signature has no ds:SignatureValue")?,
        signature_method: signature_method.ok_or("SignedInfo has no ds:SignatureMethod")?,
        references,
        leaf_cert_der: leaf_cert.ok_or("signature has no ds:X509Certificate")?,
    })
}

/// Read the required `Algorithm` attribute off a DigestMethod/SignatureMethod.
fn read_algorithm_attr(e: &BytesStart) -> Result<String, String> {
    for attr in e.attributes() {
        let attr = attr.map_err(|err| format!("cannot read an attribute: {err}"))?;
        if attr.key.as_ref() == b"Algorithm" {
            return Ok(attr
                .unescape_value()
                .map_err(|err| format!("cannot unescape Algorithm: {err}"))?
                .into_owned());
        }
    }
    Err("method element has no Algorithm attribute".into())
}

/// Extract the RSA public key from a DER-encoded certificate.
fn rsa_public_key_from_der(der: &[u8]) -> Result<rsa::RsaPublicKey, String> {
    use rsa::pkcs8::DecodePublicKey;
    use x509_parser::prelude::*;

    let (_, cert) = X509Certificate::from_der(der)
        .map_err(|e| format!("embedded certificate is not valid X.509: {e}"))?;
    match cert.public_key().parsed() {
        Ok(x509_parser::public_key::PublicKey::RSA(_)) => {}
        Ok(_) => return Err("embedded certificate does not hold an RSA key".into()),
        Err(e) => return Err(format!("cannot parse embedded public key: {e}")),
    }
    rsa::RsaPublicKey::from_public_key_der(cert.public_key().raw)
        .map_err(|e| format!("cannot load embedded RSA public key: {e}"))
}

/// Read a PEM certificate and return its DER bytes.
fn cert_der(path: &Path) -> Result<Vec<u8>, String> {
    use x509_parser::prelude::*;
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read certificate {}: {e}", path.display()))?;
    let (_, pem) = parse_x509_pem(&data)
        .map_err(|e| format!("certificate {} is not valid PEM: {e}", path.display()))?;
    Ok(pem.contents)
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn b64d(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|e| format!("value is not valid base64: {e}"))
}

/// A supported reference DigestMethod. Real DCPs are signed with SHA-1 (Interop
/// and older SMPTE tools); SHA-256 is the KDM profile this crate signs with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DigestAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl DigestAlg {
    fn from_uri(uri: &str) -> Result<Self, String> {
        match uri {
            "http://www.w3.org/2000/09/xmldsig#sha1" => Ok(Self::Sha1),
            "http://www.w3.org/2001/04/xmlenc#sha256" => Ok(Self::Sha256),
            "http://www.w3.org/2001/04/xmldsig-more#sha384" => Ok(Self::Sha384),
            "http://www.w3.org/2001/04/xmlenc#sha512" => Ok(Self::Sha512),
            other => Err(format!("unsupported DigestMethod algorithm: {other}")),
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            // sha1::Sha1 and the sha2 hashers share the same digest::Digest trait.
            Self::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }

    fn uri(self) -> &'static str {
        match self {
            Self::Sha1 => "http://www.w3.org/2000/09/xmldsig#sha1",
            Self::Sha256 => DIGEST_METHOD,
            Self::Sha384 => "http://www.w3.org/2001/04/xmldsig-more#sha384",
            Self::Sha512 => "http://www.w3.org/2001/04/xmlenc#sha512",
        }
    }
}

/// A supported RSASSA-PKCS1-v1_5 SignatureMethod (the only signature family
/// SMPTE and Interop DCPs use), paired with the hash that DigestInfo prefixes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RsaSigAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl RsaSigAlg {
    fn from_uri(uri: &str) -> Result<Self, String> {
        match uri {
            "http://www.w3.org/2000/09/xmldsig#rsa-sha1" => Ok(Self::Sha1),
            "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256" => Ok(Self::Sha256),
            "http://www.w3.org/2001/04/xmldsig-more#rsa-sha384" => Ok(Self::Sha384),
            "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512" => Ok(Self::Sha512),
            other => Err(format!("unsupported SignatureMethod algorithm: {other}")),
        }
    }

    fn uri(self) -> &'static str {
        match self {
            Self::Sha1 => "http://www.w3.org/2000/09/xmldsig#rsa-sha1",
            Self::Sha256 => SIG_METHOD,
            Self::Sha384 => "http://www.w3.org/2001/04/xmldsig-more#rsa-sha384",
            Self::Sha512 => "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512",
        }
    }

    /// Verify the PKCS#1 v1.5 signature over the canonical SignedInfo bytes,
    /// hashing with this method's digest before the RSA check.
    fn verify(
        self,
        key: &rsa::RsaPublicKey,
        signed_info_c14n: &[u8],
        signature: &[u8],
    ) -> Result<(), rsa::Error> {
        match self {
            Self::Sha1 => key.verify(
                rsa::Pkcs1v15Sign::new::<sha1::Sha1>(),
                &DigestAlg::Sha1.digest(signed_info_c14n),
                signature,
            ),
            Self::Sha256 => key.verify(
                rsa::Pkcs1v15Sign::new::<sha2::Sha256>(),
                &DigestAlg::Sha256.digest(signed_info_c14n),
                signature,
            ),
            Self::Sha384 => key.verify(
                rsa::Pkcs1v15Sign::new::<sha2::Sha384>(),
                &DigestAlg::Sha384.digest(signed_info_c14n),
                signature,
            ),
            Self::Sha512 => key.verify(
                rsa::Pkcs1v15Sign::new::<sha2::Sha512>(),
                &DigestAlg::Sha512.digest(signed_info_c14n),
                signature,
            ),
        }
    }

    /// Sign the canonical SignedInfo bytes with this method's hash.
    fn sign(
        self,
        key: &rsa::RsaPrivateKey,
        signed_info_c14n: &[u8],
    ) -> Result<Vec<u8>, rsa::Error> {
        match self {
            Self::Sha1 => key.sign(
                rsa::Pkcs1v15Sign::new::<sha1::Sha1>(),
                &DigestAlg::Sha1.digest(signed_info_c14n),
            ),
            Self::Sha256 => key.sign(
                rsa::Pkcs1v15Sign::new::<sha2::Sha256>(),
                &DigestAlg::Sha256.digest(signed_info_c14n),
            ),
            Self::Sha384 => key.sign(
                rsa::Pkcs1v15Sign::new::<sha2::Sha384>(),
                &DigestAlg::Sha384.digest(signed_info_c14n),
            ),
            Self::Sha512 => key.sign(
                rsa::Pkcs1v15Sign::new::<sha2::Sha512>(),
                &DigestAlg::Sha512.digest(signed_info_c14n),
            ),
        }
    }
}

/// Inclusive Canonical XML 1.0 (WithComments) of a fragment, pure Rust.
///
/// libxml2 is the engine xmlsec1 canonicalizes with; this matches its output
/// byte-for-byte for the fragments the signer emits. Scope is narrowed to
/// exactly that input: elements, text, comments, namespace declarations (the
/// default and prefixed, including a descendant that redefines the default, as
/// KDMRequiredExtensions and EncryptedKey do) and unprefixed attributes. A
/// DOCTYPE, processing instruction, CDATA section, XML declaration, entity
/// beyond the standard five, or namespaced attribute is a hard error: none can
/// occur here, and canonicalizing one silently could yield a wrong digest.
pub(crate) fn c14n(fragment: &str) -> Result<Vec<u8>, String> {
    use quick_xml::escape::unescape;
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(fragment);
    let mut out = String::with_capacity(fragment.len());

    // One frame per open element holding the (prefix, uri) declarations that
    // element rendered, so its End tag can drop them from scope. The default
    // namespace uses the empty prefix.
    let mut ns_stack: Vec<Vec<(String, String)>> = Vec::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| format!("fragment is not valid XML for c14n: {e}"))?
        {
            Event::Start(e) => out.push_str(&c14n_start_tag(&e, &mut ns_stack)?),
            Event::End(e) => {
                out.push_str(&c14n_end_tag(e.name().as_ref())?);
                ns_stack.pop();
            }
            Event::Empty(e) => {
                // c14n forbids self-closing tags: emit an explicit start + end.
                out.push_str(&c14n_start_tag(&e, &mut ns_stack)?);
                out.push_str(&c14n_end_tag(e.name().as_ref())?);
                ns_stack.pop();
            }
            Event::Text(e) => {
                let raw = std::str::from_utf8(&e)
                    .map_err(|err| format!("c14n text is not UTF-8: {err}"))?;
                let normalized = normalize_line_endings(raw);
                let text = unescape(&normalized)
                    .map_err(|err| format!("c14n cannot unescape text: {err}"))?;
                out.push_str(&escape_text(&text));
            }
            Event::Comment(e) => {
                let raw = std::str::from_utf8(&e)
                    .map_err(|err| format!("c14n comment is not UTF-8: {err}"))?;
                out.push_str("<!--");
                out.push_str(&normalize_line_endings(raw));
                out.push_str("-->");
            }
            Event::Eof => break,
            other => {
                return Err(format!(
                    "c14n input has an unsupported node ({other:?}); only elements, \
                     text and comments are canonicalized here"
                ));
            }
        }
    }

    Ok(out.into_bytes())
}

/// Current in-scope URI of `prefix` ("" is the default), searching innermost
/// frame first. Only rendered (changed) declarations live in the stack, so the
/// innermost hit is the value in force.
fn inscope_uri(ns_stack: &[Vec<(String, String)>], prefix: &str) -> Option<String> {
    ns_stack
        .iter()
        .rev()
        .flat_map(|frame| frame.iter().rev())
        .find(|(p, _)| p == prefix)
        .map(|(_, uri)| uri.clone())
}

/// Serialize one start tag in canonical form and push its rendered namespaces.
fn c14n_start_tag(
    e: &quick_xml::events::BytesStart,
    ns_stack: &mut Vec<Vec<(String, String)>>,
) -> Result<String, String> {
    let qname = std::str::from_utf8(e.name().as_ref())
        .map_err(|err| format!("c14n element name is not UTF-8: {err}"))?
        .to_string();

    // Split attributes into namespace declarations and ordinary attributes.
    let mut decls: Vec<(String, String)> = Vec::new(); // (prefix, uri), "" = default
    let mut attrs: Vec<(String, String)> = Vec::new(); // (name, value)
    for attr in e.attributes() {
        let attr = attr.map_err(|err| format!("c14n cannot read an attribute: {err}"))?;
        let key = std::str::from_utf8(attr.key.as_ref())
            .map_err(|err| format!("c14n attribute name is not UTF-8: {err}"))?;
        let value = attr
            .unescape_value()
            .map_err(|err| format!("c14n cannot unescape an attribute value: {err}"))?
            .into_owned();
        if key == "xmlns" {
            decls.push((String::new(), value));
        } else if let Some(prefix) = key.strip_prefix("xmlns:") {
            decls.push((prefix.to_string(), value));
        } else if key.contains(':') {
            return Err(format!(
                "c14n does not support namespaced attribute '{key}'; the signer emits none"
            ));
        } else {
            attrs.push((key.to_string(), value));
        }
    }

    // Inclusive c14n renders a namespace only where its in-scope value differs
    // from the ancestor context. An empty default matches an absent one, so
    // xmlns="" renders only when it overrides a non-empty ancestor default.
    let mut rendered: Vec<(String, String)> = Vec::new();
    for (prefix, uri) in decls {
        if inscope_uri(ns_stack, &prefix).as_deref().unwrap_or("") != uri {
            rendered.push((prefix, uri));
        }
    }
    ns_stack.push(rendered.clone());

    // Namespaces sort by prefix (empty default first); attributes by name (all
    // are unprefixed here, so no namespace-uri key participates in the sort).
    rendered.sort_by(|a, b| a.0.cmp(&b.0));
    attrs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut tag = String::new();
    tag.push('<');
    tag.push_str(&qname);
    for (prefix, uri) in &rendered {
        if prefix.is_empty() {
            tag.push_str(" xmlns=\"");
        } else {
            tag.push_str(" xmlns:");
            tag.push_str(prefix);
            tag.push_str("=\"");
        }
        tag.push_str(&escape_attr(uri));
        tag.push('"');
    }
    for (name, value) in &attrs {
        tag.push(' ');
        tag.push_str(name);
        tag.push_str("=\"");
        tag.push_str(&escape_attr(value));
        tag.push('"');
    }
    tag.push('>');
    Ok(tag)
}

fn c14n_end_tag(name: &[u8]) -> Result<String, String> {
    let qname = std::str::from_utf8(name)
        .map_err(|err| format!("c14n element name is not UTF-8: {err}"))?;
    Ok(format!("</{qname}>"))
}

/// C14N text-node escaping: `&` `<` `>` and CR.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
    out
}

/// C14N attribute-value escaping: `&` `<` `"` and tab, LF, CR.
fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
    out
}

/// XML line-ending normalization: CRLF and lone CR become LF. Character
/// references such as `&#xD;` are still literal text here and untouched, so a
/// real CR one later decodes to is preserved and escaped on output.
fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certificate::generate_chain;
    use std::sync::OnceLock;

    struct Chain {
        _dir: tempfile::TempDir,
        root: PathBuf,
        intermediate: PathBuf,
        signer: PathBuf,
        signer_key: PathBuf,
    }

    fn chain() -> &'static Chain {
        static CHAIN: OnceLock<Chain> = OnceLock::new();
        CHAIN.get_or_init(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let p = dir.path();
            assert_eq!(generate_chain("Acme", p), 0, "chain generation failed");
            Chain {
                root: p.join("root.pem"),
                intermediate: p.join("intermediate.pem"),
                signer: p.join("signer.pem"),
                signer_key: p.join("signer.key"),
                _dir: dir,
            }
        })
    }

    fn leaf_signer(c: &Chain) -> XmlSigner {
        XmlSigner {
            cert_file: c.signer.clone(),
            key_file: c.signer_key.clone(),
            chain_files: vec![c.intermediate.clone(), c.root.clone()],
        }
    }

    // A non-KDM enveloped document: a small IMF-CPL-like tree with a default
    // namespace and two Id-bearing elements to reference. The root does not
    // declare the ds prefix, so the signer must declare it on ds:Signature.
    fn cpl_doc() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CompositionPlaylist xmlns="http://example.com/imf/cpl">
  <Id>urn:uuid:11111111-2222-3333-4444-555555555555</Id>
  <ContentTitleText Id="ID_title">Example &amp; Co "IMF" CPL</ContentTitleText>
  <ReelList Id="ID_reels">
    <Reel>
      <Id>urn:uuid:66666666-7777-8888-9999-aaaaaaaaaaaa</Id>
    </Reel>
  </ReelList>
</CompositionPlaylist>
"#
        .to_string()
    }

    fn xmlsec1_available() -> bool {
        std::process::Command::new("xmlsec1")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn xmlsec1_verify(doc: &Path, trusted_pem: &Path) -> std::process::Output {
        std::process::Command::new("xmlsec1")
            .arg("--verify")
            .arg("--trusted-pem")
            .arg(trusted_pem)
            .args(["--id-attr:Id", "ContentTitleText"])
            .args(["--id-attr:Id", "ReelList"])
            .arg(doc)
            .output()
            .expect("run xmlsec1")
    }

    // Standard enveloped profile: verify with no --id-attr hints at all.
    fn xmlsec1_verify_no_id(doc: &Path, trusted_pem: &Path) -> std::process::Output {
        std::process::Command::new("xmlsec1")
            .arg("--verify")
            .arg("--trusted-pem")
            .arg(trusted_pem)
            .arg(doc)
            .output()
            .expect("run xmlsec1")
    }

    #[test]
    fn generic_doc_signature_verifies_with_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let c = chain();
        let signed = sign_enveloped(
            &cpl_doc(),
            &["ID_title", "ID_reels"],
            "Id",
            None,
            &leaf_signer(c),
        )
        .expect("sign generic doc");

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cpl-signed.xml");
        std::fs::write(&out, &signed).unwrap();

        let result = xmlsec1_verify(&out, &c.root);
        eprintln!(
            "xmlsec1 --verify (generic doc):\n  status: {}\n  stdout: {}\n  stderr: {}",
            result.status,
            String::from_utf8_lossy(&result.stdout).trim(),
            String::from_utf8_lossy(&result.stderr).trim(),
        );
        assert!(
            result.status.success(),
            "xmlsec1 must verify the signed generic document against the trusted root"
        );
    }

    #[test]
    fn generic_doc_tamper_fails_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let c = chain();
        let signed = sign_enveloped(
            &cpl_doc(),
            &["ID_title", "ID_reels"],
            "Id",
            None,
            &leaf_signer(c),
        )
        .expect("sign generic doc");
        let tampered = signed.replacen("Example &amp; Co", "Tampered &amp; Co", 1);
        assert_ne!(signed, tampered, "tamper must change the document");

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cpl-tampered.xml");
        std::fs::write(&out, &tampered).unwrap();

        assert!(
            !xmlsec1_verify(&out, &c.root).status.success(),
            "xmlsec1 must reject a tampered referenced element"
        );
    }

    #[test]
    fn verify_enveloped_accepts_a_good_signature_and_rejects_tampering() {
        let c = chain();
        let signed = sign_enveloped(
            &cpl_doc(),
            &["ID_title", "ID_reels"],
            "Id",
            None,
            &leaf_signer(c),
        )
        .expect("sign generic doc");

        // The embedded leaf, and the leaf pinned as the trusted cert, both verify.
        verify_enveloped(&signed, "Id", None).expect("self-consistent signature must verify");
        verify_enveloped(&signed, "Id", Some(&c.signer))
            .expect("signature must verify against the pinned leaf cert");

        // A different trusted cert (the root) must be rejected.
        assert!(
            verify_enveloped(&signed, "Id", Some(&c.root)).is_err(),
            "a non-matching trusted cert must fail"
        );

        // Tampering a referenced element must fail the digest check.
        let tampered = signed.replacen("Example &amp; Co", "Tampered &amp; Co", 1);
        let err = verify_enveloped(&tampered, "Id", None)
            .expect_err("tampered reference must fail verification");
        assert!(err.contains("digest mismatch"), "got: {err}");
    }

    #[test]
    fn document_enveloped_signature_verifies_with_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let c = chain();
        let signed = sign_document_enveloped(&cpl_doc(), &leaf_signer(c))
            .expect("sign document (enveloped)");

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cpl-enveloped.xml");
        std::fs::write(&out, &signed).unwrap();

        // The whole point of the standard profile: NO --id-attr hints needed.
        let result = xmlsec1_verify_no_id(&out, &c.root);
        eprintln!(
            "xmlsec1 --verify (enveloped, no --id-attr):\n  status: {}\n  stdout: {}\n  stderr: {}",
            result.status,
            String::from_utf8_lossy(&result.stdout).trim(),
            String::from_utf8_lossy(&result.stderr).trim(),
        );
        assert!(
            result.status.success(),
            "xmlsec1 must verify the enveloped document with no --id-attr hints"
        );
    }

    #[test]
    fn document_enveloped_tamper_fails_xmlsec1() {
        if !xmlsec1_available() {
            eprintln!("skipping: xmlsec1 not installed");
            return;
        }
        let c = chain();
        let signed = sign_document_enveloped(&cpl_doc(), &leaf_signer(c))
            .expect("sign document (enveloped)");
        let tampered = signed.replacen("Example &amp; Co", "Tampered &amp; Co", 1);
        assert_ne!(signed, tampered, "tamper must change the document");

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cpl-enveloped-tampered.xml");
        std::fs::write(&out, &tampered).unwrap();

        assert!(
            !xmlsec1_verify_no_id(&out, &c.root).status.success(),
            "xmlsec1 must reject a tampered enveloped document"
        );
    }

    #[test]
    fn verify_document_enveloped_accepts_good_and_rejects_tampering() {
        let c = chain();
        let signed = sign_document_enveloped(&cpl_doc(), &leaf_signer(c))
            .expect("sign document (enveloped)");

        verify_document_enveloped(&signed, None).expect("self-consistent signature must verify");
        verify_document_enveloped(&signed, Some(&c.signer))
            .expect("signature must verify against the pinned leaf cert");
        assert!(
            verify_document_enveloped(&signed, Some(&c.root)).is_err(),
            "a non-matching trusted cert must fail"
        );

        let tampered = signed.replacen("Example &amp; Co", "Tampered &amp; Co", 1);
        let err = verify_document_enveloped(&tampered, None)
            .expect_err("tampered document must fail verification");
        assert!(err.contains("digest mismatch"), "got: {err}");
    }

    #[test]
    fn sha1_document_signature_verifies_and_tamper_fails() {
        let c = chain();
        // Sign the whole document with SHA-1/rsa-sha1, the profile real Interop
        // and older SMPTE DCPs use, through the same internal code path.
        let signed = sign_document_enveloped_with(
            &cpl_doc(),
            &leaf_signer(c),
            DigestAlg::Sha1,
            RsaSigAlg::Sha1,
        )
        .expect("sign document with sha1");
        assert!(signed.contains("xmldsig#sha1"));
        assert!(signed.contains("xmldsig#rsa-sha1"));

        verify_document_enveloped(&signed, None).expect("sha1 signature must verify");
        verify_document_enveloped(&signed, Some(&c.signer))
            .expect("sha1 signature must verify against the pinned leaf cert");

        let tampered = signed.replacen("Example &amp; Co", "Tampered &amp; Co", 1);
        let err = verify_document_enveloped(&tampered, None)
            .expect_err("tampered sha1 document must fail verification");
        assert!(err.contains("digest mismatch"), "got: {err}");
    }

    #[test]
    fn sha1_byid_signature_verifies_and_tamper_fails() {
        let c = chain();
        let signed = sign_enveloped_with(
            &cpl_doc(),
            &["ID_title", "ID_reels"],
            "Id",
            None,
            &leaf_signer(c),
            DigestAlg::Sha1,
            RsaSigAlg::Sha1,
        )
        .expect("sign by-id with sha1");

        verify_enveloped(&signed, "Id", None).expect("sha1 by-id signature must verify");
        let tampered = signed.replacen("Example &amp; Co", "Tampered &amp; Co", 1);
        assert!(verify_enveloped(&tampered, "Id", None).is_err());
    }

    #[test]
    fn unsupported_algorithm_is_rejected() {
        assert!(DigestAlg::from_uri("http://example.com/md5").is_err());
        assert!(RsaSigAlg::from_uri("http://example.com/dsa").is_err());
    }

    // Cross-check against real published DCPs: point POSTKIT_CLAIRMETA_DATA at a
    // clone of ClairMeta_Data. The ECL set is SHA-1 signed, so before the
    // algorithm dispatch these all falsely failed with a signature error.
    #[test]
    fn real_ecl_dcps_verify() {
        let Ok(root) = std::env::var("POSTKIT_CLAIRMETA_DATA") else {
            eprintln!("skipping: set POSTKIT_CLAIRMETA_DATA to a ClairMeta_Data clone");
            return;
        };
        let ecl = std::path::Path::new(&root).join("DCP/ECL-SET");
        let mut checked = 0;
        for entry in std::fs::read_dir(&ecl).expect("read ECL-SET") {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&dir).unwrap() {
                let path = file.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if !(name.starts_with("CPL") || name.starts_with("PKL")) || !name.ends_with(".xml")
                {
                    continue;
                }
                let xml = std::fs::read_to_string(&path).unwrap();
                if !xml.contains("SignatureValue") {
                    continue;
                }
                verify_document_enveloped(&xml, None)
                    .unwrap_or_else(|e| panic!("real DCP {} must verify: {e}", path.display()));
                // A tampered copy of the same document must be rejected.
                if let Some(pos) = xml.find("<Issuer>") {
                    let mut bad = xml.clone();
                    bad.insert(pos + "<Issuer>".len(), 'x');
                    assert!(
                        verify_document_enveloped(&bad, None).is_err(),
                        "tampered {} must fail",
                        path.display()
                    );
                }
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "found no signed CPL/PKL under {}",
            ecl.display()
        );
        eprintln!("verified {checked} real signed ECL documents");
    }

    #[test]
    fn missing_reference_element_is_an_error() {
        let c = chain();
        let err = sign_enveloped(&cpl_doc(), &["ID_absent"], "Id", None, &leaf_signer(c))
            .expect_err("must fail when a referenced id is absent");
        assert!(err.contains("ID_absent"), "got: {err}");
    }
}
