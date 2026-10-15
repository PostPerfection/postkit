// a projector or media block certificate downloaded by vendor and serial through the system curl
use crate::certificate::{cert_info_from_pem, chain_from_pem, chain_to_pem};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

pub const SYSTEM_CURL: &str = "curl";
const SFTP_SCHEME: &str = "sftp://";
const CURL_PROTOCOLS_PREFIX: &str = "Protocols:";
const SFTP_PROTOCOL: &str = "sftp";
const DOLBY_LEAF_SUFFIX: &str = ".cert.sha256.pem";
// the archive names DCP-o-matic looks for beside a Dolby or Doremi leaf
const DOLBY_ARCHIVE_SUFFIXES: [&str; 2] = [".dcicerts.zip", ".certs.zip"];
const DOLBY_CHAIN_MEMBER: &str = "chain.sha256.pem";
const MAXIMUM_ARCHIVE_BYTES: usize = 1024 * 1024;
const MAXIMUM_CHAIN_BYTES: u64 = 256 * 1024;
const CURL_FILE_TOO_LARGE: i32 = 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Dolby,
    Qube,
    Christie,
    Gdc,
    Barco,
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Vendor::Dolby => "Dolby",
            Vendor::Qube => "Qube",
            Vendor::Christie => "Christie",
            Vendor::Gdc => "GDC",
            Vendor::Barco => "Barco",
        })
    }
}

#[derive(Clone)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorServers {
    pub dolby: String,
    pub qube: String,
    pub christie: String,
    pub gdc: String,
    pub barco: String,
}

impl Default for VendorServers {
    fn default() -> Self {
        VendorServers {
            dolby: "ftp://ftp.cinema.dolby.com/Certificates".to_string(),
            qube: "ftp://certificates.qubecinema.com".to_string(),
            christie: "ftp://certificates.christiedigital.com".to_string(),
            gdc: "ftp://ftp.gdc-tech.com".to_string(),
            barco: "sftp://certificates.barco.com".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CertificateRequest<'a> {
    pub vendor: Vendor,
    pub serial: &'a str,
    // qube only, the device type such as QXPD
    pub qube_type: Option<&'a str>,
    pub credentials: Option<&'a Credentials>,
}

pub fn parse_vendor(name: &str) -> Result<Vendor, String> {
    match name.to_ascii_lowercase().as_str() {
        "dolby" | "doremi" => Ok(Vendor::Dolby),
        "qube" => Ok(Vendor::Qube),
        "christie" => Ok(Vendor::Christie),
        "gdc" | "gdc-tech" => Ok(Vendor::Gdc),
        "barco" => Ok(Vendor::Barco),
        v @ ("sony" | "nec") => Err(format!(
            "automated download for {v} is not available; obtain the certificate from the vendor"
        )),
        other => Err(format!(
            "unknown vendor '{other}'; obtain the certificate from the vendor"
        )),
    }
}

pub fn needs_credentials(vendor: Vendor) -> bool {
    matches!(vendor, Vendor::Christie | Vendor::Gdc | Vendor::Barco)
}

// nothing but plain serial characters reaches curl or a URL path
pub fn validate_serial(serial: &str) -> Result<(), String> {
    if serial.is_empty()
        || !serial
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(format!(
            "invalid serial '{serial}': use letters, digits and '-' only"
        ));
    }
    Ok(())
}

fn dolby_dir_url(base: &str, serial: &str) -> Result<String, String> {
    let digits: String = serial.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 3 {
        return Err(format!(
            "dolby serial '{serial}' must start with at least 3 digits"
        ));
    }
    Ok(format!("{base}/{}xxx/", &digits[..3]))
}

fn dolby_pick(entries: &[String], serial: &str) -> Option<String> {
    let want = format!("-{serial}{DOLBY_LEAF_SUFFIX}");
    entries.iter().find(|e| e.ends_with(&want)).cloned()
}

fn dolby_archive_pick(entries: &[String], leaf_file: &str) -> Option<String> {
    let stem = leaf_file.strip_suffix(DOLBY_LEAF_SUFFIX)?;
    DOLBY_ARCHIVE_SUFFIXES
        .iter()
        .map(|suffix| format!("{stem}{suffix}"))
        .find(|name| entries.contains(name))
}

// None when the archive holds no chain, an error when its chain starts with another certificate
fn chain_from_archive(
    archive: &[u8],
    archive_name: &str,
    leaf_pem: &str,
) -> Result<Option<String>, String> {
    use std::io::Read;
    if archive.len() > MAXIMUM_ARCHIVE_BYTES {
        return Err(format!(
            "{archive_name} is larger than {MAXIMUM_ARCHIVE_BYTES} bytes"
        ));
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|e| format!("{archive_name} is not a ZIP: {e}"))?;
    let mut chain_text = None;
    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .map_err(|e| format!("{archive_name}: {e}"))?;
        if entry.name().rsplit('/').next() != Some(DOLBY_CHAIN_MEMBER) {
            continue;
        }
        let mut text = String::new();
        entry
            .take(MAXIMUM_CHAIN_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("{archive_name}: {DOLBY_CHAIN_MEMBER}: {e}"))?;
        if text.len() as u64 > MAXIMUM_CHAIN_BYTES {
            return Err(format!(
                "{archive_name}: {DOLBY_CHAIN_MEMBER} is larger than {MAXIMUM_CHAIN_BYTES} bytes"
            ));
        }
        chain_text = Some(text);
        break;
    }
    let Some(chain_text) = chain_text else {
        return Ok(None);
    };
    let chain = chain_from_pem(&chain_text)
        .map_err(|e| format!("{archive_name}: {DOLBY_CHAIN_MEMBER}: {e}"))?;
    let leaf = chain_from_pem(leaf_pem)?;
    if chain[0].der != leaf[0].der {
        return Err(format!(
            "the chain in {archive_name} does not start with the certificate listed beside it"
        ));
    }
    Ok(Some(chain_to_pem(&chain)))
}

fn qube_dir_url(base: &str, qtype: &str) -> String {
    format!("{base}/SMPTE-{qtype}/")
}

fn qube_pad(serial: &str) -> String {
    if serial.chars().all(|c| c.is_ascii_digit()) && serial.len() < 5 {
        format!("{serial:0>5}")
    } else {
        serial.to_string()
    }
}

// the newest sha256 certificate named "<type>-<padded serial>-"
fn qube_pick(entries: &[String], qtype: &str, serial: &str) -> Option<String> {
    let prefix = format!("{qtype}-{}-", qube_pad(serial));
    let mut matches: Vec<&String> = entries
        .iter()
        .filter(|e| e.starts_with(&prefix) && e.contains("sha256"))
        .collect();
    matches.sort();
    matches.last().map(|s| (*s).clone())
}

// F-IMB first, then IMB-S2
fn christie_paths(serial: &str) -> Result<Vec<String>, String> {
    if !serial.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("christie serial '{serial}' must be digits only"));
    }
    if serial.len() > 12 {
        return Err(format!(
            "christie serial '{serial}' is longer than 12 digits"
        ));
    }
    let s = format!("{serial:0>12}");
    Ok(vec![
        format!("/Certificates/F-IMB/F-IMB_{s}_sha256.pem"),
        format!("/Certificates/IMB-S2/IMB-S2_{s}_sha256.pem"),
    ])
}

fn gdc_path(serial: &str) -> String {
    format!("/SHA256/{serial}.crt.pem")
}

fn barco_path(serial: &str) -> Result<String, String> {
    if serial.len() != 10 {
        return Err(format!(
            "barco serial '{serial}' must be exactly 10 characters"
        ));
    }
    Ok(format!(
        "/{}xxx/{serial}/Barco-ICMP.{serial}_cert.pem",
        &serial[..7]
    ))
}

fn curl_config_quote(value: &str) -> String {
    let mut quoted = String::new();
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{0b}' => quoted.push_str("\\v"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn curl_config(url: &str, creds: &Credentials) -> String {
    format!(
        "url = {}\nuser = {}\n",
        curl_config_quote(url),
        curl_config_quote(&format!("{}:{}", creds.user, creds.password))
    )
}

fn curl_failure(code: i32) -> &'static str {
    match code {
        6 => "could not resolve host",
        7 => "could not connect to host",
        28 => "connection timed out",
        9 | 19 | 78 => "file not found on server",
        CURL_FILE_TOO_LARGE => "the file is larger than allowed",
        67 => "authentication failed (check the vendor user and password)",
        _ => "download failed",
    }
}

// the stock curl on Windows and macOS is built without sftp
fn missing_protocol_error(curl_version: &str, protocol: &str) -> Option<String> {
    let supported = curl_version
        .lines()
        .filter_map(|line| line.strip_prefix(CURL_PROTOCOLS_PREFIX))
        .flat_map(str::split_whitespace)
        .any(|name| name == protocol);
    (!supported).then(|| {
        format!(
            "the curl on this computer cannot download over {protocol}, which the Barco \
             certificate server needs. The curl that comes with Windows and macOS has no \
             {protocol}: install a curl built with {protocol} (curl.se/download, or Homebrew's \
             curl on macOS) and put it first on PATH, or get the certificate from Barco"
        )
    })
}

fn require_protocol(protocol: &str) -> Result<(), String> {
    let output = Command::new(SYSTEM_CURL)
        .arg("-V")
        .output()
        .map_err(|e| format!("cannot run curl (is it installed?): {e}"))?;
    match missing_protocol_error(&String::from_utf8_lossy(&output.stdout), protocol) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

// the credentials reach curl in a config on stdin, never argv or a log
fn curl_with_credentials(url: &str, creds: &Credentials) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut command = Command::new(SYSTEM_CURL);
    command
        .arg("-s")
        .args(["--connect-timeout", "15"])
        .args(["--max-time", "45"]);
    // skips the sftp host key check, as dcp-o-matic does
    if url.starts_with(SFTP_SCHEME) {
        command.arg("-k");
    }
    command
        .arg("-K")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run curl (is it installed?): {e}"))?;
    child
        .stdin
        .take()
        .ok_or("cannot pass credentials to curl")?
        .write_all(curl_config(url, creds).as_bytes())
        .map_err(|e| format!("cannot write curl config: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let code = out.status.code().unwrap_or(-1);
    Err(format!(
        "{} at {url} (curl exit {code})",
        curl_failure(code)
    ))
}

fn curl(url: &str, extra_arguments: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new(SYSTEM_CURL);
    command
        .arg("-s")
        .args(["--connect-timeout", "15"])
        .args(["--max-time", "45"])
        .args(extra_arguments)
        .arg(url);
    let out = command
        .output()
        .map_err(|e| format!("cannot run curl (is it installed?): {e}"))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let code = out.status.code().unwrap_or(-1);
    Err(format!("{} (curl exit {code})", curl_failure(code)))
}

fn listing(url: &str) -> Result<Vec<String>, String> {
    Ok(String::from_utf8_lossy(&curl(url, &["-l"])?)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn fetch_credentialed(
    request: &CertificateRequest<'_>,
    servers: &VendorServers,
    creds: &Credentials,
) -> Result<String, String> {
    let serial = request.serial;
    let (base, paths) = match request.vendor {
        Vendor::Christie => (&servers.christie, christie_paths(serial)?),
        Vendor::Gdc => (&servers.gdc, vec![gdc_path(serial)]),
        Vendor::Barco => (&servers.barco, vec![barco_path(serial)?]),
        Vendor::Dolby | Vendor::Qube => unreachable!("not a credentialed vendor"),
    };
    if base.starts_with(SFTP_SCHEME) {
        require_protocol(SFTP_PROTOCOL)?;
    }
    let mut last_err = String::new();
    for path in &paths {
        match curl_with_credentials(&format!("{base}{path}"), creds) {
            Ok(bytes) => return Ok(String::from_utf8_lossy(&bytes).to_string()),
            Err(e) => last_err = e,
        }
    }
    Err(format!("no certificate for serial '{serial}': {last_err}"))
}

// a Dolby leaf comes with its chain when an archive beside it holds one
fn fetch_anonymous(
    request: &CertificateRequest<'_>,
    servers: &VendorServers,
) -> Result<String, String> {
    let serial = request.serial;
    let (dir_url, entries, file) = match request.vendor {
        Vendor::Dolby => {
            let dir = dolby_dir_url(&servers.dolby, serial)?;
            let entries = listing(&dir)?;
            let file = dolby_pick(&entries, serial)
                .ok_or_else(|| format!("no certificate for dolby serial '{serial}' at {dir}"))?;
            (dir, entries, file)
        }
        Vendor::Qube => {
            let qtype = request
                .qube_type
                .ok_or("qube needs the device type, such as QXPD")?;
            validate_serial(qtype).map_err(|_| {
                format!("invalid device type '{qtype}': use letters, digits and '-' only")
            })?;
            let dir = qube_dir_url(&servers.qube, qtype);
            let entries = listing(&dir)?;
            let file = qube_pick(&entries, qtype, serial).ok_or_else(|| {
                format!("no certificate for qube {qtype} serial '{serial}' at {dir}")
            })?;
            (dir, entries, file)
        }
        Vendor::Christie | Vendor::Gdc | Vendor::Barco => {
            unreachable!("a credentialed vendor")
        }
    };
    let leaf_pem = String::from_utf8_lossy(&curl(&format!("{dir_url}{file}"), &[])?).to_string();
    let archive_name = match request.vendor {
        Vendor::Dolby => dolby_archive_pick(&entries, &file),
        _ => None,
    };
    let Some(archive_name) = archive_name else {
        return Ok(leaf_pem);
    };
    let maximum_bytes = MAXIMUM_ARCHIVE_BYTES.to_string();
    let archive = curl(
        &format!("{dir_url}{archive_name}"),
        &["--max-filesize", &maximum_bytes],
    )?;
    Ok(chain_from_archive(&archive, &archive_name, &leaf_pem)?.unwrap_or(leaf_pem))
}

// the downloaded PEM, refused unless it parses as X.509
pub fn fetch_pem(
    request: &CertificateRequest<'_>,
    servers: &VendorServers,
) -> Result<String, String> {
    validate_serial(request.serial)?;
    let pem = if needs_credentials(request.vendor) {
        let creds = request.credentials.ok_or_else(|| {
            format!(
                "{} needs a vendor account: set its user and password",
                request.vendor
            )
        })?;
        fetch_credentialed(request, servers, creds)?
    } else {
        fetch_anonymous(request, servers)?
    };
    cert_info_from_pem(&pem)?;
    Ok(pem)
}

// writes the certificate and returns "subject / serial"
pub fn fetch(
    request: &CertificateRequest<'_>,
    servers: &VendorServers,
    out: &Path,
) -> Result<String, String> {
    let pem = fetch_pem(request, servers)?;
    let info = cert_info_from_pem(&pem)?;
    crate::fs::write_atomic(out, pem.as_bytes())?;
    Ok(format!("{} / {}", info.subject_cn, info.serial))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::test_support::{fixtures, read};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};

    const BASIC_AUTHORIZATION_PREFIX: &str = "Authorization: Basic ";
    const DOLBY_SERIAL: &str = "218281828";
    const DOLBY_FILE: &str = "Dolby-DCP2000-218281828.cert.sha256.pem";
    const DOLBY_ARCHIVE: &str = "Dolby-DCP2000-218281828.dcicerts.zip";
    const GDC_SERIAL: &str = "H1234";

    #[derive(Default)]
    struct Requests {
        paths: Vec<String>,
        authorizations: Vec<String>,
    }

    // plain http on localhost standing in for the vendor servers, each path answers its body
    struct VendorServer {
        origin: String,
        requests: Arc<Mutex<Requests>>,
    }

    impl VendorServer {
        fn start(bodies: HashMap<String, Vec<u8>>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Requests::default()));
            let recorded = requests.clone();
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let path = request_line.split(' ').nth(1).unwrap_or_default();
                    let mut recorded = recorded.lock().unwrap();
                    recorded.paths.push(path.to_string());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line.trim_end().is_empty() {
                            break;
                        }
                        if let Some(value) =
                            line.trim_end().strip_prefix(BASIC_AUTHORIZATION_PREFIX)
                        {
                            recorded.authorizations.push(value.to_string());
                        }
                    }
                    let (status, body) = match bodies.get(path) {
                        Some(body) => ("200 OK", body.as_slice()),
                        None => ("404 Not Found", &[][..]),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                }
            });
            Self { origin, requests }
        }

        fn servers(&self) -> VendorServers {
            VendorServers {
                dolby: format!("{}/Certificates", self.origin),
                qube: self.origin.clone(),
                christie: self.origin.clone(),
                gdc: self.origin.clone(),
                barco: self.origin.clone(),
            }
        }

        fn paths(&self) -> Vec<String> {
            self.requests.lock().unwrap().paths.clone()
        }
    }

    fn dolby_server(certificate_body: String) -> VendorServer {
        VendorServer::start(HashMap::from([
            (
                "/Certificates/218xxx/".to_string(),
                format!("Dolby-DCP2000-218281828.cert.mpeg.pem\n{DOLBY_FILE}\n").into_bytes(),
            ),
            (
                format!("/Certificates/218xxx/{DOLBY_FILE}"),
                certificate_body.into_bytes(),
            ),
        ]))
    }

    fn zip_of(members: &[(&str, String)]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buffer);
        for (name, text) in members {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        buffer.into_inner()
    }

    // the folder layout seen on Dolby's server: the leaf PEM and a ZIP holding leaf and chain
    fn dolby_archive_server(leaf: &str, chain: String) -> VendorServer {
        VendorServer::start(HashMap::from([
            (
                "/Certificates/218xxx/".to_string(),
                format!("{DOLBY_FILE}\n{DOLBY_ARCHIVE}\n").into_bytes(),
            ),
            (
                format!("/Certificates/218xxx/{DOLBY_FILE}"),
                leaf.as_bytes().to_vec(),
            ),
            (
                format!("/Certificates/218xxx/{DOLBY_ARCHIVE}"),
                zip_of(&[
                    ("cert.sha256.pem", leaf.to_string()),
                    ("chain.sha256.pem", chain),
                ]),
            ),
        ]))
    }

    #[test]
    fn a_dolby_archive_beside_the_leaf_stores_the_whole_chain_leaf_first() {
        let f = fixtures();
        let leaf = read(&f.security_managers[0].certificate);
        let chain = f.vendor_chain_pem(&f.security_managers[0].certificate);
        let server = dolby_archive_server(&leaf, chain.clone());
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("screen.pem");

        fetch(&dolby_request(), &server.servers(), &out).unwrap();
        let stored = chain_from_pem(&read(&out)).unwrap();
        let expected = chain_from_pem(&chain).unwrap();
        assert_eq!(stored, expected);
        assert_eq!(stored[0].der, chain_from_pem(&leaf).unwrap()[0].der);
        assert_eq!(
            server.paths().last().unwrap(),
            &format!("/Certificates/218xxx/{DOLBY_ARCHIVE}")
        );
    }

    #[test]
    fn a_dolby_archive_whose_chain_starts_with_another_certificate_is_refused() {
        let f = fixtures();
        let leaf = read(&f.security_managers[0].certificate);
        let other_chain = f.vendor_chain_pem(&f.security_managers[1].certificate);
        let server = dolby_archive_server(&leaf, other_chain);
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("screen.pem");

        let error = fetch(&dolby_request(), &server.servers(), &out).unwrap_err();
        assert_eq!(
            error,
            format!(
                "the chain in {DOLBY_ARCHIVE} does not start with the certificate listed beside it"
            )
        );
        assert!(!out.exists());
    }

    fn dolby_request() -> CertificateRequest<'static> {
        CertificateRequest {
            vendor: Vendor::Dolby,
            serial: DOLBY_SERIAL,
            qube_type: None,
            credentials: None,
        }
    }

    #[test]
    fn a_dolby_certificate_is_listed_downloaded_and_written() {
        let pem = read(&fixtures().security_managers[0].certificate);
        let server = dolby_server(pem.clone());
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("screen.pem");

        let summary = fetch(&dolby_request(), &server.servers(), &out).unwrap();
        assert_eq!(read(&out), pem);
        let info = cert_info_from_pem(&pem).unwrap();
        assert_eq!(summary, format!("{} / {}", info.subject_cn, info.serial));
        assert_eq!(
            server.paths(),
            [
                "/Certificates/218xxx/".to_string(),
                format!("/Certificates/218xxx/{DOLBY_FILE}")
            ]
        );
    }

    #[test]
    fn a_download_that_is_not_a_certificate_is_refused_and_nothing_written() {
        let server = dolby_server("<html>maintenance</html>".to_string());
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("screen.pem");
        assert!(fetch(&dolby_request(), &server.servers(), &out).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn a_credentialed_vendor_without_an_account_is_refused_before_curl_runs() {
        let server = VendorServer::start(HashMap::new());
        let directory = tempfile::tempdir().unwrap();
        let out = directory.path().join("screen.pem");
        let request = CertificateRequest {
            vendor: Vendor::Christie,
            serial: "218281",
            qube_type: None,
            credentials: None,
        };
        let error = fetch(&request, &server.servers(), &out).unwrap_err();
        assert_eq!(
            error,
            "Christie needs a vendor account: set its user and password"
        );
        assert!(server.paths().is_empty());
        assert!(!out.exists());
    }

    #[test]
    fn a_credentialed_download_sends_the_account_and_writes_the_certificate() {
        use base64::Engine;
        let pem = read(&fixtures().security_managers[1].certificate);
        let server = VendorServer::start(HashMap::from([(
            gdc_path(GDC_SERIAL),
            pem.clone().into_bytes(),
        )]));
        let credentials = Credentials {
            user: "projectionist".into(),
            password: "hunter2".into(),
        };
        let request = CertificateRequest {
            vendor: Vendor::Gdc,
            serial: GDC_SERIAL,
            qube_type: None,
            credentials: Some(&credentials),
        };
        assert_eq!(fetch_pem(&request, &server.servers()).unwrap(), pem);
        let authorizations = server.requests.lock().unwrap().authorizations.clone();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&authorizations[0])
            .unwrap();
        assert_eq!(decoded, b"projectionist:hunter2");
    }

    #[test]
    fn a_curl_without_sftp_is_named_plainly() {
        let windows = "curl 8.13.0 (Windows) libcurl/8.13.0 Schannel zlib/1.3.1 WinIDN\n\
            Release-Date: 2025-04-02\n\
            Protocols: dict file ftp ftps http https imap imaps ipfs ipns mqtt pop3 pop3s smb smbs smtp smtps telnet tftp ws wss\n";
        let macos = "curl 8.7.1 (x86_64-apple-darwin23.0) libcurl/8.7.1 (SecureTransport) LibreSSL/3.3.6\n\
            Protocols: dict file ftp ftps gopher gophers http https imap imaps ipfs ipns ldap ldaps mqtt pop3 pop3s rtsp smb smbs smtp smtps telnet tftp\n";
        for version in [windows, macos] {
            let error = missing_protocol_error(version, SFTP_PROTOCOL).unwrap();
            assert!(
                error.starts_with("the curl on this computer cannot download over sftp"),
                "{error}"
            );
        }
        let linux = "curl 8.15.0 (x86_64-redhat-linux-gnu) libcurl/8.15.0 libssh/0.11.5\n\
            Protocols: dict file ftp ftps http https scp sftp smtp\n";
        assert_eq!(missing_protocol_error(linux, SFTP_PROTOCOL), None);
    }

    #[test]
    fn vendor_parsing_routes_correctly() {
        assert_eq!(parse_vendor("dolby").unwrap(), Vendor::Dolby);
        assert_eq!(parse_vendor("Doremi").unwrap(), Vendor::Dolby);
        assert_eq!(parse_vendor("qube").unwrap(), Vendor::Qube);
        let e = parse_vendor("sony").unwrap_err();
        assert!(e.contains("obtain the certificate from the vendor"));
        assert!(parse_vendor("acme").is_err());
    }

    #[test]
    fn serial_validation_blocks_odd_input() {
        assert!(validate_serial("218281828").is_ok());
        assert!(validate_serial("H123-45").is_ok());
        assert!(validate_serial("").is_err());
        assert!(validate_serial("../etc/passwd").is_err());
        assert!(validate_serial("a b").is_err());
    }

    #[test]
    fn dolby_dir_and_pick() {
        let base = VendorServers::default().dolby;
        assert_eq!(
            dolby_dir_url(&base, "218281828").unwrap(),
            "ftp://ftp.cinema.dolby.com/Certificates/218xxx/"
        );
        assert!(dolby_dir_url(&base, "ab").is_err());
        let entries = vec![
            "Dolby-DCP2000-218281828.cert.mpeg.pem".to_string(),
            "Dolby-DCP2000-218281828.cert.sha256.pem".to_string(),
            "Dolby-DCP2000-218281828.dcicerts.zip".to_string(),
        ];
        assert_eq!(
            dolby_pick(&entries, "218281828").unwrap(),
            "Dolby-DCP2000-218281828.cert.sha256.pem"
        );
        assert!(dolby_pick(&entries, "999999999").is_none());
    }

    #[test]
    fn qube_pad_and_pick() {
        assert_eq!(qube_pad("54"), "00054");
        assert_eq!(qube_pad("00054"), "00054");
        let entries = vec![
            "QXPD-00054-10-14-sha256.pem.crt".to_string(),
            "QXPD-00097-08-14-sha256.pem.crt".to_string(),
        ];
        assert_eq!(
            qube_pick(&entries, "QXPD", "54").unwrap(),
            "QXPD-00054-10-14-sha256.pem.crt"
        );
        assert!(qube_pick(&entries, "QXPD", "12345").is_none());
    }

    #[test]
    fn credentialed_vendors_parse_and_flag() {
        assert_eq!(parse_vendor("christie").unwrap(), Vendor::Christie);
        assert_eq!(parse_vendor("GDC").unwrap(), Vendor::Gdc);
        assert_eq!(parse_vendor("gdc-tech").unwrap(), Vendor::Gdc);
        assert_eq!(parse_vendor("barco").unwrap(), Vendor::Barco);
        for v in [Vendor::Christie, Vendor::Gdc, Vendor::Barco] {
            assert!(needs_credentials(v));
        }
        for v in [Vendor::Dolby, Vendor::Qube] {
            assert!(!needs_credentials(v));
        }
    }

    #[test]
    fn christie_pads_serial_and_orders_fallback() {
        let paths = christie_paths("218281").unwrap();
        assert_eq!(
            paths,
            vec![
                "/Certificates/F-IMB/F-IMB_000000218281_sha256.pem".to_string(),
                "/Certificates/IMB-S2/IMB-S2_000000218281_sha256.pem".to_string(),
            ]
        );
        assert_eq!(
            christie_paths("123456789012").unwrap()[0],
            "/Certificates/F-IMB/F-IMB_123456789012_sha256.pem"
        );
        assert!(christie_paths("12A45").is_err());
        assert!(christie_paths("1234567890123").is_err());
    }

    #[test]
    fn gdc_path_uses_serial_as_is() {
        assert_eq!(gdc_path("H1234"), "/SHA256/H1234.crt.pem");
    }

    #[test]
    fn barco_requires_ten_chars_and_splits_dir() {
        assert_eq!(
            barco_path("1234567890").unwrap(),
            "/1234567xxx/1234567890/Barco-ICMP.1234567890_cert.pem"
        );
        assert!(barco_path("123").is_err());
        assert!(barco_path("12345678901").is_err());
    }

    #[test]
    fn credentials_debug_redacts_the_password() {
        let credentials = Credentials {
            user: "projectionist".into(),
            password: "hunter2".into(),
        };
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("projectionist"), "{debug}");
    }

    #[test]
    fn a_password_with_quotes_backslashes_and_newlines_reaches_curl_intact() {
        use base64::Engine;
        const EMPTY_HTTP_RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let credentials = Credentials {
            user: "projectionist".into(),
            password: "a\"b\\c\tv\u{0b}\r\nurl = \"http://elsewhere.test/\"".into(),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut authorization = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix(BASIC_AUTHORIZATION_PREFIX) {
                    authorization = Some(value.to_string());
                }
            }
            stream.write_all(EMPTY_HTTP_RESPONSE).unwrap();
            authorization
        });

        let mut curl = Command::new(SYSTEM_CURL)
            .args(["-q", "-s", "--noproxy", "*", "-K", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        curl.stdin
            .take()
            .unwrap()
            .write_all(curl_config(&url, &credentials).as_bytes())
            .unwrap();
        assert!(curl.wait().unwrap().success());

        let authorization = server.join().unwrap().expect("curl sent no basic auth");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(authorization)
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            format!("{}:{}", credentials.user, credentials.password)
        );
    }
}
