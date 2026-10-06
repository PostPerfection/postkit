use super::bundle::{CinemaBundle, KdmNaming, name_fields_from_content_title, write_zip};
use super::cinema::{Cinema, Recipient, Screen};
use super::formulation::{
    ContentStandard, FormulationFlagNames, choose_formulation, resolve_formulation,
};
use super::history;
use super::screen_checks::{CheckReport, check_screen, check_signer};
use super::window::{KdmWindowTimes, LocalWindow, kdm_window_in_time_zone};
use crate::certificate::{
    AudioForensicMarking, KdmConfig, KdmContentKey, KdmFormulation, PictureForensicMarking,
    RewrapConfig, cert_info_from_pem, generate_kdm, parse_kdm, read_certificate,
    resolve_kdm_window, rewrap_dkdm,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CERTIFICATE_EXTENSIONS: [&str; 3] = ["pem", "crt", "cer"];
const KDM_XML_EXTENSION: &str = "xml";
const ADDITIONAL_RECIPIENTS_DIRECTORY: &str = "additional";
const ADDITIONAL_RECIPIENTS_LABEL: &str = "additional recipients";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KdmOptions {
    // None derives the formulation from the device certificates
    pub formulation: Option<KdmFormulation>,
    pub picture_forensic_marking: PictureForensicMarking,
    pub audio_forensic_marking: AudioForensicMarking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdmSigner {
    pub certificate: PathBuf,
    pub key: PathBuf,
    // the CA certificates above the signer leaf, intermediate first
    pub chain: Vec<PathBuf>,
}

impl KdmSigner {
    fn leaf_first_chain(&self) -> Vec<PathBuf> {
        std::iter::once(self.certificate.clone())
            .chain(self.chain.iter().cloned())
            .collect()
    }
}

// a KDM issued from content keys, the dcpwizard path
#[derive(Debug, Clone)]
pub struct KdmRequest {
    pub cpl_id: String,
    pub content_title: String,
    pub signer: KdmSigner,
    // "now", RFC 3339 or a duration such as "2 weeks"
    pub valid_from: String,
    pub valid_to: String,
    // empty makes postkit mint a fresh content key
    pub content_keys: Vec<KdmContentKey>,
    pub annotation: Option<String>,
    pub history: Option<PathBuf>,
    // empty writes the DCI assume-trust thumbprint
    pub device_certs: Vec<PathBuf>,
    pub options: KdmOptions,
    pub formulation_flags: FormulationFlagNames,
}

// a logging failure warns, the KDM is already written
fn log_history(
    history_path: &Path,
    cpl_id: &str,
    content_title: &str,
    recipient_cert: &Path,
    valid_from: &str,
    valid_to: &str,
    output: &Path,
) {
    let info = read_certificate(recipient_cert);
    let record = history::Record::now(
        cpl_id,
        content_title,
        &info.subject_cn,
        &info.serial,
        valid_from,
        valid_to,
        &output.display().to_string(),
    );
    if let Err(e) = history::append(history_path, &record) {
        tracing::warn!("could not append KDM history: {e}");
    }
}

pub fn issue_kdm(request: &KdmRequest, recipient_cert: &Path, output: &Path) -> Result<(), String> {
    let (valid_from, valid_to) = resolve_kdm_window(&request.valid_from, &request.valid_to)?;
    let formulation = resolve_formulation(
        request.options.formulation,
        request.device_certs.len(),
        request.formulation_flags,
    )?;
    let config = KdmConfig {
        cpl_id: request.cpl_id.clone(),
        content_title: request.content_title.clone(),
        annotation: request.annotation.clone(),
        recipient_cert_file: recipient_cert.to_path_buf(),
        signer_cert_file: request.signer.certificate.clone(),
        signer_key_file: request.signer.key.clone(),
        signer_chain_files: request.signer.chain.clone(),
        output_file: output.to_path_buf(),
        valid_from,
        valid_to,
        formulation,
        content_keys: request.content_keys.clone(),
        device_cert_files: request.device_certs.clone(),
        picture_forensic_marking: request.options.picture_forensic_marking,
        audio_forensic_marking: request.options.audio_forensic_marking,
        issue_date: None,
    };
    generate_kdm(&config)?;
    if let Some(history_path) = &request.history {
        log_history(
            history_path,
            &config.cpl_id,
            &config.content_title,
            &config.recipient_cert_file,
            &config.valid_from,
            &config.valid_to,
            &config.output_file,
        );
    }
    Ok(())
}

// one KDM per certificate, written to output_dir/NNN_<cert stem>.kdm.xml
pub fn issue_kdm_batch(
    request: &KdmRequest,
    recipient_certs: &[PathBuf],
    output_dir: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create output directory: {e}"))?;
    let mut failures = 0;
    for (index, cert) in recipient_certs.iter().enumerate() {
        let stem = cert
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("recipient");
        // the index keeps outputs apart when certificates share a file name
        let output = output_dir.join(format!("{:03}_{stem}.kdm.xml", index + 1));
        match issue_kdm(request, cert, &output) {
            Ok(()) => tracing::info!("KDM for {} -> {}", cert.display(), output.display()),
            Err(e) => {
                tracing::error!("{e}");
                tracing::error!("KDM generation failed for {}", cert.display());
                failures += 1;
            }
        }
    }
    if failures == 0 {
        tracing::info!("Generated {} KDM(s)", recipient_certs.len());
        Ok(())
    } else {
        Err(format!(
            "{failures} of {} KDM(s) failed",
            recipient_certs.len()
        ))
    }
}

// every *.pem, *.crt and *.cer, sorted, and an error when there are none
pub fn certs_in_dir(dir: &Path) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read cert directory {}: {e}", dir.display()))?;
    let mut certs: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
                    CERTIFICATE_EXTENSIONS.contains(&x.to_ascii_lowercase().as_str())
                })
        })
        .filter_map(|p| p.to_str().map(String::from))
        .collect();
    certs.sort();
    if certs.is_empty() {
        return Err(format!(
            "no certificates (*.pem/*.crt/*.cer) found in {}",
            dir.display()
        ));
    }
    Ok(certs)
}

// recipients that share one delivery email: a cinema, or the loose certificates with an empty name
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientGroup {
    pub name: String,
    pub emails: Vec<String>,
    pub cert_paths: Vec<PathBuf>,
}

pub fn group_recipients(loose: Vec<PathBuf>, recipients: Vec<Recipient>) -> Vec<RecipientGroup> {
    let mut groups = Vec::new();
    if !loose.is_empty() {
        groups.push(RecipientGroup {
            name: String::new(),
            emails: Vec::new(),
            cert_paths: loose,
        });
    }
    for recipient in recipients {
        match groups
            .iter_mut()
            .find(|group: &&mut RecipientGroup| group.name == recipient.cinema)
        {
            Some(group) => group.cert_paths.push(recipient.cert_path),
            None => groups.push(RecipientGroup {
                name: recipient.cinema,
                emails: recipient.emails,
                cert_paths: vec![recipient.cert_path],
            }),
        }
    }
    groups
}

fn directory_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn xml_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some(KDM_XML_EXTENSION))
        .collect();
    files.sort();
    files
}

// sends one group's KDM files: (group name, addresses, files)
pub type SendGroup<'a> = dyn FnMut(&str, &[String], &[PathBuf]) -> Result<(), String> + 'a;

pub struct GroupDelivery<'a> {
    pub extra_addresses: &'a [String],
    pub only_extra_addresses: bool,
    pub send: &'a mut SendGroup<'a>,
}

// one email per group, each with that group's KDMs, and the number of groups that failed
pub fn issue_and_send_groups(
    request: &KdmRequest,
    groups: &[RecipientGroup],
    output_root: &Path,
    delivery: GroupDelivery<'_>,
) -> usize {
    let several_groups = groups.len() > 1;
    let mut failures = 0;
    for group in groups {
        let out_dir = if several_groups {
            let directory = if group.name.is_empty() {
                ADDITIONAL_RECIPIENTS_DIRECTORY.to_string()
            } else {
                directory_name(&group.name)
            };
            output_root.join(directory)
        } else {
            output_root.to_path_buf()
        };
        if let Err(e) = issue_kdm_batch(request, &group.cert_paths, &out_dir) {
            tracing::error!("{e}");
            failures += 1;
            continue;
        }
        let mut to: Vec<String> = if delivery.only_extra_addresses {
            Vec::new()
        } else {
            group.emails.clone()
        };
        for address in delivery.extra_addresses {
            if !to.contains(address) {
                to.push(address.clone());
            }
        }
        let files = xml_files_in(&out_dir);
        let label = if group.name.is_empty() {
            ADDITIONAL_RECIPIENTS_LABEL
        } else {
            &group.name
        };
        match (delivery.send)(&group.name, &to, &files) {
            Ok(()) => tracing::info!("emailed {} KDM(s) for {label}", files.len()),
            Err(e) => {
                tracing::error!("{label}: {e}");
                failures += 1;
            }
        }
    }
    failures
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedKdm {
    pub cinema: String,
    pub screen: String,
    pub file_name: String,
    pub formulation: KdmFormulation,
    pub not_valid_before: String,
    pub not_valid_after: String,
    pub issue_date: DateTime<Utc>,
    pub recipient_subject: String,
    pub recipient_serial: String,
    pub recipient_thumbprint: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRefusal {
    pub cinema: String,
    pub screen: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkdmIssueOutcome {
    pub cpl_id: String,
    pub content_title: String,
    pub bundles: Vec<CinemaBundle>,
    pub refused: Vec<ScreenRefusal>,
    pub not_checked: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DkdmIssue<'a> {
    pub dkdm_xml: &'a str,
    pub dkdm_recipient_key: &'a Path,
    pub signer: &'a KdmSigner,
    // None reads the standard from the DCNC content title
    pub standard: Option<ContentStandard>,
    pub formulation: Option<KdmFormulation>,
    pub picture_forensic_marking: PictureForensicMarking,
    pub audio_forensic_marking: AudioForensicMarking,
    // the three letters naming the facility that issues the KDMs
    pub creation_facility: &'a str,
    pub issue_date: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub struct ScreenTarget<'a> {
    pub cinema: &'a Cinema,
    pub screen: &'a Screen,
    pub window: LocalWindow,
}

struct WrittenKdm {
    issued: IssuedKdm,
    xml: String,
    start_date: NaiveDate,
    end_date: NaiveDate,
}

fn screen_label(cinema: &Cinema, screen: &Screen) -> String {
    format!("{} / {}", cinema.name, screen.name)
}

fn findings_text(report: &CheckReport) -> Vec<String> {
    report.failures.iter().map(ToString::to_string).collect()
}

fn write_temporary(directory: &Path, contents: &str) -> Result<PathBuf, String> {
    let path = directory.join(format!("{}.pem", uuid::Uuid::new_v4()));
    std::fs::write(&path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

fn outside_dkdm_window(window: &KdmWindowTimes, dkdm_from: &str, dkdm_to: &str) -> Option<String> {
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    let (from, to) = (parse(dkdm_from)?, parse(dkdm_to)?);
    let inside =
        from.timestamp() <= window.start.timestamp() && window.end.timestamp() <= to.timestamp();
    (!inside).then(|| {
        format!(
            "the window {} to {} reaches outside the DKDM's {dkdm_from} to {dkdm_to}",
            window.not_valid_before, window.not_valid_after
        )
    })
}

struct DkdmContext<'a> {
    issue: &'a DkdmIssue<'a>,
    dkdm_file: PathBuf,
    temporary: &'a Path,
    standard: Option<ContentStandard>,
    dkdm_not_valid_before: String,
    dkdm_not_valid_after: String,
    naming_fields: super::bundle::KdmNameFields,
}

fn issue_screen(
    context: &DkdmContext<'_>,
    target: &ScreenTarget<'_>,
    not_checked: &mut Vec<String>,
) -> Result<WrittenKdm, Vec<String>> {
    let issue = context.issue;
    let label = screen_label(target.cinema, target.screen);
    let zone = target.cinema.time_zone.as_deref().ok_or_else(|| {
        vec![format!(
            "{label}: cinema '{}' has no time zone, set one so the window can be written in local time",
            target.cinema.name
        )]
    })?;
    let window =
        kdm_window_in_time_zone(&target.window, zone).map_err(|e| vec![format!("{label}: {e}")])?;

    let signer_report = check_signer(&issue.signer.leaf_first_chain(), &window, issue.issue_date);
    let screen_report = check_screen(&label, target.screen, &window, issue.issue_date);
    for note in signer_report
        .not_checked
        .iter()
        .chain(&screen_report.not_checked)
    {
        if !not_checked.contains(note) {
            not_checked.push(note.clone());
        }
    }
    let mut reasons = findings_text(&signer_report);
    reasons.extend(findings_text(&screen_report));
    if !reasons.is_empty() {
        return Err(reasons);
    }
    let mut warnings: Vec<String> = screen_report
        .warnings
        .iter()
        .map(ToString::to_string)
        .collect();

    let choice = choose_formulation(
        issue.formulation,
        target.screen.authorized_devices.len(),
        context.standard,
    )
    .map_err(|e| vec![format!("{label}: {e}")])?;
    warnings.extend(
        choice
            .warnings
            .iter()
            .map(|warning| format!("{label}: {warning}")),
    );
    if let Some(warning) = outside_dkdm_window(
        &window,
        &context.dkdm_not_valid_before,
        &context.dkdm_not_valid_after,
    ) {
        warnings.push(format!("{label}: {warning}"));
    }

    let write = |contents: &str| write_temporary(context.temporary, contents);
    let recipient_pem = target
        .screen
        .cert
        .pem()
        .map_err(|e| vec![format!("{label}: {e}")])?;
    let recipient_file = write(&recipient_pem).map_err(|e| vec![e])?;
    let device_cert_files = if choice.formulation.lists_supplied_devices() {
        target
            .screen
            .authorized_devices
            .iter()
            .map(|device| write(&device.certificate))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| vec![e])?
    } else {
        Vec::new()
    };
    let config = RewrapConfig {
        dkdm_file: context.dkdm_file.clone(),
        dkdm_recipient_key_file: issue.dkdm_recipient_key.to_path_buf(),
        recipient_cert_file: recipient_file,
        signer_cert_file: issue.signer.certificate.clone(),
        signer_key_file: issue.signer.key.clone(),
        signer_chain_files: issue.signer.chain.clone(),
        output_file: PathBuf::new(),
        valid_from: window.not_valid_before.clone(),
        valid_to: window.not_valid_after.clone(),
        device_cert_files,
        formulation: choice.formulation,
        picture_forensic_marking: issue.picture_forensic_marking,
        audio_forensic_marking: issue.audio_forensic_marking,
        issue_date: Some(issue.issue_date),
    };
    let generated = rewrap_dkdm(&config).map_err(|e| vec![format!("{label}: {e}")])?;
    let recipient =
        cert_info_from_pem(&recipient_pem).map_err(|e| vec![format!("{label}: {e}")])?;

    let naming = KdmNaming {
        fields: &context.naming_fields,
        creation_facility: issue.creation_facility,
        active: window.start_date,
        inactive: window.end_date,
    };
    let serial = target
        .screen
        .device_serial
        .as_deref()
        .unwrap_or(&target.screen.name);
    Ok(WrittenKdm {
        issued: IssuedKdm {
            cinema: target.cinema.name.clone(),
            screen: target.screen.name.clone(),
            file_name: naming.kdm_file_name(serial),
            formulation: choice.formulation,
            not_valid_before: window.not_valid_before,
            not_valid_after: window.not_valid_after,
            issue_date: issue.issue_date,
            recipient_subject: recipient.subject_cn,
            recipient_serial: recipient.serial,
            recipient_thumbprint: recipient.thumbprint,
            warnings,
        },
        xml: generated.xml,
        start_date: window.start_date,
        end_date: window.end_date,
    })
}

// re-wraps the DKDM once per screen and writes one ZIP per cinema into output_dir
pub fn issue_from_dkdm(
    issue: &DkdmIssue<'_>,
    targets: &[ScreenTarget<'_>],
    output_dir: &Path,
) -> Result<DkdmIssueOutcome, String> {
    let metadata = parse_kdm(issue.dkdm_xml)?;
    let naming_fields = name_fields_from_content_title(&metadata.content_title);
    let temporary = tempfile::tempdir().map_err(|e| format!("cannot create temp dir: {e}"))?;
    let dkdm_file = temporary.path().join("dkdm.xml");
    std::fs::write(&dkdm_file, issue.dkdm_xml)
        .map_err(|e| format!("cannot write {}: {e}", dkdm_file.display()))?;
    let context = DkdmContext {
        issue,
        dkdm_file,
        temporary: temporary.path(),
        standard: issue.standard.or(naming_fields.standard),
        dkdm_not_valid_before: metadata.not_valid_before.clone(),
        dkdm_not_valid_after: metadata.not_valid_after.clone(),
        naming_fields,
    };

    let mut outcome = DkdmIssueOutcome {
        cpl_id: metadata.cpl_id.to_string(),
        content_title: metadata.content_title.clone(),
        ..Default::default()
    };
    let mut written: Vec<(&Cinema, WrittenKdm)> = Vec::new();
    for target in targets {
        match issue_screen(&context, target, &mut outcome.not_checked) {
            Ok(kdm) => written.push((target.cinema, kdm)),
            Err(reasons) => outcome.refused.push(ScreenRefusal {
                cinema: target.cinema.name.clone(),
                screen: target.screen.name.clone(),
                reasons,
            }),
        }
    }

    let mut cinemas: Vec<&Cinema> = Vec::new();
    for (cinema, _) in &written {
        if !cinemas.iter().any(|known| known.name == cinema.name) {
            cinemas.push(cinema);
        }
    }
    for cinema in cinemas {
        let kdms: Vec<&WrittenKdm> = written
            .iter()
            .filter(|(owner, _)| owner.name == cinema.name)
            .map(|(_, kdm)| kdm)
            .collect();
        let active = kdms.iter().map(|kdm| kdm.start_date).min();
        let inactive = kdms.iter().map(|kdm| kdm.end_date).max();
        let (Some(active), Some(inactive)) = (active, inactive) else {
            continue;
        };
        let zip_name = KdmNaming {
            fields: &context.naming_fields,
            creation_facility: issue.creation_facility,
            active,
            inactive,
        }
        .zip_name(&cinema.name);
        let entries: Vec<(String, Vec<u8>)> = kdms
            .iter()
            .map(|kdm| (kdm.issued.file_name.clone(), kdm.xml.clone().into_bytes()))
            .collect();
        let zip_path = write_zip(output_dir, &zip_name, &entries)?;
        outcome.bundles.push(CinemaBundle {
            cinema: cinema.name.clone(),
            emails: cinema.emails.clone(),
            zip_name,
            zip_path,
            kdms: kdms.iter().map(|kdm| kdm.issued.clone()).collect(),
        });
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certificate::{ChainRule, cert_info_from_file, unwrap_kdm};
    use crate::kdm_distribution::test_support::{
        CPL_ID, DCNC_TITLE, chain_screen, cinemas, content_keys, dkdm, fixtures, local_window, read,
    };
    use crate::xmldsig::xmlsec1_cli;
    use chrono::Duration;
    use std::io::Read;

    const COMMAND_LINE: FormulationFlagNames = FormulationFlagNames {
        formulation: "--formulation",
        device_certificate: "--device-cert",
    };
    const KDM_ID_ATTRIBUTES: &[&str] = &["AuthenticatedPublic", "AuthenticatedPrivate"];
    // DCI DCSS 9.4.3.5: base64 SHA-1 of nothing
    const ASSUME_TRUST: &str = "2jmj7l5rSw0yVb/vlWAYkK/YBwk=";
    const ISSUE_DATE_FORMAT: &str = "%Y-%m-%dT%H:%M:%S+00:00";
    const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

    // a certificate minted today cannot sign a window starting today, so windows start tomorrow
    fn signable_window() -> (String, String) {
        let start = Utc::now() + Duration::days(1);
        let end = start + Duration::days(7);
        (
            start.format(ISSUE_DATE_FORMAT).to_string(),
            end.format(ISSUE_DATE_FORMAT).to_string(),
        )
    }

    fn request(content_keys: Vec<KdmContentKey>, history: Option<PathBuf>) -> KdmRequest {
        let (valid_from, valid_to) = signable_window();
        KdmRequest {
            cpl_id: CPL_ID.to_string(),
            content_title: "Test Feature".to_string(),
            signer: fixtures().signer(),
            valid_from,
            valid_to,
            content_keys,
            annotation: None,
            history,
            device_certs: Vec::new(),
            options: KdmOptions::default(),
            formulation_flags: COMMAND_LINE,
        }
    }

    fn recipients_dir(dir: &Path, count: usize) -> PathBuf {
        let recipients = dir.join("recipients");
        std::fs::create_dir_all(&recipients).unwrap();
        for (index, manager) in fixtures().security_managers.iter().take(count).enumerate() {
            std::fs::copy(
                &manager.certificate,
                recipients.join(format!("screen_{index}.pem")),
            )
            .unwrap();
            std::fs::copy(&manager.key, recipients.join(format!("screen_{index}.key"))).unwrap();
        }
        recipients
    }

    fn assert_verifies(kdm: &Path) {
        let f = fixtures();
        let output = xmlsec1_cli::verify(
            kdm,
            &f.distributor_chain[1],
            &[&f.distributor_chain[0]],
            KDM_ID_ATTRIBUTES,
        );
        assert!(
            output.status.success(),
            "xmlsec1 must verify {}: {}",
            kdm.display(),
            xmlsec1_cli::report(&output)
        );
    }

    #[test]
    fn an_empty_cpl_id_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut request = request(Vec::new(), None);
        request.cpl_id = String::new();
        let recipient = &fixtures().security_managers[0].certificate;
        assert!(issue_kdm(&request, recipient, &dir.path().join("k.xml")).is_err());
    }

    #[test]
    fn a_duration_end_keeps_the_start_offset_and_an_absolute_end_passes_through() {
        let (from, to) =
            crate::certificate::resolve_kdm_window("2024-06-01T00:00:00+02:00", "1 day").unwrap();
        assert_eq!(from, "2024-06-01T00:00:00+02:00");
        assert_eq!(to, "2024-06-02T00:00:00+02:00");
        let (_, to) = crate::certificate::resolve_kdm_window(
            "2024-06-01T00:00:00+00:00",
            "2024-06-15T00:00:00+00:00",
        )
        .unwrap();
        assert_eq!(to, "2024-06-15T00:00:00+00:00");
    }

    #[test]
    fn certs_in_dir_lists_only_certs_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.pem"), "x").unwrap();
        std::fs::write(dir.path().join("a.crt"), "x").unwrap();
        std::fs::write(dir.path().join("k.key"), "x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let found = certs_in_dir(dir.path()).unwrap();
        assert_eq!(found.len(), 2, "only cert extensions counted");
        assert!(found[0].ends_with("a.crt") && found[1].ends_with("b.pem"));
    }

    #[test]
    fn certs_in_dir_empty_or_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            certs_in_dir(dir.path()).is_err(),
            "empty dir must fail loud"
        );
        assert!(certs_in_dir(Path::new("/nonexistent/certs")).is_err());
    }

    #[test]
    fn a_batch_reports_failure_for_a_bad_recipient() {
        let dir = tempfile::tempdir().unwrap();
        let error = issue_kdm_batch(
            &request(Vec::new(), None),
            &[PathBuf::from("/nonexistent/recipient.pem")],
            &dir.path().join("out"),
        )
        .unwrap_err();
        assert_eq!(error, "1 of 1 KDM(s) failed");
    }

    #[test]
    fn a_batch_writes_one_signed_kdm_per_recipient_bound_to_the_key_and_logs_it() {
        let dir = tempfile::tempdir().unwrap();
        let recipients = recipients_dir(dir.path(), 2);
        let certs: Vec<PathBuf> = certs_in_dir(&recipients)
            .unwrap()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(certs.len(), 2, "cert_dir globbing skips the .key files");
        let history_path = dir.path().join("history.jsonl");
        let out = dir.path().join("kdms");
        issue_kdm_batch(
            &request(content_keys(), Some(history_path.clone())),
            &certs,
            &out,
        )
        .unwrap();

        let records = history::read_all(&history_path).unwrap();
        assert_eq!(records.len(), 2, "one history record per KDM");
        assert_eq!(records[0].content_title, "Test Feature");
        assert!(!records[0].recipient_serial.is_empty());
        assert!(!read(&history_path).to_lowercase().contains("key"));

        for (index, cert) in certs.iter().enumerate() {
            let stem = cert.file_stem().unwrap().to_str().unwrap();
            let kdm = out.join(format!("{:03}_{stem}.kdm.xml", index + 1));
            let xml = read(&kdm);
            let unwrapped = unwrap_kdm(&xml, &recipients.join(format!("{stem}.key"))).unwrap();
            for key in content_keys() {
                assert_eq!(unwrapped.content_key(&key.key_id), Some(&key.content_key));
            }
            assert_verifies(&kdm);
        }
    }

    #[test]
    fn an_annotation_override_lands_escaped_in_the_kdm() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("annotated.kdm.xml");
        let mut request = request(Vec::new(), None);
        request.annotation = Some("Release KDM <v2> & final".into());
        issue_kdm(&request, &fixtures().security_managers[0].certificate, &out).unwrap();
        assert!(
            read(&out)
                .contains("<AnnotationText>Release KDM &lt;v2&gt; &amp; final</AnnotationText>")
        );
    }

    #[test]
    fn device_certs_replace_the_assume_trust_thumbprint() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixtures();
        let kdm_for = |name: &str, devices: Vec<PathBuf>| {
            let out = dir.path().join(name);
            let mut request = request(Vec::new(), None);
            request.device_certs = devices;
            issue_kdm(&request, &f.security_managers[0].certificate, &out).unwrap();
            read(&out)
        };
        assert!(kdm_for("open.xml", Vec::new()).contains(ASSUME_TRUST));
        let restricted = kdm_for("restricted.xml", vec![f.link_decryptor.certificate.clone()]);
        assert!(!restricted.contains(ASSUME_TRUST));
        let thumbprint = cert_info_from_file(&f.link_decryptor.certificate)
            .unwrap()
            .thumbprint;
        assert!(restricted.contains(&format!(
            "<CertificateThumbprint>{thumbprint}</CertificateThumbprint>"
        )));
    }

    #[test]
    fn groups_go_to_their_own_folders_and_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixtures();
        let groups = group_recipients(
            vec![f.security_managers[0].certificate.clone()],
            vec![Recipient {
                cinema: "Rex".into(),
                emails: vec!["kdm@rex.test".into()],
                screen: "1".into(),
                cert_path: f.security_managers[1].certificate.clone(),
            }],
        );
        assert_eq!(groups.len(), 2);
        let mut sent = Vec::new();
        let mut send = |name: &str, to: &[String], files: &[PathBuf]| {
            sent.push((name.to_string(), to.to_vec(), files.len()));
            Ok(())
        };
        let failures = issue_and_send_groups(
            &request(Vec::new(), None),
            &groups,
            dir.path(),
            GroupDelivery {
                extra_addresses: &["office@dist.test".to_string()],
                only_extra_addresses: false,
                send: &mut send,
            },
        );
        assert_eq!(failures, 0);
        assert_eq!(
            sent,
            vec![
                (String::new(), vec!["office@dist.test".to_string()], 1),
                (
                    "Rex".to_string(),
                    vec!["kdm@rex.test".to_string(), "office@dist.test".to_string()],
                    1
                ),
            ]
        );
        assert!(dir.path().join("additional").is_dir());
        assert!(dir.path().join("Rex").is_dir());
    }

    fn element<'a>(xml: &'a str, name: &str) -> &'a str {
        let open = format!("<{name}>");
        let start = xml.find(&open).unwrap() + open.len();
        let end = start + xml[start..].find('<').unwrap();
        &xml[start..end]
    }

    fn zip_entries(path: &Path) -> Vec<(String, String)> {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        (0..archive.len())
            .map(|index| {
                let mut entry = archive.by_index(index).unwrap();
                let mut content = String::new();
                entry.read_to_string(&mut content).unwrap();
                (entry.name().to_string(), content)
            })
            .collect()
    }

    fn dkdm_issue<'a>(
        dkdm_xml: &'a str,
        signer: &'a KdmSigner,
        issue_date: DateTime<Utc>,
    ) -> DkdmIssue<'a> {
        DkdmIssue {
            dkdm_xml,
            dkdm_recipient_key: &fixtures().distributor_signer_key,
            signer,
            standard: None,
            formulation: None,
            picture_forensic_marking: PictureForensicMarking::default(),
            audio_forensic_marking: AudioForensicMarking::default(),
            creation_facility: "dis",
            issue_date,
        }
    }

    #[test]
    fn a_dkdm_issues_one_zip_per_cinema_whose_kdms_unwrap_to_its_content_keys() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let (rex, odeon) = cinemas();
        let window = local_window();
        let targets: Vec<ScreenTarget<'_>> = rex
            .screens
            .iter()
            .map(|screen| ScreenTarget {
                cinema: &rex,
                screen,
                window,
            })
            .chain(odeon.screens.iter().map(|screen| ScreenTarget {
                cinema: &odeon,
                screen,
                window,
            }))
            .collect();
        let dkdm_xml = dkdm(DCNC_TITLE, 30);
        let signer = f.signer();
        let issue_date = Utc::now() + Duration::minutes(5);
        let outcome = issue_from_dkdm(
            &dkdm_issue(&dkdm_xml, &signer, issue_date),
            &targets,
            dir.path(),
        )
        .unwrap();
        assert!(outcome.refused.is_empty(), "{:#?}", outcome.refused);
        assert!(
            outcome
                .not_checked
                .iter()
                .any(|note| note.contains("rule 12"))
        );
        assert_eq!(outcome.bundles.len(), 2);

        let dates = format!(
            "{}_{}",
            window.start.format("%Y%m%d"),
            window.end.format("%Y%m%d")
        );
        let rex_bundle = &outcome.bundles[0];
        assert_eq!(
            rex_bundle.zip_name,
            format!("k_LongerThanFo3D_FTR_EN-XX_51-HI-VI_Rex_{dates}_DIS_OV_US-13")
        );
        let entries = zip_entries(&rex_bundle.zip_path);
        let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                format!("k_LongerThanFo3D_FTR_EN-XX_51-HI-VI_1001_{dates}_DIS_OV_US-13.xml"),
                format!("k_LongerThanFo3D_FTR_EN-XX_51-HI-VI_1002_{dates}_DIS_OV_US-13.xml"),
            ]
        );

        let local_start = window.start.format(LOCAL_FORMAT).to_string();
        let keyed = [
            (&entries[0].1, &f.security_managers[0].key),
            (&entries[1].1, &f.security_managers[1].key),
            (
                &zip_entries(&outcome.bundles[1].zip_path)[0].1,
                &f.security_managers[2].key,
            ),
        ];
        for (xml, key) in keyed {
            let metadata = parse_kdm(xml).unwrap();
            assert_eq!(metadata.cpl_id.to_string(), CPL_ID);
            assert_eq!(metadata.content_title, DCNC_TITLE);
            assert!(
                metadata.not_valid_before.starts_with(&local_start),
                "{}",
                metadata.not_valid_before
            );
            for timestamp in [&metadata.not_valid_before, &metadata.not_valid_after] {
                assert_eq!(timestamp.len(), 25);
                assert!(!timestamp.ends_with('Z'));
            }
            let unwrapped = unwrap_kdm(xml, key).unwrap();
            assert_eq!(unwrapped.keys.len(), content_keys().len());
            for content_key in content_keys() {
                assert_eq!(
                    unwrapped.content_key(&content_key.key_id),
                    Some(&content_key.content_key)
                );
            }
            assert_eq!(
                element(xml, "IssueDate"),
                issue_date.format(ISSUE_DATE_FORMAT).to_string()
            );
        }

        // screen 1 lists its two devices, screen 2 has none and trusts any
        let devices = [&f.link_decryptor.certificate, &f.projector.certificate];
        for device in devices {
            let thumbprint = cert_info_from_file(device).unwrap().thumbprint;
            assert!(
                entries[0].1.contains(&thumbprint),
                "screen 1 lists {}",
                device.display()
            );
        }
        assert!(!entries[0].1.contains(ASSUME_TRUST));
        assert!(entries[1].1.contains(ASSUME_TRUST));
        assert_eq!(
            rex_bundle.kdms[0].formulation,
            KdmFormulation::MultipleModifiedTransitional1
        );
        assert_eq!(
            rex_bundle.kdms[1].formulation,
            KdmFormulation::ModifiedTransitional1
        );

        let written = dir.path().join("screen1.xml");
        std::fs::write(&written, &entries[0].1).unwrap();
        assert_verifies(&written);
    }

    #[test]
    fn every_issue_carries_its_own_issue_date() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let (rex, _) = cinemas();
        let targets = [ScreenTarget {
            cinema: &rex,
            screen: &rex.screens[1],
            window: local_window(),
        }];
        let dkdm_xml = dkdm(DCNC_TITLE, 30);
        let signer = f.signer();
        let first_date = Utc::now() + Duration::minutes(1);
        let second_date = first_date + Duration::hours(3);
        let issue_dates: Vec<String> = [first_date, second_date]
            .into_iter()
            .map(|date| {
                let outcome = issue_from_dkdm(
                    &dkdm_issue(&dkdm_xml, &signer, date),
                    &targets,
                    &dir.path().join(date.timestamp().to_string()),
                )
                .unwrap();
                let entries = zip_entries(&outcome.bundles[0].zip_path);
                element(&entries[0].1, "IssueDate").to_string()
            })
            .collect();
        assert_eq!(
            issue_dates[0],
            first_date.format(ISSUE_DATE_FORMAT).to_string()
        );
        assert_eq!(
            issue_dates[1],
            second_date.format(ISSUE_DATE_FORMAT).to_string()
        );
        assert_ne!(issue_dates[0], element(&dkdm_xml, "IssueDate"));
    }

    #[test]
    fn a_refused_screen_is_named_and_the_others_are_still_issued() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let (rex, mut odeon) = cinemas();
        odeon.time_zone = None;
        let wrong_role = chain_screen("3", "2001", &f.link_decryptor.certificate, &[]);
        let window = local_window();
        let targets = [
            ScreenTarget {
                cinema: &rex,
                screen: &rex.screens[1],
                window,
            },
            ScreenTarget {
                cinema: &rex,
                screen: &wrong_role,
                window,
            },
            ScreenTarget {
                cinema: &odeon,
                screen: &odeon.screens[0],
                window,
            },
        ];
        let dkdm_xml = dkdm(DCNC_TITLE, 30);
        let signer = f.signer();
        let outcome = issue_from_dkdm(
            &dkdm_issue(&dkdm_xml, &signer, Utc::now()),
            &targets,
            dir.path(),
        )
        .unwrap();
        assert_eq!(outcome.bundles.len(), 1);
        assert_eq!(outcome.bundles[0].kdms.len(), 1);
        assert_eq!(outcome.refused.len(), 2);
        let role = &outcome.refused[0];
        assert_eq!((role.cinema.as_str(), role.screen.as_str()), ("Rex", "3"));
        assert!(
            role.reasons[0].contains("Rex / 3: recipient certificate"),
            "{:?}",
            role.reasons
        );
        assert!(
            role.reasons[0].contains(&ChainRule::Role.to_string()),
            "{:?}",
            role.reasons
        );
        assert!(
            outcome.refused[1].reasons[0].contains("has no time zone"),
            "{:?}",
            outcome.refused[1].reasons
        );
    }

    #[test]
    fn a_window_beyond_the_dkdm_is_issued_with_a_warning() {
        let f = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let (rex, _) = cinemas();
        let targets = [ScreenTarget {
            cinema: &rex,
            screen: &rex.screens[1],
            window: local_window(),
        }];
        let short_dkdm = dkdm(DCNC_TITLE, 3);
        let signer = f.signer();
        let outcome = issue_from_dkdm(
            &dkdm_issue(&short_dkdm, &signer, Utc::now()),
            &targets,
            dir.path(),
        )
        .unwrap();
        let warnings = &outcome.bundles[0].kdms[0].warnings;
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("outside the DKDM")),
            "{warnings:?}"
        );
    }
}
