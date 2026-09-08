//! push a finished package to a theatre management system over ftp or sftp
//! (DCP-o-matic's tms_protocol / tms_ip / tms_path / tms_user / tms_password).
//! the config file holds the password and the key passphrase: neither is ever
//! logged, echoed in an error, or passed as a command-line argument, and Debug
//! redacts both.
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
const REDACTED: &str = "<redacted>";
const FTP_TAKES_NO_KEY: &str =
    "ftp cannot log in with a key: give the tms config a password, or use sftp";

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
    #[serde(default)]
    pub password: Option<String>,
    /// sftp private key to log in with. when it is set the password is not sent.
    #[serde(default)]
    pub private_key: Option<PathBuf>,
    /// passphrase of `private_key`, for a key that is stored encrypted.
    #[serde(default)]
    pub private_key_passphrase: Option<String>,
}

// redact the password and the passphrase so neither can leak through Debug/log output.
impl fmt::Debug for TmsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmsConfig")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| REDACTED))
            .field("private_key", &self.private_key)
            .field(
                "private_key_passphrase",
                &self.private_key_passphrase.as_ref().map(|_| REDACTED),
            )
            .finish()
    }
}

/// the credential the login sends.
enum TmsLogin<'a> {
    Password(&'a str),
    PrivateKey {
        path: &'a Path,
        passphrase: Option<&'a str>,
    },
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
        if self.protocol == TmsProtocol::Ftp && self.private_key.is_some() {
            return Err(FTP_TAKES_NO_KEY.to_string());
        }
        self.login()?;
        Ok(())
    }

    /// the key when the config names one, the password otherwise.
    fn login(&self) -> Result<TmsLogin<'_>, String> {
        match (&self.private_key, &self.password) {
            (Some(path), _) => Ok(TmsLogin::PrivateKey {
                path,
                passphrase: self.private_key_passphrase.as_deref(),
            }),
            (None, Some(password)) => Ok(TmsLogin::Password(password)),
            (None, None) => Err("tms config needs a password or a private_key".to_string()),
        }
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
        Self::connect_with_known_hosts(config, &known_hosts_path()?)
    }

    // private so nothing outside this file can aim the host key check at another file
    fn connect_with_known_hosts(
        config: &TmsConfig,
        known_hosts_file: &Path,
    ) -> Result<Self, String> {
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
        check_host_key(&session, host, port, known_hosts_file)?;
        match config.login()? {
            TmsLogin::Password(password) => {
                session
                    .userauth_password(&config.user, password)
                    .map_err(|e| {
                        format!(
                            "sftp login as {} on {host}:{port} failed: {}",
                            config.user,
                            e.message()
                        )
                    })?;
            }
            TmsLogin::PrivateKey { path, passphrase } => {
                // libssh2 reads the public half out of the private key file
                session
                    .userauth_pubkey_file(&config.user, None, path, passphrase)
                    .map_err(|e| {
                        format!(
                            "sftp login as {} on {host}:{port} with the key {} failed: {}",
                            config.user,
                            path.display(),
                            e.message()
                        )
                    })?;
            }
        }
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
fn check_host_key(
    session: &ssh2::Session,
    host: &str,
    port: u16,
    known_hosts_file: &Path,
) -> Result<(), String> {
    let (key, _key_type) = session
        .host_key()
        .ok_or_else(|| format!("{host} offered no host key"))?;
    let fingerprint = host_key_fingerprint(session);
    refuse_revoked_key(known_hosts_file, host, port, key, &fingerprint)?;
    let mut known_hosts = session
        .known_hosts()
        .map_err(|e| format!("cannot read known hosts: {e}"))?;
    // a missing file is not an error here: it leaves the check with no entry for
    // the host, which is the NotFound refusal below.
    if known_hosts_file.exists() {
        known_hosts
            .read_file(known_hosts_file, ssh2::KnownHostFileKind::OpenSSH)
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

const REVOKED_MARKER: &str = "@revoked";
// the line is `@revoked <hosts> <keytype> <base64>`
const REVOKED_LINE_FIELDS: usize = 4;
const REVOKED_KEY_FIELD: usize = 3;

// libssh2 reports a revoked key as a plain Match, it hands back no OpenSSH marker
fn refuse_revoked_key(
    known_hosts_file: &Path,
    host: &str,
    port: u16,
    key: &[u8],
    fingerprint: &str,
) -> Result<(), String> {
    use base64::Engine;
    if !known_hosts_file.exists() {
        return Ok(());
    }
    let text = std::fs::read(known_hosts_file)
        .map_err(|e| format!("cannot read {}: {e}", known_hosts_file.display()))?;
    // a hand edited line may drop the padding, so neither side keeps it
    let offered = base64::engine::general_purpose::STANDARD.encode(key);
    let offered = offered.trim_end_matches('=');
    for (index, line) in String::from_utf8_lossy(&text).lines().enumerate() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() != Some(&REVOKED_MARKER) {
            continue;
        }
        if fields.len() < REVOKED_LINE_FIELDS {
            return Err(format!(
                "line {} of {} starts with {REVOKED_MARKER} but carries no host key. \
                 refusing to upload: a known_hosts that cannot be read cannot be trusted",
                index + 1,
                known_hosts_file.display()
            ));
        }
        if fields[REVOKED_KEY_FIELD].trim_end_matches('=') == offered {
            return Err(format!(
                "the host key of {host}:{port} ({fingerprint}) is marked {REVOKED_MARKER} in {}. \
                 refusing to upload: that key is compromised, ask the cinema for the new one",
                known_hosts_file.display()
            ));
        }
    }
    Ok(())
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
        let password = match config.login()? {
            TmsLogin::Password(password) => password,
            TmsLogin::PrivateKey { .. } => return Err(FTP_TAKES_NO_KEY.to_string()),
        };
        tracing::warn!(
            "ftp sends the {host} login and the package unencrypted; sftp is the safer protocol \
             wherever the TMS offers it"
        );
        let mut stream = suppaftp::FtpStream::connect((host, port))
            .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
        stream
            .login(config.user.as_str(), password)
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
        assert_eq!(config.password.as_deref(), Some("hunter2"));
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("hunter2"),
            "password must not appear in Debug: {debug}"
        );
        assert!(debug.contains(REDACTED));
    }

    #[test]
    fn a_key_config_parses_and_debug_redacts_the_passphrase() {
        let config = config_from_toml(
            r#"
            protocol = "sftp"
            host = "tms.cinema.test"
            path = "/dcp"
            user = "projectionist"
            private_key = "/home/projectionist/.ssh/tms_ed25519"
            private_key_passphrase = "opensesame"
            "#,
        )
        .unwrap();
        assert_eq!(config.password, None);
        assert_eq!(
            config.private_key.as_deref(),
            Some(Path::new("/home/projectionist/.ssh/tms_ed25519"))
        );
        assert!(matches!(
            config.login().unwrap(),
            TmsLogin::PrivateKey {
                passphrase: Some("opensesame"),
                ..
            }
        ));
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("opensesame"),
            "the passphrase must not appear in Debug: {debug}"
        );
        assert!(debug.contains("tms_ed25519"), "{debug}");
    }

    #[test]
    fn a_key_wins_over_a_password_and_ftp_refuses_a_key() {
        let both = r#"
            protocol = "sftp"
            host = "tms.cinema.test"
            path = "/dcp"
            user = "projectionist"
            password = "hunter2"
            private_key = "/home/projectionist/.ssh/tms_ed25519"
            "#;
        let config = config_from_toml(both).unwrap();
        assert!(matches!(
            config.login().unwrap(),
            TmsLogin::PrivateKey { .. }
        ));

        let err = config_from_toml(&both.replace(r#""sftp""#, r#""ftp""#)).unwrap_err();
        assert!(err.contains("ftp cannot log in with a key"), "{err}");
    }

    #[test]
    fn a_config_with_no_password_and_no_key_is_refused() {
        let err = config_from_toml(
            r#"
            protocol = "sftp"
            host = "tms.cinema.test"
            path = "/dcp"
            user = "projectionist"
            "#,
        )
        .unwrap_err();
        assert!(err.contains("password or a private_key"), "{err}");
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

// unix only: an unprivileged sshd on a throwaway config is not a thing on windows
#[cfg(all(test, unix))]
mod local_sshd_tests {
    use super::{
        REVOKED_MARKER, SftpTransport, TmsConfig, TmsProtocol, collect_files, upload_with,
    };
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const LOOPBACK: &str = "127.0.0.1";
    // a name no account has, so an sshd log line naming it came from this test
    const PROBE_USER: &str = "tms_upload_probe";
    const PROBE_PASSWORD: &str = "this-password-must-never-leave-the-client";
    // sshd logs this only once a password arrived, not for the method probe that carries the user name
    const PASSWORD_ATTEMPT_LOG: &str = "Failed password";
    // covers the Postponed, Accepted and Failed lines, so any key offered at all matches
    const PUBKEY_ATTEMPT_LOG: &str = "publickey for";
    const PUBKEY_ACCEPTED_LOG: &str = "Accepted publickey for";
    const LISTENING_LOG: &str = "Server listening on";
    const CONNECTION_OVER_LOG: &str = "Connection closed";
    const SERVER_START_TIMEOUT: Duration = Duration::from_secs(20);
    const LOG_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(25);
    const HOST_KEY: &str = "hostkey";
    const OTHER_HOST_KEY: &str = "otherkey";
    const CLIENT_KEY: &str = "clientkey";
    const WRONG_CLIENT_KEY: &str = "wrongkey";
    const BASE_DIRECTORY: &str = "incoming";
    const PACKAGE_NAME: &str = "MyFilm_FTR_F_EN-XX_OV";
    // the mxf bytes carry a nul and a byte over 0x7f, so a text-mode transfer would show up
    const PACKAGE_FILES: [(&str, &[u8]); 4] = [
        ("ASSETMAP.xml", b"<AssetMap/>"),
        ("CPL_x.xml", b"<CompositionPlaylist/>"),
        ("VOLINDEX.xml", b"<VolumeIndex/>"),
        (
            "sub/deep/picture.mxf",
            &[0x06, 0x0e, 0x2b, 0x34, 0x00, 0xff, 0x80, 0x0a, 0x0d, 0x1a],
        ),
    ];
    // shorter than the ASSETMAP.xml above, so a second upload that did not truncate would leave a tail
    const REPLACED_ASSETMAP: &[u8] = b"<A/>";
    const READ_ONLY_DIRECTORY_MODE: u32 = 0o555;
    const WRITABLE_DIRECTORY_MODE: u32 = 0o755;

    fn sshd_binary() -> PathBuf {
        let candidates = [
            "/usr/sbin/sshd",
            "/usr/bin/sshd",
            "/usr/local/sbin/sshd",
            "/usr/local/bin/sshd",
            "/opt/homebrew/sbin/sshd",
        ];
        for candidate in candidates {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return path;
            }
        }
        panic!(
            "no sshd found in {candidates:?}. this test needs the openssh server package, \
             it is not skipped when the binary is missing"
        );
    }

    fn free_loopback_port() -> u16 {
        let listener = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    /// the account this sshd can log in: it runs unprivileged, so the only user it
    /// can authenticate is the one that started it.
    fn current_user() -> String {
        let output = Command::new("id")
            .arg("-un")
            .output()
            .expect("`id -un` must work to name the user the local sshd can log in");
        assert!(output.status.success(), "`id -un` failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn generate_key(directory: &Path, name: &str) -> String {
        let key_path = directory.join(name);
        let status = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen must be installed to run the host key tests");
        assert!(status.success(), "ssh-keygen failed for {name}");
        let published = std::fs::read_to_string(key_path.with_extension("pub")).unwrap();
        // the pub file is "ssh-ed25519 <base64> <comment>", known_hosts wants the first two fields
        let mut fields = published.split_whitespace();
        let algorithm = fields.next().unwrap();
        let material = fields.next().unwrap();
        format!("{algorithm} {material}")
    }

    #[derive(Clone, Copy, PartialEq)]
    enum ServerLogin {
        /// a probe user with no account, so the login is always refused
        Password,
        /// the user running the tests, by key, so the login can succeed and put files
        PublicKey,
    }

    struct LocalSshServer {
        process: Child,
        directory: tempfile::TempDir,
        port: u16,
        host_key: String,
        other_host_key: String,
        login: ServerLogin,
    }

    impl LocalSshServer {
        fn start() -> Self {
            Self::start_with(ServerLogin::Password)
        }

        fn start_with_key_login() -> Self {
            Self::start_with(ServerLogin::PublicKey)
        }

        fn start_with(login: ServerLogin) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let host_key = generate_key(directory.path(), HOST_KEY);
            let other_host_key = generate_key(directory.path(), OTHER_HOST_KEY);
            let port = free_loopback_port();
            let authentication = match login {
                ServerLogin::Password => "PasswordAuthentication yes\n\
                     PubkeyAuthentication no\n\
                     AuthorizedKeysFile none\n"
                    .to_string(),
                ServerLogin::PublicKey => {
                    generate_key(directory.path(), CLIENT_KEY);
                    generate_key(directory.path(), WRONG_CLIENT_KEY);
                    let authorized_keys = directory.path().join("authorized_keys");
                    std::fs::copy(
                        directory.path().join(format!("{CLIENT_KEY}.pub")),
                        &authorized_keys,
                    )
                    .unwrap();
                    std::fs::create_dir_all(directory.path().join(BASE_DIRECTORY)).unwrap();
                    format!(
                        "PasswordAuthentication no\n\
                         PubkeyAuthentication yes\n\
                         UsePAM no\n\
                         AuthorizedKeysFile {}\n\
                         Subsystem sftp internal-sftp\n",
                        authorized_keys.display()
                    )
                }
            };
            let config_path = directory.path().join("sshd_config");
            std::fs::write(
                &config_path,
                format!(
                    "ListenAddress {LOOPBACK}\n\
                     Port {port}\n\
                     HostKey {}\n\
                     PidFile none\n\
                     StrictModes no\n\
                     KbdInteractiveAuthentication no\n\
                     PermitRootLogin no\n\
                     LogLevel VERBOSE\n\
                     {authentication}",
                    directory.path().join(HOST_KEY).display()
                ),
            )
            .unwrap();
            let process = Command::new(sshd_binary())
                .arg("-D")
                .arg("-f")
                .arg(&config_path)
                .arg("-E")
                .arg(directory.path().join("log"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("cannot start sshd");
            let server = Self {
                process,
                directory,
                port,
                host_key,
                other_host_key,
                login,
            };
            server.wait_until_listening();
            server
        }

        // readiness comes off the log, a tcp probe would leave a connection in it that
        // wait_for_connection_to_close would then mistake for the test's own
        fn wait_until_listening(&self) {
            let deadline = Instant::now() + SERVER_START_TIMEOUT;
            while Instant::now() < deadline {
                if self.log().contains(LISTENING_LOG) {
                    return;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            panic!("sshd never listened on {}. log: {}", self.port, self.log());
        }

        fn log(&self) -> String {
            let mut text = String::new();
            if let Ok(mut file) = std::fs::File::open(self.directory.path().join("log")) {
                let _ = file.read_to_string(&mut text);
            }
            text
        }

        // waiting for the close line keeps "no password arrived" from just reading the log too early
        fn wait_for_connection_to_close(&self) {
            let deadline = Instant::now() + LOG_SETTLE_TIMEOUT;
            while Instant::now() < deadline {
                if self.log().contains(CONNECTION_OVER_LOG) {
                    return;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            panic!(
                "sshd never logged the end of the connection, so the log cannot be trusted. log: {}",
                self.log()
            );
        }

        // the other entries are the markers and key types a real known_hosts holds, so a
        // refusal is never an empty-file accident
        fn known_hosts_with(&self, line: &str) -> PathBuf {
            let path = self.directory.path().join("known_hosts");
            std::fs::write(
                &path,
                format!(
                    "@cert-authority *.cinema.test {other}\n\
                     @revoked tms.decommissioned.test {other}\n\
                     tms.other-cinema.test {other}\n\
                     sk.cinema.test sk-ssh-ed25519@openssh.com \
                     AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIHVDLwHNGVBmpXaMRRTvJXBiFPMuHkNxOm6\
                     iRw+bHGZTAAAABHNzaDo=\n\
                     |1|F1E1w0Ic4qCPGZC8yZBHiSZ0Vd0=|hK1Zx6dq8OqvKgTNy2FQ8HqfeBQ= ssh-ed25519 \
                     AAAAC3NzaC1lZDI1NTE5AAAAIA1lNwjrY0xVeF7mQxvWpx7oOtXK5gYQfBBQ0nqPtvhZ\n\
                     {line}\n",
                    other = self.other_host_key
                ),
            )
            .unwrap();
            path
        }

        /// the known_hosts line that records this server's own key.
        fn own_entry(&self) -> String {
            format!("[{LOOPBACK}]:{} {}", self.port, self.host_key)
        }

        /// the known_hosts line that records the other key, the one this server does
        /// not answer with.
        fn impostor_entry(&self) -> String {
            format!("[{LOOPBACK}]:{} {}", self.port, self.other_host_key)
        }

        /// the remote directory an upload creates the package directory under.
        fn base_path(&self) -> PathBuf {
            self.directory.path().join(BASE_DIRECTORY)
        }

        fn config(&self) -> TmsConfig {
            let (user, password, private_key, path) = match self.login {
                ServerLogin::Password => (
                    PROBE_USER.to_string(),
                    Some(PROBE_PASSWORD.to_string()),
                    None,
                    "/incoming".to_string(),
                ),
                ServerLogin::PublicKey => (
                    current_user(),
                    None,
                    Some(self.directory.path().join(CLIENT_KEY)),
                    self.base_path().display().to_string(),
                ),
            };
            TmsConfig {
                protocol: TmsProtocol::Sftp,
                host: LOOPBACK.to_string(),
                port: Some(self.port),
                path,
                user,
                password,
                private_key,
                private_key_passphrase: None,
            }
        }
    }

    /// what `upload_package` runs, with the known_hosts file pointed at the test's
    /// own rather than the one in the home directory.
    fn upload(config: &TmsConfig, known_hosts: &Path, package: &Path) -> Result<(), String> {
        let mut transport = SftpTransport::connect_with_known_hosts(config, known_hosts)?;
        upload_with(&mut transport, &config.path, package)
    }

    fn local_package(directory: &Path) -> PathBuf {
        let package = directory.join(PACKAGE_NAME);
        for (name, bytes) in PACKAGE_FILES {
            let path = package.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
        }
        package
    }

    /// every file under `directory`, as slash-separated paths relative to it.
    fn relative_paths(directory: &Path) -> Vec<String> {
        collect_files(directory)
            .unwrap()
            .iter()
            .map(|path| {
                path.strip_prefix(directory)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn set_mode(directory: &Path, mode: u32) {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    impl Drop for LocalSshServer {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    #[test]
    fn an_unknown_host_is_refused_before_the_password_is_sent() {
        let server = LocalSshServer::start();
        let known_hosts = server.known_hosts_with("# no entry for the server under test");
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("an unknown host must not be uploaded to");
        assert!(error.contains("is not in"), "{error}");
        assert!(error.contains("ssh-keyscan"), "{error}");
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            !log.contains(PASSWORD_ATTEMPT_LOG),
            "the password was sent to a host that is not in known_hosts. log: {log}"
        );
    }

    #[test]
    fn a_mismatched_host_key_is_refused_before_the_password_is_sent() {
        let server = LocalSshServer::start();
        let impostor = server.impostor_entry();
        let known_hosts = server.known_hosts_with(&impostor);
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("a host whose key changed must not be uploaded to");
        assert!(error.contains("does not match"), "{error}");
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            !log.contains(PASSWORD_ATTEMPT_LOG),
            "the password was sent to a host whose key does not match known_hosts. log: {log}"
        );
    }

    #[test]
    fn a_revoked_host_key_is_refused_even_when_a_plain_line_matches() {
        let server = LocalSshServer::start();
        let entry = server.own_entry();
        let revoked = format!("{REVOKED_MARKER} {}", server.own_entry());
        let known_hosts = server.known_hosts_with(&format!("{entry}\n{revoked}"));
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("a revoked host key must not be uploaded to");
        assert!(error.contains(REVOKED_MARKER), "{error}");
        assert!(
            error.contains(&known_hosts.display().to_string()),
            "{error}"
        );
        assert!(error.contains("SHA256:"), "{error}");
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            !log.contains(PASSWORD_ATTEMPT_LOG),
            "the password was sent to a host whose key is revoked. log: {log}"
        );
    }

    #[test]
    fn a_revoked_line_for_another_key_leaves_the_real_one_accepted() {
        let server = LocalSshServer::start();
        let entry = server.own_entry();
        let revoked = format!("{REVOKED_MARKER} {}", server.impostor_entry());
        let known_hosts = server.known_hosts_with(&format!("{revoked}\n{entry}"));
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("no account exists for the probe user");
        assert!(
            error.contains("login as") && !error.contains(REVOKED_MARKER),
            "a key that is not the revoked one must still pass the check: {error}"
        );
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            log.contains(PASSWORD_ATTEMPT_LOG) && log.contains(PROBE_USER),
            "sshd never saw a password attempt, so the check did not pass a known host. log: {log}"
        );
    }

    #[test]
    fn a_key_that_is_only_revoked_is_refused_as_revoked_not_as_unknown() {
        let server = LocalSshServer::start();
        let revoked = format!(
            "{REVOKED_MARKER} * {}",
            server.host_key.trim_end_matches('=')
        );
        let known_hosts = server.known_hosts_with(&revoked);
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("a revoked host key must not be uploaded to");
        assert!(error.contains(REVOKED_MARKER), "{error}");
        assert!(
            !error.contains("ssh-keyscan"),
            "a revoked key must not be reported as a host nobody has seen yet: {error}"
        );
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            !log.contains(PASSWORD_ATTEMPT_LOG),
            "the password was sent to a host whose key is revoked. log: {log}"
        );
    }

    #[test]
    fn a_malformed_revoked_line_is_refused_with_the_file_and_the_line_number() {
        let server = LocalSshServer::start();
        let malformed = format!("{REVOKED_MARKER} tms.cinema.test");
        let known_hosts = server.known_hosts_with(&malformed);
        let text = std::fs::read_to_string(&known_hosts).unwrap();
        let line_number = text.lines().position(|line| line == malformed).unwrap() + 1;
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("a known_hosts that cannot be parsed must not be uploaded against");
        assert!(error.contains(&format!("line {line_number}")), "{error}");
        assert!(
            error.contains(&known_hosts.display().to_string()),
            "{error}"
        );
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            !log.contains(PASSWORD_ATTEMPT_LOG),
            "the password was sent while known_hosts could not be parsed. log: {log}"
        );
    }

    #[test]
    fn a_matching_host_key_is_accepted_and_the_login_follows_it() {
        let server = LocalSshServer::start();
        let entry = server.own_entry();
        let known_hosts = server.known_hosts_with(&entry);
        // the login fails because PROBE_USER is nobody, what matters is that it was reached
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("no account exists for the probe user");
        assert!(
            error.contains("login as") && !error.contains("known_hosts"),
            "a known host key must pass the check and let the login run: {error}"
        );
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            log.contains(PASSWORD_ATTEMPT_LOG) && log.contains(PROBE_USER),
            "sshd never saw a password attempt, so the check did not pass a known host. log: {log}"
        );
    }

    #[test]
    fn a_key_login_uploads_every_file_with_the_same_bytes_and_layout() {
        let server = LocalSshServer::start_with_key_login();
        let known_hosts = server.known_hosts_with(&server.own_entry());
        let local = tempfile::tempdir().unwrap();
        let package = local_package(local.path());

        upload(&server.config(), &known_hosts, &package).unwrap();

        let remote_root = server.base_path().join(PACKAGE_NAME);
        let mut expected: Vec<String> = PACKAGE_FILES
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        expected.sort();
        assert_eq!(relative_paths(&remote_root), expected);
        assert_eq!(relative_paths(&package), expected);
        for (name, bytes) in PACKAGE_FILES {
            assert_eq!(
                std::fs::read(remote_root.join(name)).unwrap(),
                bytes,
                "{name} did not arrive byte for byte"
            );
        }
        let log = server.log();
        assert!(
            log.contains(PUBKEY_ACCEPTED_LOG),
            "the upload did not authenticate with the key. log: {log}"
        );
    }

    #[test]
    fn a_second_upload_overwrites_the_files_it_finds_and_leaves_the_rest() {
        let server = LocalSshServer::start_with_key_login();
        let known_hosts = server.known_hosts_with(&server.own_entry());
        let local = tempfile::tempdir().unwrap();
        let package = local_package(local.path());
        let config = server.config();
        upload(&config, &known_hosts, &package).unwrap();

        std::fs::write(package.join("ASSETMAP.xml"), REPLACED_ASSETMAP).unwrap();
        std::fs::remove_file(package.join("CPL_x.xml")).unwrap();
        upload(&config, &known_hosts, &package).unwrap();

        let remote_root = server.base_path().join(PACKAGE_NAME);
        assert_eq!(
            std::fs::read(remote_root.join("ASSETMAP.xml")).unwrap(),
            REPLACED_ASSETMAP,
            "a second upload must truncate the file it replaces"
        );
        assert!(
            remote_root.join("CPL_x.xml").is_file(),
            "the upload adds and replaces files, it does not mirror the package directory"
        );
    }

    #[test]
    fn a_mismatched_host_key_is_refused_before_the_key_is_offered() {
        let server = LocalSshServer::start_with_key_login();
        let known_hosts = server.known_hosts_with(&server.impostor_entry());
        let error = SftpTransport::connect_with_known_hosts(&server.config(), &known_hosts)
            .err()
            .expect("a host whose key changed must not be logged in to");
        assert!(error.contains("does not match"), "{error}");
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            !log.contains(PUBKEY_ATTEMPT_LOG),
            "the key was offered to a host whose key does not match known_hosts. log: {log}"
        );
    }

    #[test]
    fn a_key_the_server_does_not_know_is_refused_as_a_login_failure() {
        let server = LocalSshServer::start_with_key_login();
        let known_hosts = server.known_hosts_with(&server.own_entry());
        let mut config = server.config();
        config.private_key = Some(server.directory.path().join(WRONG_CLIENT_KEY));
        let error = SftpTransport::connect_with_known_hosts(&config, &known_hosts)
            .err()
            .expect("a key the server has no authorized_keys line for must not log in");
        assert!(
            error.contains(&format!("login as {}", config.user)),
            "{error}"
        );
        assert!(
            error.contains(&format!("{LOOPBACK}:{}", server.port)),
            "{error}"
        );
        assert!(error.contains(WRONG_CLIENT_KEY), "{error}");
        assert!(
            !error.contains("known_hosts") && !error.contains("does not match"),
            "a refused key must not read as a host key problem: {error}"
        );
        server.wait_for_connection_to_close();
        let log = server.log();
        assert!(
            log.contains(PUBKEY_ATTEMPT_LOG) && !log.contains(PUBKEY_ACCEPTED_LOG),
            "sshd must have seen the key and refused it. log: {log}"
        );
    }

    #[test]
    fn a_file_the_server_refuses_names_the_file_and_the_remote_path() {
        let server = LocalSshServer::start_with_key_login();
        let known_hosts = server.known_hosts_with(&server.own_entry());
        let local = tempfile::tempdir().unwrap();
        let package = local_package(local.path());
        let remote_root = server.base_path().join(PACKAGE_NAME);
        std::fs::create_dir_all(&remote_root).unwrap();
        set_mode(&remote_root, READ_ONLY_DIRECTORY_MODE);

        let error = upload(&server.config(), &known_hosts, &package)
            .expect_err("a package directory the server cannot write must not report success");

        set_mode(&remote_root, WRITABLE_DIRECTORY_MODE);
        let refused = remote_root.join("ASSETMAP.xml");
        assert!(error.contains("ASSETMAP.xml"), "{error}");
        assert!(error.contains(&refused.display().to_string()), "{error}");
        assert!(
            !refused.exists(),
            "the file the error names must not be on the server"
        );
    }
}
