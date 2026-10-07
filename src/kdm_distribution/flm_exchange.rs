use super::flm::parse_flm;
use base64::Engine;
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use ureq::http::header::{
    ACCEPT, AUTHORIZATION, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION,
};
use ureq::http::{HeaderMap, StatusCode};
use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts, TlsConfig, TlsProvider};
use url::{Origin, Url};

pub const SITE_LIST_NAMESPACE: &str = "http://www.smpte-ra.org/ns/430-15/2017/SiteList";
pub const MAXIMUM_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAXIMUM_REDIRECTS: usize = 5;
const REDIRECT_STATUSES: [StatusCode; 5] = [
    StatusCode::MOVED_PERMANENTLY,
    StatusCode::FOUND,
    StatusCode::SEE_OTHER,
    StatusCode::TEMPORARY_REDIRECT,
    StatusCode::PERMANENT_REDIRECT,
];
const XLINK_NAMESPACE: &str = "http://www.w3.org/1999/xlink";
const XLINK_SIMPLE_TYPE: &str = "simple";
const XML_MEDIA_TYPE: &str = "application/xml";
const HTTPS_SCHEME: &str = "https";
const HTTP_SCHEME: &str = "http";
const XML_DATE_TIME_WITHOUT_OFFSET: &str = "%Y-%m-%dT%H:%M:%S%.f";
const REDACTED: &str = "<redacted>";

#[derive(Clone)]
pub enum FeedAuthentication {
    None,
    Basic {
        username: String,
        password: String,
    },
    ClientCertificate {
        certificate_chain: PathBuf,
        private_key: PathBuf,
    },
}

impl FeedAuthentication {
    fn has_credentials(&self) -> bool {
        !matches!(self, Self::None)
    }
}

impl fmt::Debug for FeedAuthentication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &REDACTED)
                .finish(),
            Self::ClientCertificate {
                certificate_chain,
                private_key,
            } => f
                .debug_struct("ClientCertificate")
                .field("certificate_chain", certificate_chain)
                .field("private_key", private_key)
                .finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Feed {
    pub site_list_url: String,
    pub authentication: FeedAuthentication,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedSyncState {
    #[serde(default)]
    pub site_list_etag: Option<String>,
    #[serde(default)]
    pub site_list_last_modified: Option<String>,
    // facility id to the SiteList `modified` of the FLM last fetched and parsed
    #[serde(default)]
    pub fetched_facility_modified: BTreeMap<String, DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteListFacility {
    pub id: String,
    pub modified: DateTime<FixedOffset>,
    pub href: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteList {
    pub originator: String,
    pub system_name: String,
    pub date_time_created: DateTime<FixedOffset>,
    pub facilities: Vec<SiteListFacility>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedFlm {
    pub facility_id: String,
    pub modified: DateTime<FixedOffset>,
    pub url: String,
    pub xml: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacilityFailure {
    pub facility_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRefresh {
    // None when the server answered 304 Not Modified
    pub site_list: Option<SiteList>,
    pub fetched: Vec<FetchedFlm>,
    pub failures: Vec<FacilityFailure>,
    pub removed_facility_ids: Vec<String>,
    pub state: FeedSyncState,
}

pub fn refresh_feed(feed: &Feed, previous: &FeedSyncState) -> Result<FeedRefresh, String> {
    refresh_feed_with_root_certificates(feed, previous, RootCerts::WebPki)
}

pub(crate) fn refresh_feed_with_root_certificates(
    feed: &Feed,
    previous: &FeedSyncState,
    root_certificates: RootCerts,
) -> Result<FeedRefresh, String> {
    let site_list_url = Url::parse(&feed.site_list_url)
        .map_err(|error| format!("the feed's SiteList URL is not a URL: {error}"))?;
    check_request_url(&site_list_url)?;
    if feed.authentication.has_credentials() && site_list_url.scheme() != HTTPS_SCHEME {
        return Err(format!(
            "the feed {site_list_url} has credentials, which are only sent over https"
        ));
    }
    let client = FeedClient::new(feed, &site_list_url, root_certificates)?;

    let mut conditional_headers = Vec::new();
    if let Some(etag) = &previous.site_list_etag {
        conditional_headers.push((IF_NONE_MATCH.as_str(), etag.as_str()));
    }
    if let Some(last_modified) = &previous.site_list_last_modified {
        conditional_headers.push((IF_MODIFIED_SINCE.as_str(), last_modified.as_str()));
    }
    let FeedResponse::Document {
        url: site_list_document_url,
        etag,
        last_modified,
        body,
    } = client.get(&site_list_url, &conditional_headers)?
    else {
        return Ok(FeedRefresh {
            site_list: None,
            fetched: Vec::new(),
            failures: Vec::new(),
            removed_facility_ids: Vec::new(),
            state: previous.clone(),
        });
    };
    let site_list = parse_site_list(&body)
        .map_err(|error| format!("SiteList at {site_list_document_url}: {error}"))?;

    let listed_ids: HashSet<&str> = site_list
        .facilities
        .iter()
        .map(|facility| facility.id.as_str())
        .collect();
    let removed_facility_ids = previous
        .fetched_facility_modified
        .keys()
        .filter(|id| !listed_ids.contains(id.as_str()))
        .cloned()
        .collect();

    let mut fetched = Vec::new();
    let mut failures = Vec::new();
    let mut fetched_facility_modified = BTreeMap::new();
    for facility in &site_list.facilities {
        let previous_modified = previous.fetched_facility_modified.get(&facility.id);
        let modified = facility.modified.with_timezone(&Utc);
        if previous_modified == Some(&modified) {
            fetched_facility_modified.insert(facility.id.clone(), modified);
            continue;
        }
        match client.fetch_flm(&site_list_document_url, facility) {
            Ok(flm) => {
                fetched_facility_modified.insert(facility.id.clone(), modified);
                fetched.push(flm);
            }
            Err(reason) => {
                // keeping the older instant retries the facility on the next refresh
                if let Some(previous_modified) = previous_modified {
                    fetched_facility_modified.insert(facility.id.clone(), *previous_modified);
                }
                failures.push(FacilityFailure {
                    facility_id: facility.id.clone(),
                    reason,
                });
            }
        }
    }

    // a 304 next time would skip retrying the failed facilities
    let keep_validators = failures.is_empty();
    let state = FeedSyncState {
        site_list_etag: etag.filter(|_| keep_validators),
        site_list_last_modified: last_modified.filter(|_| keep_validators),
        fetched_facility_modified,
    };
    Ok(FeedRefresh {
        site_list: Some(site_list),
        fetched,
        failures,
        removed_facility_ids,
        state,
    })
}

enum FeedResponse {
    NotModified,
    Document {
        url: Url,
        etag: Option<String>,
        last_modified: Option<String>,
        body: String,
    },
}

struct FeedClient {
    agent: ureq::Agent,
    site_origin: Origin,
    has_credentials: bool,
    authorization: Option<String>,
}

impl FeedClient {
    fn new(feed: &Feed, site_list_url: &Url, root_certificates: RootCerts) -> Result<Self, String> {
        // ureq would otherwise use whichever provider the process installed as default
        let crypto_provider = Arc::new(rustls::crypto::ring::default_provider());
        let (authorization, client_certificate) = match &feed.authentication {
            FeedAuthentication::None => (None, None),
            FeedAuthentication::Basic { username, password } => {
                let credentials = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                (Some(format!("Basic {credentials}")), None)
            }
            FeedAuthentication::ClientCertificate {
                certificate_chain,
                private_key,
            } => (
                None,
                Some(load_client_certificate(
                    certificate_chain,
                    private_key,
                    &crypto_provider,
                )?),
            ),
        };
        let tls_config = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(crypto_provider)
            .root_certs(root_certificates)
            .client_cert(client_certificate)
            .build();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // `get` follows redirects itself to check each target
            .max_redirects(0)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .tls_config(tls_config)
            .build()
            .into();
        Ok(Self {
            agent,
            site_origin: site_list_url.origin(),
            has_credentials: feed.authentication.has_credentials(),
            authorization,
        })
    }

    fn get(
        &self,
        start: &Url,
        conditional_headers: &[(&str, &str)],
    ) -> Result<FeedResponse, String> {
        let mut url = start.clone();
        for _ in 0..=MAXIMUM_REDIRECTS {
            let mut request = self.agent.get(url.as_str()).header(ACCEPT, XML_MEDIA_TYPE);
            for (name, value) in conditional_headers {
                request = request.header(*name, *value);
            }
            if let Some(authorization) = &self.authorization
                && url.origin() == self.site_origin
            {
                request = request.header(AUTHORIZATION, authorization);
            }
            let mut response = request
                .call()
                .map_err(|error| format!("GET {url} failed: {error}"))?;
            let status = response.status();
            if REDIRECT_STATUSES.contains(&status) {
                url = self.redirect_target(&url, status, response.headers())?;
                continue;
            }
            if status == StatusCode::NOT_MODIFIED {
                return Ok(FeedResponse::NotModified);
            }
            let body = response
                .body_mut()
                .with_config()
                .limit(MAXIMUM_RESPONSE_BYTES)
                .read_to_string()
                .map_err(|error| match error {
                    ureq::Error::BodyExceedsLimit(_) => format!(
                        "the response from {url} is larger than {MAXIMUM_RESPONSE_BYTES} bytes"
                    ),
                    error => format!("reading the response from {url} failed: {error}"),
                });
            if status != StatusCode::OK {
                return Err(status_error(&url, status, body.ok().as_deref()));
            }
            let headers = response.headers();
            return Ok(FeedResponse::Document {
                etag: header_text(headers, ETAG),
                last_modified: header_text(headers, LAST_MODIFIED),
                body: body?,
                url,
            });
        }
        Err(format!(
            "GET {start} redirected more than {MAXIMUM_REDIRECTS} times"
        ))
    }

    fn redirect_target(
        &self,
        from: &Url,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<Url, String> {
        let location = header_text(headers, LOCATION).ok_or_else(|| {
            format!("GET {from} returned HTTP {status} without a readable Location")
        })?;
        let target = from
            .join(&location)
            .map_err(|error| format!("GET {from} redirected to {location}: {error}"))?;
        check_request_url(&target)?;
        if leaves_https(from, &target) {
            return Err(format!(
                "refused the redirect from {from} to {target}, which leaves https"
            ));
        }
        if self.has_credentials && target.origin() != self.site_origin {
            return Err(format!(
                "refused the redirect from {from} to {target}: the feed's credentials are only sent to {}",
                self.site_origin.ascii_serialization()
            ));
        }
        Ok(target)
    }

    fn fetch_flm(
        &self,
        site_list_url: &Url,
        facility: &SiteListFacility,
    ) -> Result<FetchedFlm, String> {
        let flm_url = site_list_url.join(&facility.href).map_err(|error| {
            format!(
                "href {} does not resolve against {site_list_url}: {error}",
                facility.href
            )
        })?;
        check_request_url(&flm_url)?;
        if leaves_https(site_list_url, &flm_url) {
            return Err(format!(
                "refused {flm_url}, which leaves https for a SiteList read over https"
            ));
        }
        if self.has_credentials && flm_url.origin() != self.site_origin {
            return Err(format!(
                "{flm_url} is not on {}, the only origin the feed's credentials are sent to",
                self.site_origin.ascii_serialization()
            ));
        }
        let FeedResponse::Document { body, .. } = self.get(&flm_url, &[])? else {
            return Err(format!(
                "GET {flm_url} returned HTTP 304 to an unconditional request"
            ));
        };
        let flm = parse_flm(&body).map_err(|error| format!("{flm_url}: {error}"))?;
        if flm.facility.id.as_deref() != Some(facility.id.as_str()) {
            return Err(format!(
                "the FLM at {flm_url} has FacilityID {}, the SiteList lists it as {}",
                flm.facility.id.as_deref().unwrap_or("(none)"),
                facility.id
            ));
        }
        Ok(FetchedFlm {
            facility_id: facility.id.clone(),
            modified: facility.modified,
            url: flm_url.to_string(),
            xml: body,
        })
    }
}

fn check_request_url(url: &Url) -> Result<(), String> {
    if !url.username().is_empty() || url.password().is_some() {
        let mut without_credentials = url.clone();
        // only fails for URLs that cannot carry credentials
        let _ = without_credentials.set_username("");
        let _ = without_credentials.set_password(None);
        return Err(format!(
            "{without_credentials} has a username or password in the URL, put credentials in the feed's authentication"
        ));
    }
    if url.scheme() != HTTPS_SCHEME && url.scheme() != HTTP_SCHEME {
        return Err(format!("{url} is not an http or https URL"));
    }
    Ok(())
}

fn leaves_https(from: &Url, to: &Url) -> bool {
    from.scheme() == HTTPS_SCHEME && to.scheme() != HTTPS_SCHEME
}

fn header_text(headers: &HeaderMap, name: ureq::http::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn status_error(url: &Url, status: StatusCode, body: Option<&str>) -> String {
    let status = status.as_u16();
    match body.and_then(parse_error_document) {
        Some(ServerError {
            token,
            message: Some(message),
        }) => format!("GET {url} returned HTTP {status}: {token}: {message}"),
        Some(ServerError {
            token,
            message: None,
        }) => format!("GET {url} returned HTTP {status}: {token}"),
        None => format!("GET {url} returned HTTP {status}"),
    }
}

fn load_client_certificate(
    chain_path: &Path,
    key_path: &Path,
    crypto_provider: &CryptoProvider,
) -> Result<ClientCert, String> {
    let chain_pem = std::fs::read(chain_path).map_err(|error| {
        format!(
            "cannot read the client certificate chain {}: {error}",
            chain_path.display()
        )
    })?;
    let key_pem = std::fs::read(key_path).map_err(|error| {
        format!(
            "cannot read the private key {}: {error}",
            key_path.display()
        )
    })?;
    let unreadable_chain = || {
        format!(
            "{} holds no readable PEM certificate chain",
            chain_path.display()
        )
    };
    let chain = ureq::tls::parse_pem(&chain_pem)
        .filter_map(|item| match item {
            Ok(PemItem::Certificate(certificate)) => Some(Ok(certificate)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<Certificate<'static>>, _>>()
        .map_err(|_| unreadable_chain())?;
    if chain.is_empty() {
        return Err(unreadable_chain());
    }
    // parser errors can quote the key file
    let unreadable_key = format!("{} holds no readable PEM private key", key_path.display());
    let key = PrivateKey::from_pem(&key_pem).map_err(|_| unreadable_key.clone())?;
    let key_der = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|_| unreadable_key)?;
    // ureq panics on a key it cannot use or one that does not match the leaf
    rustls::sign::CertifiedKey::from_der(
        chain
            .iter()
            .map(|certificate| CertificateDer::from(certificate.der().to_vec()))
            .collect(),
        key_der,
        crypto_provider,
    )
    .map_err(|error| {
        format!(
            "the private key {} does not fit the certificate chain {}: {error}",
            key_path.display(),
            chain_path.display()
        )
    })?;
    Ok(ClientCert::new_with_certs(&chain, key))
}

type Node<'a> = roxmltree::Node<'a, 'a>;

fn parse_xml(xml: &str) -> Result<roxmltree::Document<'_>, String> {
    roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            ..Default::default()
        },
    )
    .map_err(|error| format!("not valid XML: {error}"))
}

fn site_list_child<'a>(node: Node<'a>, name: &str) -> Option<Node<'a>> {
    site_list_children(node, name).next()
}

fn site_list_children<'a>(node: Node<'a>, name: &str) -> impl Iterator<Item = Node<'a>> {
    node.children().filter(move |child| {
        child.is_element()
            && child.tag_name().name() == name
            && child.tag_name().namespace() == Some(SITE_LIST_NAMESPACE)
    })
}

fn element_text(node: Node<'_>) -> String {
    node.text().unwrap_or_default().trim().to_string()
}

fn required_child_text(node: Node<'_>, name: &str) -> Result<String, String> {
    site_list_child(node, name)
        .map(element_text)
        .ok_or_else(|| format!("<{}> has no <{name}>", node.tag_name().name()))
}

fn required_attribute<'a>(
    facility: Node<'a>,
    name: impl Into<roxmltree::ExpandedName<'a, 'a>>,
    label: &str,
) -> Result<&'a str, String> {
    facility
        .attribute(name)
        .map(str::trim)
        .ok_or_else(|| format!("a <Facility> has no {label} attribute"))
}

// an xs:dateTime without an offset is read as UTC
fn parse_xml_date_time(text: &str) -> Result<DateTime<FixedOffset>, String> {
    let text = text.trim();
    if let Ok(instant) = DateTime::parse_from_rfc3339(text) {
        return Ok(instant);
    }
    NaiveDateTime::parse_from_str(text, XML_DATE_TIME_WITHOUT_OFFSET)
        .map(|naive| naive.and_utc().fixed_offset())
        .map_err(|_| format!("{text} is not an xs:dateTime"))
}

fn parse_site_list(xml: &str) -> Result<SiteList, String> {
    let document = parse_xml(xml)?;
    let root = document.root_element();
    if root.tag_name().name() != "SiteList" {
        return Err(format!(
            "root element is <{}>, expected <SiteList>",
            root.tag_name().name()
        ));
    }
    let namespace = root.tag_name().namespace();
    if namespace != Some(SITE_LIST_NAMESPACE) {
        return Err(format!(
            "SiteList is in namespace {}, only {SITE_LIST_NAMESPACE} is read",
            namespace.unwrap_or("(none)")
        ));
    }
    let originator = required_child_text(root, "Originator")?;
    let system_name = required_child_text(root, "SystemName")?;
    let date_time_created = parse_xml_date_time(&required_child_text(root, "DateTimeCreated")?)?;
    let facility_list =
        site_list_child(root, "FacilityList").ok_or("<SiteList> has no <FacilityList>")?;

    let mut facilities = Vec::new();
    let mut seen_ids = HashSet::new();
    for facility in site_list_children(facility_list, "Facility") {
        let id = required_attribute(facility, "id", "id")?;
        let modified_text = required_attribute(facility, "modified", "modified")?;
        let modified = parse_xml_date_time(modified_text)
            .map_err(|error| format!("Facility {id} modified: {error}"))?;
        let href = required_attribute(facility, (XLINK_NAMESPACE, "href"), "xlink:href")?;
        let link_type = required_attribute(facility, (XLINK_NAMESPACE, "type"), "xlink:type")?;
        if link_type != XLINK_SIMPLE_TYPE {
            return Err(format!(
                "Facility {id} has xlink:type {link_type}, expected {XLINK_SIMPLE_TYPE}"
            ));
        }
        if !seen_ids.insert(id) {
            return Err(format!("Facility {id} is listed more than once"));
        }
        facilities.push(SiteListFacility {
            id: id.to_string(),
            modified,
            href: href.to_string(),
        });
    }
    Ok(SiteList {
        originator,
        system_name,
        date_time_created,
        facilities,
    })
}

struct ServerError {
    token: String,
    message: Option<String>,
}

fn parse_error_document(xml: &str) -> Option<ServerError> {
    let document = parse_xml(xml).ok()?;
    let root = document.root_element();
    if root.tag_name().name() != "Error" || root.tag_name().namespace() != Some(SITE_LIST_NAMESPACE)
    {
        return None;
    }
    Some(ServerError {
        token: site_list_child(root, "Token").map(element_text)?,
        message: site_list_child(root, "Message").map(element_text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::test_support::{extended_flm, read};
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    };
    use rustls::server::WebPkiClientVerifier;
    use rustls::{
        RootCertStore, ServerConfig, ServerConnection, StreamOwned, SupportedProtocolVersion,
    };
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;

    const SMPTE_EXAMPLE_FLM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flm/st430-16b-2017.xml"
    );
    const SMPTE_EXAMPLE_FACILITY_ID: &str = "urn:x-facilityID:example.com:XPL:US-1600176-XPL";
    const ISDCF_SITE_LIST_NAMESPACE: &str = "http://isdcf.com/2010/04/SiteList";
    const SITE_LIST_PATH: &str = "/feed/sitelist.xml";
    const USERNAME: &str = "distributor";
    const PASSWORD: &str = "correct-horse-battery-staple";
    const FIRST_MODIFIED: &str = "2026-10-01T10:00:00Z";
    const LAST_MODIFIED_DATE: &str = "Thu, 01 Oct 2026 10:00:00 GMT";
    const TIME_ZONE: &str = "Europe/London";

    fn facility_id(name: &str) -> String {
        format!("urn:x-facilityID:example.com:{name}")
    }

    fn cinema_flm(name: &str) -> String {
        extended_flm(name, TIME_ZONE, &[])
    }

    fn site_list_xml(namespace: &str, facilities: &[(&str, &str, &str)]) -> String {
        let entries: String = facilities
            .iter()
            .map(|(id, modified, href)| {
                format!(
                    r#"<Facility id="{id}" modified="{modified}" xlink:href="{href}" xlink:type="simple"/>"#
                )
            })
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<SiteList xmlns="{namespace}" xmlns:xlink="http://www.w3.org/1999/xlink">
  <Originator>https://feed.example.com/feed/sitelist.xml</Originator>
  <SystemName>Exhibitor TMS</SystemName>
  <DateTimeCreated>2026-10-01T10:00:00+01:00</DateTimeCreated>
  <FacilityList>{entries}</FacilityList>
</SiteList>"#
        )
    }

    fn error_xml(token: &str, message: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Error xmlns="{SITE_LIST_NAMESPACE}"><Token>{token}</Token><Message>{message}</Message></Error>"#
        )
    }

    fn basic_authorization() -> String {
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{PASSWORD}"));
        format!("Basic {credentials}")
    }

    fn feed(url: String, authentication: FeedAuthentication) -> Feed {
        Feed {
            site_list_url: url,
            authentication,
        }
    }

    fn basic() -> FeedAuthentication {
        FeedAuthentication::Basic {
            username: USERNAME.to_string(),
            password: PASSWORD.to_string(),
        }
    }

    #[derive(Clone)]
    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl TestResponse {
        fn xml(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                headers: vec![(
                    "Content-Type".to_string(),
                    "application/xml; charset=UTF-8".to_string(),
                )],
                body: body.into(),
            }
        }

        fn redirect(status: u16, location: &str) -> Self {
            Self::xml(status, Vec::new()).with_header("Location", location)
        }

        fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    #[derive(Clone)]
    struct RecordedRequest {
        path: String,
        headers: HashMap<String, String>,
        client_certificate: Option<Vec<u8>>,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).map(String::as_str)
        }
    }

    type Routes = Arc<Mutex<HashMap<String, TestResponse>>>;
    type Requests = Arc<Mutex<Vec<RecordedRequest>>>;

    struct TestServer {
        origin: String,
        routes: Routes,
        requests: Requests,
    }

    impl TestServer {
        fn plain() -> Self {
            Self::start(HTTP_SCHEME, None)
        }

        fn tls(identity: &TestIdentity, require_client_certificate: bool) -> Self {
            Self::start(
                HTTPS_SCHEME,
                Some(Arc::new(identity.server_config(
                    require_client_certificate,
                    rustls::DEFAULT_VERSIONS,
                ))),
            )
        }

        fn start(scheme: &str, tls: Option<Arc<ServerConfig>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("{scheme}://{}", listener.local_addr().unwrap());
            let routes = Routes::default();
            let requests = Requests::default();
            let (thread_routes, thread_requests) = (routes.clone(), requests.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    match &tls {
                        None => serve(stream, None, &thread_routes, &thread_requests),
                        Some(config) => {
                            serve_tls(stream, config.clone(), &thread_routes, &thread_requests)
                        }
                    }
                }
            });
            Self {
                origin,
                routes,
                requests,
            }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.origin)
        }

        fn route(&self, path: &str, response: TestResponse) {
            self.routes
                .lock()
                .unwrap()
                .insert(path.to_string(), response);
        }

        fn take_requests(&self) -> Vec<RecordedRequest> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }
    }

    fn serve_tls(
        stream: TcpStream,
        config: Arc<ServerConfig>,
        routes: &Routes,
        requests: &Requests,
    ) {
        let Ok(connection) = ServerConnection::new(config) else {
            return;
        };
        let mut stream = StreamOwned::new(connection, stream);
        while stream.conn.is_handshaking() {
            if stream.conn.complete_io(&mut stream.sock).is_err() {
                return;
            }
        }
        let client_certificate = stream
            .conn
            .peer_certificates()
            .and_then(|chain| chain.first())
            .map(|leaf| leaf.to_vec());
        serve(stream, client_certificate, routes, requests);
    }

    fn serve(
        mut stream: impl Read + Write,
        client_certificate: Option<Vec<u8>>,
        routes: &Routes,
        requests: &Requests,
    ) {
        let mut head = Vec::new();
        let mut reader = BufReader::new(&mut stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) if line == "\r\n" => break,
                Ok(_) => head.push(line.trim_end().to_string()),
            }
        }
        drop(reader);
        let Some(path) = head
            .first()
            .and_then(|request_line| request_line.split(' ').nth(1))
        else {
            return;
        };
        let headers = head
            .iter()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let request = RecordedRequest {
            path: path.to_string(),
            headers,
            client_certificate,
        };
        let response = routes
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_else(|| TestResponse::xml(404, Vec::new()));
        let not_modified = response.header("ETag").is_some()
            && response.header("ETag") == request.header("if-none-match");
        requests.lock().unwrap().push(request);
        let (status, body) = if not_modified {
            (304, &[][..])
        } else {
            (response.status, &response.body[..])
        };
        let mut head = format!("HTTP/1.1 {status} Test\r\nConnection: close\r\n");
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if status != 304 {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        // the client hangs up early on an oversized body
        if stream.write_all(head.as_bytes()).is_ok() && stream.write_all(body).is_ok() {
            let _ = stream.flush();
        }
    }

    struct TestIdentity {
        certificate_authority: rcgen::Certificate,
        server_certificate: rcgen::Certificate,
        server_key: KeyPair,
        client_certificate: rcgen::Certificate,
        directory: tempfile::TempDir,
    }

    impl TestIdentity {
        fn new() -> Self {
            let authority_key = KeyPair::generate().unwrap();
            let mut authority_parameters = CertificateParams::new(Vec::<String>::new()).unwrap();
            authority_parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            authority_parameters
                .distinguished_name
                .push(DnType::CommonName, "FLM exchange test CA");
            let certificate_authority = authority_parameters.self_signed(&authority_key).unwrap();

            let leaf = |name: &str, purpose: ExtendedKeyUsagePurpose| {
                let key = KeyPair::generate().unwrap();
                let mut parameters = CertificateParams::new(vec![name.to_string()]).unwrap();
                parameters.extended_key_usages = vec![purpose];
                let certificate = parameters
                    .signed_by(&key, &certificate_authority, &authority_key)
                    .unwrap();
                (certificate, key)
            };
            let (server_certificate, server_key) =
                leaf("127.0.0.1", ExtendedKeyUsagePurpose::ServerAuth);
            let (client_certificate, client_key) =
                leaf("distributor.test", ExtendedKeyUsagePurpose::ClientAuth);

            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("client.pem"),
                format!(
                    "{}{}",
                    client_certificate.pem(),
                    certificate_authority.pem()
                ),
            )
            .unwrap();
            std::fs::write(
                directory.path().join("client.key"),
                client_key.serialize_pem(),
            )
            .unwrap();
            Self {
                certificate_authority,
                server_certificate,
                server_key,
                client_certificate,
                directory,
            }
        }

        fn root_certificates(&self) -> RootCerts {
            RootCerts::new_with_certs(&[
                Certificate::from_der(self.certificate_authority.der()).to_owned()
            ])
        }

        fn client_authentication(&self) -> FeedAuthentication {
            FeedAuthentication::ClientCertificate {
                certificate_chain: self.directory.path().join("client.pem"),
                private_key: self.directory.path().join("client.key"),
            }
        }

        fn server_config(
            &self,
            require_client_certificate: bool,
            versions: &[&'static SupportedProtocolVersion],
        ) -> ServerConfig {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = ServerConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(versions)
                .unwrap();
            let builder = if require_client_certificate {
                let mut roots = RootCertStore::empty();
                roots.add(self.certificate_authority.der().clone()).unwrap();
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                        .build()
                        .unwrap();
                builder.with_client_cert_verifier(verifier)
            } else {
                builder.with_no_client_auth()
            };
            builder
                .with_single_cert(
                    vec![self.server_certificate.der().clone()],
                    PrivateKeyDer::try_from(self.server_key.serialize_der()).unwrap(),
                )
                .unwrap()
        }
    }

    fn refresh_trusting(
        identity: &TestIdentity,
        feed: &Feed,
        previous: &FeedSyncState,
    ) -> Result<FeedRefresh, String> {
        refresh_feed_with_root_certificates(feed, previous, identity.root_certificates())
    }

    fn fetched_ids(refresh: &FeedRefresh) -> Vec<&str> {
        refresh
            .fetched
            .iter()
            .map(|flm| flm.facility_id.as_str())
            .collect()
    }

    fn paths(requests: &[RecordedRequest]) -> Vec<&str> {
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect()
    }

    // Cinema-A at a relative href, the SMPTE example at an absolute path
    fn two_facility_server() -> TestServer {
        let server = TestServer::plain();
        server.route(
            "/feed/flm/a.xml",
            TestResponse::xml(200, cinema_flm("Cinema-A")),
        );
        server.route(
            "/flm/smpte.xml",
            TestResponse::xml(200, read(Path::new(SMPTE_EXAMPLE_FLM))),
        );
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[
                        (&facility_id("Cinema-A"), FIRST_MODIFIED, "flm/a.xml"),
                        (SMPTE_EXAMPLE_FACILITY_ID, FIRST_MODIFIED, "/flm/smpte.xml"),
                    ],
                ),
            )
            .with_header("ETag", "\"v1\"")
            .with_header("Last-Modified", LAST_MODIFIED_DATE),
        );
        server
    }

    #[test]
    fn the_first_refresh_fetches_every_flm_and_the_next_gets_not_modified() {
        let server = two_facility_server();
        let open_feed = feed(server.url(SITE_LIST_PATH), FeedAuthentication::None);

        let first = refresh_feed(&open_feed, &FeedSyncState::default()).unwrap();
        assert_eq!(
            fetched_ids(&first),
            [facility_id("Cinema-A").as_str(), SMPTE_EXAMPLE_FACILITY_ID]
        );
        assert!(first.failures.is_empty(), "{:?}", first.failures);
        assert_eq!(first.fetched[0].url, server.url("/feed/flm/a.xml"));
        for flm in &first.fetched {
            assert_eq!(
                parse_flm(&flm.xml).unwrap().facility.id.as_deref(),
                Some(flm.facility_id.as_str())
            );
        }
        assert_eq!(
            first.site_list.as_ref().unwrap().system_name,
            "Exhibitor TMS"
        );
        assert_eq!(first.state.site_list_etag.as_deref(), Some("\"v1\""));
        assert_eq!(
            first.state.site_list_last_modified.as_deref(),
            Some(LAST_MODIFIED_DATE)
        );
        assert_eq!(first.state.fetched_facility_modified.len(), 2);
        server.take_requests();

        let second = refresh_feed(&open_feed, &first.state).unwrap();
        assert_eq!(second.site_list, None);
        assert!(second.fetched.is_empty());
        assert_eq!(second.state, first.state);
        let requests = server.take_requests();
        assert_eq!(paths(&requests), [SITE_LIST_PATH]);
        assert_eq!(requests[0].header("if-none-match"), Some("\"v1\""));
        assert_eq!(
            requests[0].header("if-modified-since"),
            Some(LAST_MODIFIED_DATE)
        );
    }

    #[test]
    fn only_the_facility_whose_modified_instant_changed_is_fetched() {
        let server = two_facility_server();
        let open_feed = feed(server.url(SITE_LIST_PATH), FeedAuthentication::None);
        let first = refresh_feed(&open_feed, &FeedSyncState::default()).unwrap();
        server.take_requests();

        let changed_modified = "2026-10-02T09:00:00Z";
        // the same instant as FIRST_MODIFIED written with another offset
        let same_instant = "2026-10-01T11:00:00+01:00";
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[
                        (&facility_id("Cinema-A"), changed_modified, "flm/a.xml"),
                        (SMPTE_EXAMPLE_FACILITY_ID, same_instant, "/flm/smpte.xml"),
                    ],
                ),
            )
            .with_header("ETag", "\"v2\""),
        );
        let second = refresh_feed(&open_feed, &first.state).unwrap();
        assert_eq!(fetched_ids(&second), [facility_id("Cinema-A").as_str()]);
        assert_eq!(
            paths(&server.take_requests()),
            [SITE_LIST_PATH, "/feed/flm/a.xml"]
        );
        assert_eq!(
            second.state.fetched_facility_modified[&facility_id("Cinema-A")],
            parse_xml_date_time(changed_modified).unwrap()
        );
        assert_eq!(second.state.site_list_etag.as_deref(), Some("\"v2\""));
    }

    #[test]
    fn a_facility_missing_from_the_site_list_is_reported_removed() {
        let server = two_facility_server();
        let open_feed = feed(server.url(SITE_LIST_PATH), FeedAuthentication::None);
        let first = refresh_feed(&open_feed, &FeedSyncState::default()).unwrap();

        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[(&facility_id("Cinema-A"), FIRST_MODIFIED, "flm/a.xml")],
                ),
            )
            .with_header("ETag", "\"v2\""),
        );
        let second = refresh_feed(&open_feed, &first.state).unwrap();
        assert_eq!(second.removed_facility_ids, [SMPTE_EXAMPLE_FACILITY_ID]);
        assert!(second.fetched.is_empty());
        assert_eq!(
            second
                .state
                .fetched_facility_modified
                .keys()
                .collect::<Vec<_>>(),
            [&facility_id("Cinema-A")]
        );
    }

    #[test]
    fn a_gone_flm_surfaces_its_token_and_the_others_are_still_fetched() {
        let server = TestServer::plain();
        server.route("/flm/a.xml", TestResponse::xml(200, cinema_flm("Cinema-A")));
        server.route(
            "/flm/gone.xml",
            TestResponse::xml(410, error_xml("NoSuchFLM", "facility closed")),
        );
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[
                        (&facility_id("Cinema-Gone"), FIRST_MODIFIED, "/flm/gone.xml"),
                        (&facility_id("Cinema-A"), FIRST_MODIFIED, "/flm/a.xml"),
                    ],
                ),
            ),
        );
        let refresh = refresh_feed(
            &feed(server.url(SITE_LIST_PATH), FeedAuthentication::None),
            &FeedSyncState::default(),
        )
        .unwrap();
        assert_eq!(fetched_ids(&refresh), [facility_id("Cinema-A").as_str()]);
        assert_eq!(refresh.failures.len(), 1);
        let failure = &refresh.failures[0];
        assert_eq!(failure.facility_id, facility_id("Cinema-Gone"));
        assert_eq!(
            failure.reason,
            format!(
                "GET {} returned HTTP 410: NoSuchFLM: facility closed",
                server.url("/flm/gone.xml")
            )
        );
        assert!(
            !refresh
                .state
                .fetched_facility_modified
                .contains_key(&facility_id("Cinema-Gone"))
        );
    }

    #[test]
    fn a_facility_id_mismatch_fails_that_facility_only_and_is_retried() {
        let server = TestServer::plain();
        server.route("/flm/a.xml", TestResponse::xml(200, cinema_flm("Cinema-A")));
        server.route("/flm/b.xml", TestResponse::xml(200, cinema_flm("Cinema-C")));
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[
                        (&facility_id("Cinema-A"), FIRST_MODIFIED, "/flm/a.xml"),
                        (&facility_id("Cinema-B"), FIRST_MODIFIED, "/flm/b.xml"),
                    ],
                ),
            )
            .with_header("ETag", "\"v1\""),
        );
        let open_feed = feed(server.url(SITE_LIST_PATH), FeedAuthentication::None);
        let first = refresh_feed(&open_feed, &FeedSyncState::default()).unwrap();
        assert_eq!(fetched_ids(&first), [facility_id("Cinema-A").as_str()]);
        assert_eq!(first.failures.len(), 1);
        assert_eq!(first.failures[0].facility_id, facility_id("Cinema-B"));
        assert!(
            first.failures[0].reason.contains(&facility_id("Cinema-C")),
            "{}",
            first.failures[0].reason
        );
        assert_eq!(first.state.site_list_etag, None);
        assert!(
            !first
                .state
                .fetched_facility_modified
                .contains_key(&facility_id("Cinema-B"))
        );
        server.take_requests();

        server.route("/flm/b.xml", TestResponse::xml(200, cinema_flm("Cinema-B")));
        let second = refresh_feed(&open_feed, &first.state).unwrap();
        let requests = server.take_requests();
        assert_eq!(requests[0].header("if-none-match"), None);
        assert_eq!(paths(&requests), [SITE_LIST_PATH, "/flm/b.xml"]);
        assert_eq!(fetched_ids(&second), [facility_id("Cinema-B").as_str()]);
        assert_eq!(second.state.site_list_etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn credentials_over_http_are_refused_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let http_url = format!("http://{address}{SITE_LIST_PATH}");
        let identity_directory = tempfile::tempdir().unwrap();
        let feeds = [
            feed(http_url.clone(), basic()),
            feed(
                http_url,
                FeedAuthentication::ClientCertificate {
                    certificate_chain: identity_directory.path().join("absent.pem"),
                    private_key: identity_directory.path().join("absent.key"),
                },
            ),
        ];
        for refused in &feeds {
            let error = refresh_feed(refused, &FeedSyncState::default()).unwrap_err();
            assert!(error.contains("only sent over https"), "{error}");
        }
        let in_url = feed(
            format!("http://{USERNAME}:{PASSWORD}@{address}{SITE_LIST_PATH}"),
            FeedAuthentication::None,
        );
        let error = refresh_feed(&in_url, &FeedSyncState::default()).unwrap_err();
        assert!(error.contains("username or password"), "{error}");
        assert!(!error.contains(PASSWORD), "{error}");

        listener.set_nonblocking(true).unwrap();
        let accepted = listener.accept().map(|_| ()).map_err(|error| error.kind());
        assert_eq!(accepted, Err(ErrorKind::WouldBlock));
    }

    #[test]
    fn basic_credentials_are_sent_over_tls_to_the_site_list_origin() {
        let identity = TestIdentity::new();
        let server = TestServer::tls(&identity, false);
        server.route("/flm/a.xml", TestResponse::xml(200, cinema_flm("Cinema-A")));
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[(&facility_id("Cinema-A"), FIRST_MODIFIED, "/flm/a.xml")],
                ),
            ),
        );
        let refresh = refresh_trusting(
            &identity,
            &feed(server.url(SITE_LIST_PATH), basic()),
            &FeedSyncState::default(),
        )
        .unwrap();
        assert_eq!(fetched_ids(&refresh), [facility_id("Cinema-A").as_str()]);
        let requests = server.take_requests();
        assert_eq!(paths(&requests), [SITE_LIST_PATH, "/flm/a.xml"]);
        for request in &requests {
            assert_eq!(
                request.header("authorization"),
                Some(basic_authorization().as_str())
            );
        }
    }

    #[test]
    fn the_client_certificate_is_presented_over_tls() {
        let identity = TestIdentity::new();
        let server = TestServer::tls(&identity, true);
        server.route("/flm/a.xml", TestResponse::xml(200, cinema_flm("Cinema-A")));
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[(&facility_id("Cinema-A"), FIRST_MODIFIED, "/flm/a.xml")],
                ),
            ),
        );
        let refresh = refresh_trusting(
            &identity,
            &feed(server.url(SITE_LIST_PATH), identity.client_authentication()),
            &FeedSyncState::default(),
        )
        .unwrap();
        assert_eq!(fetched_ids(&refresh), [facility_id("Cinema-A").as_str()]);
        let requests = server.take_requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(
                request.client_certificate.as_deref(),
                Some(identity.client_certificate.der().as_ref())
            );
            assert_eq!(request.header("authorization"), None);
        }
    }

    #[test]
    fn a_key_that_does_not_match_the_certificate_is_refused_without_a_panic() {
        let identity = TestIdentity::new();
        let other_key = identity.directory.path().join("other.key");
        std::fs::write(&other_key, KeyPair::generate().unwrap().serialize_pem()).unwrap();
        let mismatched = FeedAuthentication::ClientCertificate {
            certificate_chain: identity.directory.path().join("client.pem"),
            private_key: other_key,
        };
        let error = refresh_trusting(
            &identity,
            &feed("https://127.0.0.1:9/sitelist.xml".to_string(), mismatched),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert!(
            error.contains("does not fit the certificate chain"),
            "{error}"
        );
        assert!(!error.contains("PRIVATE KEY"), "{error}");
    }

    #[test]
    fn a_server_certificate_outside_the_webpki_roots_is_refused() {
        let identity = TestIdentity::new();
        let server = TestServer::tls(&identity, false);
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(200, site_list_xml(SITE_LIST_NAMESPACE, &[])),
        );
        let error = refresh_feed(
            &feed(server.url(SITE_LIST_PATH), basic()),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert!(error.contains("certificate"), "{error}");
        assert!(server.take_requests().is_empty());
    }

    #[test]
    fn tls_1_2_and_1_3_servers_are_both_accepted() {
        let identity = TestIdentity::new();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let server = TestServer::start(
                HTTPS_SCHEME,
                Some(Arc::new(identity.server_config(false, &[version]))),
            );
            server.route(
                SITE_LIST_PATH,
                TestResponse::xml(200, site_list_xml(SITE_LIST_NAMESPACE, &[])),
            );
            let refresh = refresh_trusting(
                &identity,
                &feed(server.url(SITE_LIST_PATH), FeedAuthentication::None),
                &FeedSyncState::default(),
            );
            assert!(refresh.is_ok(), "{:?}: {refresh:?}", version.version);
        }
    }

    #[test]
    fn an_http_flm_href_in_an_https_site_list_is_refused() {
        let identity = TestIdentity::new();
        let secure = TestServer::tls(&identity, false);
        let plain = TestServer::plain();
        plain.route("/flm/b.xml", TestResponse::xml(200, cinema_flm("Cinema-B")));
        secure.route("/flm/a.xml", TestResponse::xml(200, cinema_flm("Cinema-A")));
        let plain_href = plain.url("/flm/b.xml");
        secure.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[
                        (&facility_id("Cinema-B"), FIRST_MODIFIED, &plain_href),
                        (&facility_id("Cinema-A"), FIRST_MODIFIED, "/flm/a.xml"),
                    ],
                ),
            ),
        );
        let refresh = refresh_trusting(
            &identity,
            &feed(secure.url(SITE_LIST_PATH), FeedAuthentication::None),
            &FeedSyncState::default(),
        )
        .unwrap();
        assert_eq!(fetched_ids(&refresh), [facility_id("Cinema-A").as_str()]);
        assert_eq!(refresh.failures.len(), 1);
        assert_eq!(refresh.failures[0].facility_id, facility_id("Cinema-B"));
        assert!(
            refresh.failures[0].reason.contains("leaves https"),
            "{}",
            refresh.failures[0].reason
        );
        assert!(plain.take_requests().is_empty());
    }

    #[test]
    fn an_https_to_http_redirect_is_refused() {
        let identity = TestIdentity::new();
        let secure = TestServer::tls(&identity, false);
        let plain = TestServer::plain();
        plain.route(
            SITE_LIST_PATH,
            TestResponse::xml(200, site_list_xml(SITE_LIST_NAMESPACE, &[])),
        );
        secure.route(
            SITE_LIST_PATH,
            TestResponse::redirect(302, &plain.url(SITE_LIST_PATH)),
        );
        let error = refresh_trusting(
            &identity,
            &feed(secure.url(SITE_LIST_PATH), FeedAuthentication::None),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert!(error.contains("leaves https"), "{error}");
        assert!(plain.take_requests().is_empty());
    }

    #[test]
    fn a_redirect_never_carries_credentials_to_another_origin() {
        let identity = TestIdentity::new();
        let feed_server = TestServer::tls(&identity, false);
        let other_server = TestServer::tls(&identity, false);
        other_server.route(
            SITE_LIST_PATH,
            TestResponse::xml(200, site_list_xml(SITE_LIST_NAMESPACE, &[])),
        );
        feed_server.route(
            SITE_LIST_PATH,
            TestResponse::redirect(307, &other_server.url(SITE_LIST_PATH)),
        );
        let error = refresh_trusting(
            &identity,
            &feed(feed_server.url(SITE_LIST_PATH), basic()),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert!(error.contains("refused the redirect"), "{error}");
        assert!(other_server.take_requests().is_empty());
    }

    #[test]
    fn an_flm_on_another_origin_fails_when_the_feed_has_credentials() {
        let identity = TestIdentity::new();
        let feed_server = TestServer::tls(&identity, false);
        let other_server = TestServer::tls(&identity, false);
        other_server.route("/flm/b.xml", TestResponse::xml(200, cinema_flm("Cinema-B")));
        feed_server.route("/flm/a.xml", TestResponse::xml(200, cinema_flm("Cinema-A")));
        let other_href = other_server.url("/flm/b.xml");
        feed_server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[
                        (&facility_id("Cinema-B"), FIRST_MODIFIED, &other_href),
                        (&facility_id("Cinema-A"), FIRST_MODIFIED, "/flm/a.xml"),
                    ],
                ),
            ),
        );
        let refresh = refresh_trusting(
            &identity,
            &feed(feed_server.url(SITE_LIST_PATH), basic()),
            &FeedSyncState::default(),
        )
        .unwrap();
        assert_eq!(fetched_ids(&refresh), [facility_id("Cinema-A").as_str()]);
        assert_eq!(refresh.failures.len(), 1);
        assert_eq!(refresh.failures[0].facility_id, facility_id("Cinema-B"));
        assert!(other_server.take_requests().is_empty());
    }

    #[test]
    fn a_moved_site_list_is_followed_and_its_hrefs_resolve_against_the_new_url() {
        let server = TestServer::plain();
        server.route(
            "/old/sitelist.xml",
            TestResponse::redirect(301, "/new/sitelist.xml"),
        );
        server.route(
            "/new/sitelist.xml",
            TestResponse::xml(
                200,
                site_list_xml(
                    SITE_LIST_NAMESPACE,
                    &[(&facility_id("Cinema-A"), FIRST_MODIFIED, "flm/a.xml")],
                ),
            ),
        );
        server.route(
            "/new/flm/a.xml",
            TestResponse::xml(200, cinema_flm("Cinema-A")),
        );
        let refresh = refresh_feed(
            &feed(server.url("/old/sitelist.xml"), FeedAuthentication::None),
            &FeedSyncState::default(),
        )
        .unwrap();
        assert!(refresh.failures.is_empty(), "{:?}", refresh.failures);
        assert_eq!(refresh.fetched[0].url, server.url("/new/flm/a.xml"));
    }

    #[test]
    fn server_error_text_names_the_token_status_and_url_but_not_the_password() {
        let identity = TestIdentity::new();
        let server = TestServer::tls(&identity, false);
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(401, error_xml("NotAuthorized", "unknown distributor")),
        );
        let site_list_url = server.url(SITE_LIST_PATH);
        let error = refresh_trusting(
            &identity,
            &feed(site_list_url.clone(), basic()),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            format!("GET {site_list_url} returned HTTP 401: NotAuthorized: unknown distributor")
        );
        assert!(!error.contains(PASSWORD));
        assert!(!error.contains(&basic_authorization()));
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        let server = TestServer::plain();
        let oversized = vec![b' '; usize::try_from(MAXIMUM_RESPONSE_BYTES).unwrap() + 1];
        server.route(SITE_LIST_PATH, TestResponse::xml(200, oversized));
        let error = refresh_feed(
            &feed(server.url(SITE_LIST_PATH), FeedAuthentication::None),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert!(
            error.contains(&format!("larger than {MAXIMUM_RESPONSE_BYTES} bytes")),
            "{error}"
        );
    }

    #[test]
    fn a_site_list_in_another_namespace_is_refused_by_name() {
        let server = TestServer::plain();
        server.route(
            SITE_LIST_PATH,
            TestResponse::xml(
                200,
                site_list_xml(
                    ISDCF_SITE_LIST_NAMESPACE,
                    &[(&facility_id("Cinema-A"), FIRST_MODIFIED, "flm/a.xml")],
                ),
            ),
        );
        let error = refresh_feed(
            &feed(server.url(SITE_LIST_PATH), FeedAuthentication::None),
            &FeedSyncState::default(),
        )
        .unwrap_err();
        assert!(error.contains(ISDCF_SITE_LIST_NAMESPACE), "{error}");
        assert_eq!(server.take_requests().len(), 1);
    }

    #[test]
    fn debug_never_prints_the_password() {
        let printed = format!("{:?}", feed("https://feed.test/".to_string(), basic()));
        assert!(!printed.contains(PASSWORD), "{printed}");
        assert!(printed.contains(REDACTED));
        assert!(printed.contains(USERNAME));
    }
}
