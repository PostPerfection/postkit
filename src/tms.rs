//! push a finished package to a theatre management system over ftp or sftp
//! (DCP-o-matic's tms_protocol / tms_ip / tms_path / tms_user / tms_password).
//! the config file holds the password: it is never logged, never echoed in
//! errors, never passed as a command-line argument, and Debug redacts it.
//!
//! behind the `tms` feature, off by default: it pulls in ssh2, which links
//! libssh2 and openssl, and a caller building postkit for wasm cannot have that.
//!
//! reading the config file is the app's job: where it lives is named after the
//! app, and a `toml` dependency here would put winnow's `AsRef` impls in front of
//! every crate that links postkit, which makes dcpdoctor's schema reader stop
//! compiling. so this side takes a deserialized `TmsConfig` and validates it.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

const FTP_PORT: u16 = 21;
const SSH_PORT: u16 = 22;
/// mode for a directory we create on the remote: owner writes, others read.
const REMOTE_DIR_MODE: i32 = 0o755;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TmsProtocol {
    /// plain FTP. the login crosses the network in the clear, so sftp is the
    /// better choice wherever the TMS offers it.
    Ftp,
    /// SFTP over SSH, with the host key checked against known_hosts.
    Sftp,
}

#[derive(Clone, Deserialize)]
pub struct TmsConfig {
    pub protocol: TmsProtocol,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    /// remote directory the package directory is created under.
    pub path: String,
    pub user: String,
    pub password: String,
}

// redact the password so it can never leak through Debug/log output.
impl fmt::Debug for TmsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmsConfig")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl TmsConfig {
    /// refuse a config that names no server or no login. deserializing cannot:
    /// an empty string is a valid String.
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("tms config needs a host".to_string());
        }
        if self.user.trim().is_empty() {
            return Err("tms config needs a user".to_string());
        }
        Ok(())
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(match self.protocol {
            TmsProtocol::Ftp => FTP_PORT,
            TmsProtocol::Sftp => SSH_PORT,
        })
    }
}

/// one remote filesystem, so the upload layout is tested against a fake instead
/// of a server. errors carry only the cause: `upload_with` names the file and
/// the remote path.
pub trait TmsTransport {
    /// create `remote_dir`, succeeding if it is already there.
    fn ensure_dir(&mut self, remote_dir: &str) -> Result<(), String>;
    fn put_file(&mut self, local: &Path, remote_path: &str) -> Result<(), String>;
}

/// connect, authenticate, and upload every file under `package_dir` into
/// `<config.path>/<package dir name>/`.
pub fn upload_package(config: &TmsConfig, package_dir: &Path) -> Result<(), String> {
    let mut transport = connect(config)?;
    upload_with(transport.as_mut(), &config.path, package_dir)
}

fn connect(config: &TmsConfig) -> Result<Box<dyn TmsTransport>, String> {
    match config.protocol {
        TmsProtocol::Ftp => Ok(Box::new(FtpTransport::connect(config)?)),
        TmsProtocol::Sftp => Ok(Box::new(SftpTransport::connect(config)?)),
    }
}

/// upload every file under `package_dir` into `<remote_base>/<package dir
/// name>/`, creating each remote directory before the files that go in it.
/// stops at the first failure, naming the file and the remote path it was going
/// to.
pub fn upload_with(
    transport: &mut dyn TmsTransport,
    remote_base: &str,
    package_dir: &Path,
) -> Result<(), String> {
    if !package_dir.is_dir() {
        return Err(format!(
            "package directory not found: {}",
            package_dir.display()
        ));
    }
    let package_name = package_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("cannot read a package name off {}", package_dir.display()))?;
    let files = collect_files(package_dir)?;
    if files.is_empty() {
        return Err(format!("nothing to upload under {}", package_dir.display()));
    }

    let remote_root = join_remote(remote_base, package_name);
    tracing::info!(
        "Uploading {} files from {} to {}",
        files.len(),
        package_dir.display(),
        remote_root
    );
    ensure_dir(transport, &remote_root)?;

    let total = files.len();
    let mut made: BTreeSet<String> = BTreeSet::new();
    for (index, local) in files.iter().enumerate() {
        let relative = local.strip_prefix(package_dir).unwrap_or(local);
        let relative_remote = relative_remote_path(relative)?;
        for dir in remote_ancestors(&remote_root, &relative_remote) {
            if made.insert(dir.clone()) {
                ensure_dir(transport, &dir)?;
            }
        }
        let remote_path = join_remote(&remote_root, &relative_remote);
        transport
            .put_file(local, &remote_path)
            .map_err(|e| format!("upload of {} to {remote_path} failed: {e}", local.display()))?;
        tracing::info!(
            "[{}/{}] Uploaded: {} -> {}",
            index + 1,
            total,
            relative_remote,
            remote_path
        );
    }
    tracing::info!("Uploaded {total} files to {remote_root}");
    Ok(())
}

fn ensure_dir(transport: &mut dyn TmsTransport, remote_dir: &str) -> Result<(), String> {
    transport
        .ensure_dir(remote_dir)
        .map_err(|e| format!("cannot create remote directory {remote_dir}: {e}"))
}

/// the remote directories a file needs, outermost first, relative to
/// `remote_root` (which the caller has already created).
fn remote_ancestors(remote_root: &str, relative_remote: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut current = remote_root.to_string();
    let parts: Vec<&str> = relative_remote.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current = join_remote(&current, part);
        dirs.push(current.clone());
    }
    dirs
}

/// a package-relative path as the remote spells it: forward slashes, whatever
/// this machine's separator is.
fn relative_remote_path(relative: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| {
                        format!(
                            "cannot upload {}: the name is not utf-8",
                            relative.display()
                        )
                    })?
                    .to_string(),
            ),
            _ => {
                return Err(format!(
                    "cannot upload {}: unexpected path component",
                    relative.display()
                ));
            }
        }
    }
    Ok(parts.join("/"))
}

fn join_remote(base: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        // an empty or "/" base means the login directory's root
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// every file under `dir`, sorted. a directory we cannot read is an error: a
/// package uploaded with files silently missing is a broken delivery.
fn collect_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("cannot read an entry of {}: {e}", dir.display()))?
            .path();
        if path.is_dir() {
            collect_files_recursive(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

// ── sftp (ssh2 / libssh2) ────────────────────────────────────────────────────

struct SftpTransport {
    sftp: ssh2::Sftp,
}

impl SftpTransport {
    fn connect(config: &TmsConfig) -> Result<Self, String> {
        let host = config.host.as_str();
        let port = config.port();
        let stream = std::net::TcpStream::connect((host, port))
            .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
        let mut session =
            ssh2::Session::new().map_err(|e| format!("cannot start an ssh session: {e}"))?;
        session.set_tcp_stream(stream);
        session
            .handshake()
            .map_err(|e| format!("ssh handshake with {host}:{port} failed: {e}"))?;
        check_host_key(&session, host, port)?;
        session
            .userauth_password(&config.user, &config.password)
            .map_err(|e| {
                format!(
                    "sftp login as {} on {host}:{port} failed: {}",
                    config.user,
                    e.message()
                )
            })?;
        if !session.authenticated() {
            return Err(format!("sftp login as {} was refused", config.user));
        }
        let sftp = session
            .sftp()
            .map_err(|e| format!("cannot open an sftp channel on {host}: {e}"))?;
        Ok(Self { sftp })
    }
}

/// refuse a host whose key is not the one known_hosts records, and refuse an
/// unknown host outright rather than handing it the password. libssh2 checks no
/// host key on its own, so without this the upload trusts whatever answers on
/// the address.
fn check_host_key(session: &ssh2::Session, host: &str, port: u16) -> Result<(), String> {
    let (key, _key_type) = session
        .host_key()
        .ok_or_else(|| format!("{host} offered no host key"))?;
    let fingerprint = host_key_fingerprint(session);
    let mut known_hosts = session
        .known_hosts()
        .map_err(|e| format!("cannot read known hosts: {e}"))?;
    let known_hosts_file = known_hosts_path()?;
    // a missing file is not an error here: it leaves the check with no entry for
    // the host, which is the NotFound refusal below.
    if known_hosts_file.exists() {
        known_hosts
            .read_file(&known_hosts_file, ssh2::KnownHostFileKind::OpenSSH)
            .map_err(|e| format!("cannot read {}: {e}", known_hosts_file.display()))?;
    }
    match known_hosts.check_port(host, port, key) {
        ssh2::CheckResult::Match => Ok(()),
        ssh2::CheckResult::Mismatch => Err(format!(
            "the host key of {host}:{port} ({fingerprint}) does not match the one in {}. \
             refusing to upload: either the server was rebuilt or something is impersonating it",
            known_hosts_file.display()
        )),
        ssh2::CheckResult::NotFound => Err(format!(
            "{host}:{port} is not in {} (its key is {fingerprint}). \
             add it with `ssh-keyscan -p {port} {host} >> {}` once you have checked that \
             fingerprint with the cinema",
            known_hosts_file.display(),
            known_hosts_file.display()
        )),
        ssh2::CheckResult::Failure => Err(format!("the host key check for {host}:{port} failed")),
    }
}

fn known_hosts_path() -> Result<PathBuf, String> {
    Ok(dirs::home_dir()
        .ok_or_else(|| "cannot find a home directory to read known_hosts from".to_string())?
        .join(".ssh")
        .join("known_hosts"))
}

/// the host key as OpenSSH prints it, so it can be compared with ssh-keyscan.
fn host_key_fingerprint(session: &ssh2::Session) -> String {
    use base64::Engine;
    match session.host_key_hash(ssh2::HashType::Sha256) {
        Some(hash) => format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash)
        ),
        None => "fingerprint unavailable".to_string(),
    }
}

impl TmsTransport for SftpTransport {
    fn ensure_dir(&mut self, remote_dir: &str) -> Result<(), String> {
        let path = Path::new(remote_dir);
        if self.sftp.stat(path).is_ok() {
            return Ok(());
        }
        self.sftp
            .mkdir(path, REMOTE_DIR_MODE)
            .map_err(|e| e.message().to_string())
    }

    fn put_file(&mut self, local: &Path, remote_path: &str) -> Result<(), String> {
        let mut source = std::fs::File::open(local).map_err(|e| e.to_string())?;
        let mut target = self
            .sftp
            .create(Path::new(remote_path))
            .map_err(|e| e.message().to_string())?;
        std::io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ── ftp (suppaftp) ───────────────────────────────────────────────────────────

struct FtpTransport {
    stream: suppaftp::FtpStream,
}

impl FtpTransport {
    fn connect(config: &TmsConfig) -> Result<Self, String> {
        let host = config.host.as_str();
        let port = config.port();
        tracing::warn!(
            "ftp sends the {host} login and the package unencrypted; sftp is the safer protocol \
             wherever the TMS offers it"
        );
        let mut stream = suppaftp::FtpStream::connect((host, port))
            .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
        stream
            .login(&config.user, &config.password)
            .map_err(|e| format!("ftp login as {} on {host}:{port} failed: {e}", config.user))?;
        stream
            .transfer_type(suppaftp::types::FileType::Binary)
            .map_err(|e| format!("cannot set binary transfers on {host}: {e}"))?;
        Ok(Self { stream })
    }
}

impl Drop for FtpTransport {
    fn drop(&mut self) {
        let _ = self.stream.quit();
    }
}

impl TmsTransport for FtpTransport {
    fn ensure_dir(&mut self, remote_dir: &str) -> Result<(), String> {
        let refusal = match self.stream.mkdir(remote_dir) {
            Ok(()) => return Ok(()),
            Err(e) => e.to_string(),
        };
        // a directory that is already there is refused with the same 550 a real
        // refusal gets, so prove it exists by stepping into it. the working
        // directory is put back either way: a relative base path is read
        // against it.
        let login_dir = self
            .stream
            .pwd()
            .map_err(|e| format!("{refusal} (and the working directory is unreadable: {e})"))?;
        let found = self.stream.cwd(remote_dir);
        self.stream
            .cwd(&login_dir)
            .map_err(|e| format!("cannot return to {login_dir}: {e}"))?;
        found.map_err(|_| refusal)
    }

    fn put_file(&mut self, local: &Path, remote_path: &str) -> Result<(), String> {
        let mut source = std::fs::File::open(local).map_err(|e| e.to_string())?;
        self.stream
            .put_file(remote_path, &mut source)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// records what an upload asked of the remote, in order, so a test can check
    /// the layout without a server.
    #[derive(Default)]
    struct FakeTransport {
        calls: Vec<String>,
        existing_dirs: BTreeSet<String>,
        fail_put: Option<String>,
    }

    impl TmsTransport for FakeTransport {
        fn ensure_dir(&mut self, remote_dir: &str) -> Result<(), String> {
            self.calls.push(format!("mkdir {remote_dir}"));
            self.existing_dirs.insert(remote_dir.to_string());
            Ok(())
        }

        fn put_file(&mut self, _local: &Path, remote_path: &str) -> Result<(), String> {
            if self.fail_put.as_deref() == Some(remote_path) {
                return Err("remote disk full".to_string());
            }
            let parent = remote_path
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or("");
            assert!(
                self.existing_dirs.contains(parent),
                "{remote_path} was written before {parent} was created"
            );
            self.calls.push(format!("put {remote_path}"));
            Ok(())
        }
    }

    /// a config as an app reads one: deserialize the TOML, then validate.
    fn config_from_toml(text: &str) -> Result<TmsConfig, String> {
        let config: TmsConfig =
            toml::from_str(text).map_err(|e| format!("invalid tms config: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    fn package_with(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("MyFilm_FTR_F_EN-XX_OV");
        for file in files {
            let path = package.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"essence").unwrap();
        }
        (dir, package)
    }

    #[test]
    fn config_parses_and_debug_redacts_password() {
        let config = config_from_toml(
            r#"
            protocol = "sftp"
            host = "tms.cinema.test"
            path = "/dcp"
            user = "projectionist"
            password = "hunter2"
            "#,
        )
        .unwrap();
        assert_eq!(config.protocol, TmsProtocol::Sftp);
        assert_eq!(config.port(), 22);
        assert_eq!(config.password, "hunter2");
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("hunter2"),
            "password must not appear in Debug: {debug}"
        );
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn ftp_defaults_to_port_21_and_an_explicit_port_wins() {
        let config = config_from_toml(
            r#"
            protocol = "ftp"
            host = "10.0.0.9"
            path = "/incoming"
            user = "tms"
            password = "p"
            "#,
        )
        .unwrap();
        assert_eq!(config.protocol, TmsProtocol::Ftp);
        assert_eq!(config.port(), 21);

        let config = config_from_toml(
            r#"
            protocol = "sftp"
            host = "10.0.0.9"
            port = 2222
            path = "/incoming"
            user = "tms"
            password = "p"
            "#,
        )
        .unwrap();
        assert_eq!(config.port(), 2222);
    }

    #[test]
    fn a_config_missing_a_field_is_refused() {
        let err = config_from_toml(
            r#"
            protocol = "sftp"
            host = "tms.cinema.test"
            path = "/dcp"
            "#,
        )
        .unwrap_err();
        assert!(err.contains("invalid tms config"), "{err}");
        let err = config_from_toml(
            r#"
            protocol = "carrier-pigeon"
            host = "tms.cinema.test"
            path = "/dcp"
            user = "u"
            password = "p"
            "#,
        )
        .unwrap_err();
        assert!(err.contains("invalid tms config"), "{err}");
    }

    #[test]
    fn upload_creates_the_package_directory_then_puts_every_file() {
        let (_guard, package) = package_with(&["ASSETMAP.xml", "CPL_x.xml", "picture.mxf"]);
        let mut transport = FakeTransport::default();
        upload_with(&mut transport, "/srv/dcp", &package).unwrap();
        assert_eq!(
            transport.calls,
            vec![
                "mkdir /srv/dcp/MyFilm_FTR_F_EN-XX_OV",
                "put /srv/dcp/MyFilm_FTR_F_EN-XX_OV/ASSETMAP.xml",
                "put /srv/dcp/MyFilm_FTR_F_EN-XX_OV/CPL_x.xml",
                "put /srv/dcp/MyFilm_FTR_F_EN-XX_OV/picture.mxf",
            ]
        );
    }

    #[test]
    fn a_subdirectory_is_created_before_the_files_in_it() {
        let (_guard, package) = package_with(&["ASSETMAP.xml", "sub/deep/picture.mxf"]);
        let mut transport = FakeTransport::default();
        upload_with(&mut transport, "/srv/dcp/", &package).unwrap();
        assert_eq!(
            transport.calls,
            vec![
                "mkdir /srv/dcp/MyFilm_FTR_F_EN-XX_OV",
                "put /srv/dcp/MyFilm_FTR_F_EN-XX_OV/ASSETMAP.xml",
                "mkdir /srv/dcp/MyFilm_FTR_F_EN-XX_OV/sub",
                "mkdir /srv/dcp/MyFilm_FTR_F_EN-XX_OV/sub/deep",
                "put /srv/dcp/MyFilm_FTR_F_EN-XX_OV/sub/deep/picture.mxf",
            ]
        );
    }

    #[test]
    fn an_empty_base_path_uploads_under_the_login_directory() {
        let (_guard, package) = package_with(&["ASSETMAP.xml"]);
        let mut transport = FakeTransport::default();
        upload_with(&mut transport, "", &package).unwrap();
        assert_eq!(
            transport.calls,
            vec![
                "mkdir /MyFilm_FTR_F_EN-XX_OV",
                "put /MyFilm_FTR_F_EN-XX_OV/ASSETMAP.xml",
            ]
        );
    }

    #[test]
    fn a_failed_file_names_the_file_and_the_remote_path() {
        let (_guard, package) = package_with(&["ASSETMAP.xml", "picture.mxf"]);
        let mut transport = FakeTransport {
            fail_put: Some("/srv/dcp/MyFilm_FTR_F_EN-XX_OV/picture.mxf".to_string()),
            ..Default::default()
        };
        let err = upload_with(&mut transport, "/srv/dcp", &package).unwrap_err();
        assert!(err.contains("picture.mxf"), "{err}");
        assert!(
            err.contains("/srv/dcp/MyFilm_FTR_F_EN-XX_OV/picture.mxf"),
            "{err}"
        );
        assert!(err.contains("remote disk full"), "{err}");
        // it stopped at the failure rather than carrying on
        assert_eq!(transport.calls.len(), 2);
    }

    #[test]
    fn a_missing_or_empty_package_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut transport = FakeTransport::default();
        let err = upload_with(&mut transport, "/srv/dcp", &dir.path().join("gone")).unwrap_err();
        assert!(err.contains("package directory not found"), "{err}");

        let empty = dir.path().join("Empty_OV");
        std::fs::create_dir_all(&empty).unwrap();
        let err = upload_with(&mut transport, "/srv/dcp", &empty).unwrap_err();
        assert!(err.contains("nothing to upload"), "{err}");
    }
}
