use super::bundle::{CinemaBundle, zip_files};
use lettre::message::{Attachment, MultiPart, SinglePart, header};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use serde::{Deserialize, Serialize};
use std::fmt;

const ZIP_MIME_TYPE: &str = "application/zip";
const LEGACY_ATTACHMENT_NAME: &str = "kdms.zip";
const ZIP_EXTENSION: &str = ".zip";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    // implicit TLS, usually port 465
    #[default]
    Tls,
    // STARTTLS upgrade, usually port 587
    Starttls,
    // test servers only
    None,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    pub from: String,
    #[serde(default)]
    pub subject_template: Option<String>,
    #[serde(default)]
    pub body_template: Option<String>,
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("from", &self.from)
            .finish()
    }
}

pub fn substitute(template: &str, title: &str, cinema: &str) -> String {
    template
        .replace("{title}", title)
        .replace("{cinema}", cinema)
}

pub fn build_message(
    from: &str,
    to: &[String],
    subject: &str,
    body: &str,
    attachment_name: &str,
    attachment_bytes: Vec<u8>,
    attachment_mime: &str,
) -> Result<Message, String> {
    if to.is_empty() {
        return Err("no recipient email addresses".to_string());
    }
    let mut builder = Message::builder()
        .from(
            from.parse()
                .map_err(|e| format!("invalid from address '{from}': {e}"))?,
        )
        .subject(subject);
    for addr in to {
        builder = builder.to(addr
            .parse()
            .map_err(|e| format!("invalid recipient '{addr}': {e}"))?);
    }
    let ctype = header::ContentType::parse(attachment_mime)
        .map_err(|e| format!("bad attachment mime '{attachment_mime}': {e}"))?;
    let part = MultiPart::mixed()
        .singlepart(SinglePart::plain(body.to_string()))
        .singlepart(Attachment::new(attachment_name.to_string()).body(attachment_bytes, ctype));
    builder
        .multipart(part)
        .map_err(|e| format!("build message: {e}"))
}

// the dcpwizard shape: KDM files zipped into kdms.zip, subject and body from the templates
pub fn build_kdm_email(
    config: &SmtpConfig,
    cinema: &str,
    title: &str,
    to: &[String],
    kdm_files: &[std::path::PathBuf],
) -> Result<Message, String> {
    let mut entries = Vec::new();
    for f in kdm_files {
        let bytes = std::fs::read(f).map_err(|e| format!("cannot read {}: {e}", f.display()))?;
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("kdm.xml")
            .to_string();
        entries.push((name, bytes));
    }
    let zip = zip_files(&entries)?;
    let subject = config
        .subject_template
        .as_deref()
        .map(|t| substitute(t, title, cinema))
        .unwrap_or_else(|| format!("KDM(s) for {title}"));
    let body = config
        .body_template
        .as_deref()
        .map(|t| substitute(t, title, cinema))
        .unwrap_or_else(|| format!("Attached are the KDM(s) for \"{title}\"."));
    build_message(
        &config.from,
        to,
        &subject,
        &body,
        LEGACY_ATTACHMENT_NAME,
        zip,
        ZIP_MIME_TYPE,
    )
}

pub fn send_kdms(
    config: &SmtpConfig,
    cinema: &str,
    title: &str,
    to: &[String],
    kdm_files: &[std::path::PathBuf],
) -> Result<(), String> {
    let msg = build_kdm_email(config, cinema, title, to, kdm_files)?;
    send(config, &msg)
}

// the 2009 KDM Naming Convention: the subject line is the ZIP file name
pub fn build_bundle_email(
    config: &SmtpConfig,
    bundle: &CinemaBundle,
    title: &str,
    to: &[String],
) -> Result<Message, String> {
    let zip = std::fs::read(&bundle.zip_path)
        .map_err(|e| format!("cannot read {}: {e}", bundle.zip_path.display()))?;
    let body = config
        .body_template
        .as_deref()
        .map(|t| substitute(t, title, &bundle.cinema))
        .unwrap_or_else(|| format!("Attached are the KDM(s) for \"{title}\"."));
    build_message(
        &config.from,
        to,
        &bundle.zip_name,
        &body,
        &format!("{}{ZIP_EXTENSION}", bundle.zip_name),
        zip,
        ZIP_MIME_TYPE,
    )
}

pub fn send_bundle(
    config: &SmtpConfig,
    bundle: &CinemaBundle,
    title: &str,
    to: &[String],
) -> Result<(), String> {
    let message = build_bundle_email(config, bundle, title, to)?;
    send(config, &message)
}

pub fn send(config: &SmtpConfig, message: &Message) -> Result<(), String> {
    let mut builder = match config.security {
        Security::Tls => {
            SmtpTransport::relay(&config.host).map_err(|e| format!("smtp tls setup: {e}"))?
        }
        Security::Starttls => SmtpTransport::starttls_relay(&config.host)
            .map_err(|e| format!("smtp starttls setup: {e}"))?,
        Security::None => SmtpTransport::builder_dangerous(&config.host),
    }
    .port(config.port);

    if let (Some(u), Some(p)) = (&config.username, &config.password) {
        builder = builder.credentials(Credentials::new(u.clone(), p.clone()));
    }
    let mailer = builder.build();
    // the error type does not print the password, still only the summary is forwarded
    mailer
        .send(message)
        .map(|_| ())
        .map_err(|e| format!("smtp send to {} failed: {e}", config.host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::bundle::write_zip;
    use crate::kdm_distribution::test_support::fake_server;

    fn smtp_config_from_toml(text: &str) -> SmtpConfig {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn config_parses_and_debug_redacts_password() {
        let cfg = smtp_config_from_toml(
            r#"
            host = "smtp.example.test"
            port = 587
            security = "starttls"
            username = "user@example.test"
            password = "hunter2"
            from = "kdm@example.test"
            subject_template = "KDM for {title}"
            "#,
        );
        assert_eq!(cfg.port, 587);
        assert_eq!(cfg.security, Security::Starttls);
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("hunter2"),
            "password must not appear in Debug"
        );
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn substitution_fills_tokens() {
        assert_eq!(
            substitute("KDM for {title} at {cinema}", "Feature", "Odeon"),
            "KDM for Feature at Odeon"
        );
    }

    #[test]
    fn message_has_headers_and_attachment() {
        let msg = build_message(
            "kdm@example.test",
            &["a@cinema.test".into(), "b@cinema.test".into()],
            "KDM for Feature",
            "See attached.",
            "kdms.zip",
            b"PK\x03\x04zipbytes".to_vec(),
            "application/zip",
        )
        .unwrap();
        let out = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(out.contains("Subject: KDM for Feature"));
        assert!(out.contains("a@cinema.test"));
        assert!(out.contains("b@cinema.test"));
        assert!(out.contains("kdms.zip"), "attachment filename present");
    }

    #[test]
    fn kdm_email_zips_files_and_applies_templates() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("001_screen.kdm.xml");
        std::fs::write(&a, b"<kdm/>").unwrap();
        let cfg = smtp_config_from_toml(
            r#"
            host = "smtp.test"
            port = 465
            from = "kdm@dist.test"
            subject_template = "Keys for {title} at {cinema}"
            "#,
        );
        let msg =
            build_kdm_email(&cfg, "Odeon", "Big Feature", &["a@odeon.test".into()], &[a]).unwrap();
        let out = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(out.contains("Subject: Keys for Big Feature at Odeon"));
        assert!(out.contains("kdms.zip"));
    }

    #[test]
    fn message_requires_a_recipient() {
        let r = build_message(
            "kdm@example.test",
            &[],
            "s",
            "b",
            "f.zip",
            vec![1, 2, 3],
            "application/zip",
        );
        assert!(r.is_err());
    }

    #[test]
    fn kdm_email_reaches_the_smtp_server() {
        const RECIPIENT: &str = "a@odeon.test";
        let dir = tempfile::tempdir().unwrap();
        let kdm = dir.path().join("001_screen.kdm.xml");
        std::fs::write(&kdm, b"<kdm/>").unwrap();
        let (cfg, transcript) = fake_server();
        send_kdms(
            &cfg,
            "Odeon",
            "Big Feature",
            &[RECIPIENT.to_string()],
            &[kdm],
        )
        .unwrap();

        // the 250 to the final dot came back before send_kdms returned
        let delivered = transcript.lock().unwrap();
        assert!(
            delivered
                .commands
                .contains(&format!("RCPT TO:<{RECIPIENT}>")),
            "{}",
            delivered.commands
        );
        assert!(
            delivered
                .body
                .contains("Subject: Keys for Big Feature at Odeon"),
            "{}",
            delivered.body
        );
        assert!(delivered.body.contains("kdms.zip"), "{}", delivered.body);
    }

    #[test]
    fn a_cinema_bundle_is_sent_with_the_zip_name_as_its_subject() {
        const ZIP_NAME: &str = "k_Title_FTR_EN-XX_51_Rex_20261101_20261108_DIS_OV";
        let dir = tempfile::tempdir().unwrap();
        let zip_path = write_zip(
            dir.path(),
            ZIP_NAME,
            &[("k_Title_1001.xml".to_string(), b"<kdm/>".to_vec())],
        )
        .unwrap();
        let bundle = CinemaBundle {
            cinema: "Rex".to_string(),
            emails: vec!["kdm@rex.test".to_string()],
            zip_name: ZIP_NAME.to_string(),
            zip_path,
            kdms: Vec::new(),
        };
        let (cfg, transcript) = fake_server();
        send_bundle(&cfg, &bundle, "Title", &bundle.emails).unwrap();

        let delivered = transcript.lock().unwrap();
        assert!(
            delivered.body.contains(&format!("Subject: {ZIP_NAME}\r\n")),
            "{}",
            delivered.body
        );
        assert!(
            delivered.body.contains(&format!("{ZIP_NAME}.zip")),
            "{}",
            delivered.body
        );
        assert!(delivered.commands.contains("RCPT TO:<kdm@rex.test>"));
    }
}
