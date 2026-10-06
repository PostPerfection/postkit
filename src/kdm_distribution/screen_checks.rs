// ISDCF Doc 5 Annex A checks run before any KDM for a screen is written
use super::cinema::Screen;
use super::window::KdmWindowTimes;
use crate::certificate::{
    AUTHORIZED_DEVICE_BEST_EFFORT_RULES, AUTHORIZED_DEVICE_ROLES, ChainCertificate, ChainContext,
    ChainReport, ChainRule, KDM_RECIPIENT_ROLES, LeafRoles, chain_from_files, chain_from_pem,
    check_chain,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use x509_parser::prelude::*;

const SIGNER_CHAIN_NAME: &str = "signer chain";
const RECIPIENT_CHAIN_NAME: &str = "recipient";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CertificateCheck {
    Rule(ChainRule),
    WindowInsideRecipient,
    Readable,
}

impl std::fmt::Display for CertificateCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule(rule) => write!(f, "{rule}"),
            Self::WindowInsideRecipient => f.write_str("KDM window inside the recipient validity"),
            Self::Readable => f.write_str("readable certificate"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckFinding {
    pub screen: String,
    pub chain: String,
    pub certificate: String,
    pub check: CertificateCheck,
    pub detail: String,
}

impl std::fmt::Display for CheckFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} certificate {}: {}: {}",
            self.screen, self.chain, self.certificate, self.check, self.detail
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    pub failures: Vec<CheckFinding>,
    pub warnings: Vec<CheckFinding>,
    pub not_checked: Vec<String>,
}

impl CheckReport {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    fn add_chain(&mut self, screen: &str, chain: &str, report: ChainReport) {
        let finding = |rule_finding: crate::certificate::ChainRuleFinding| CheckFinding {
            screen: screen.to_string(),
            chain: chain.to_string(),
            certificate: rule_finding.certificate,
            check: CertificateCheck::Rule(rule_finding.rule),
            detail: rule_finding.detail,
        };
        self.failures
            .extend(report.failures.into_iter().map(finding));
        self.warnings
            .extend(report.best_effort_failures.into_iter().map(finding));
        for skipped in report.not_checked {
            // the same for every chain, so one note stands for all of them
            let note = format!("{} not checked: {}", skipped.rule, skipped.reason);
            if !self.not_checked.contains(&note) {
                self.not_checked.push(note);
            }
        }
    }

    fn unreadable(&mut self, screen: &str, chain: &str, detail: String) {
        self.failures.push(CheckFinding {
            screen: screen.to_string(),
            chain: chain.to_string(),
            certificate: chain.to_string(),
            check: CertificateCheck::Readable,
            detail,
        });
    }
}

fn check_pem_chain(
    report: &mut CheckReport,
    screen: &str,
    chain_name: &str,
    pem: &str,
    context: &ChainContext,
) -> Option<Vec<ChainCertificate>> {
    match chain_from_pem(pem) {
        Ok(chain) => {
            report.add_chain(screen, chain_name, check_chain(&chain, context));
            Some(chain)
        }
        Err(e) => {
            report.unreadable(screen, chain_name, e);
            None
        }
    }
}

fn check_window_inside_leaf(
    report: &mut CheckReport,
    screen: &str,
    leaf: &ChainCertificate,
    window: &KdmWindowTimes,
) {
    let Ok((_, certificate)) = X509Certificate::from_der(&leaf.der) else {
        return;
    };
    let validity = certificate.validity();
    let inside = validity.not_before.timestamp() <= window.start.timestamp()
        && window.end.timestamp() <= validity.not_after.timestamp();
    if !inside {
        report.failures.push(CheckFinding {
            screen: screen.to_string(),
            chain: RECIPIENT_CHAIN_NAME.to_string(),
            certificate: leaf.label.clone(),
            check: CertificateCheck::WindowInsideRecipient,
            detail: format!(
                "the window {} to {} is not inside the certificate's {} to {}",
                window.not_valid_before,
                window.not_valid_after,
                validity.not_before,
                validity.not_after
            ),
        });
    }
}

// the recipient and device chains at one time, before any window is known
pub fn check_screen_certificates(
    screen_label: &str,
    screen: &Screen,
    at: DateTime<Utc>,
) -> CheckReport {
    check_chains(screen_label, screen, at).0
}

fn check_chains(
    screen_label: &str,
    screen: &Screen,
    at: DateTime<Utc>,
) -> (CheckReport, Option<ChainCertificate>) {
    let mut report = CheckReport::default();
    let desired_times = [at];
    let recipient_pem = match screen.cert.pem() {
        Ok(pem) => pem,
        Err(e) => {
            report.unreadable(screen_label, RECIPIENT_CHAIN_NAME, e);
            return (report, None);
        }
    };
    let recipient_context = ChainContext {
        leaf_roles: LeafRoles::AnyOf(KDM_RECIPIENT_ROLES),
        desired_times: &desired_times,
        best_effort_rules: &[],
    };
    let leaf = check_pem_chain(
        &mut report,
        screen_label,
        RECIPIENT_CHAIN_NAME,
        &recipient_pem,
        &recipient_context,
    )
    .and_then(|chain| chain.into_iter().next());

    let device_context = ChainContext {
        leaf_roles: LeafRoles::AnyOf(AUTHORIZED_DEVICE_ROLES),
        desired_times: &desired_times,
        best_effort_rules: AUTHORIZED_DEVICE_BEST_EFFORT_RULES,
    };
    for device in &screen.authorized_devices {
        let chain_name = match &device.serial {
            Some(serial) => format!("authorized device {} {serial}", device.device_type),
            None => format!("authorized device {}", device.device_type),
        };
        check_pem_chain(
            &mut report,
            screen_label,
            &chain_name,
            &device.certificate,
            &device_context,
        );
    }
    (report, leaf)
}

pub fn check_screen(
    screen_label: &str,
    screen: &Screen,
    window: &KdmWindowTimes,
    issue_date: DateTime<Utc>,
) -> CheckReport {
    let (mut report, leaf) = check_chains(screen_label, screen, issue_date);
    if let Some(leaf) = leaf {
        check_window_inside_leaf(&mut report, screen_label, &leaf, window);
    }
    report
}

// ISDCF Doc 5 Annex A: the KDM signing chain is checked at IssueDate, here also across the window
pub fn check_signer(
    signer_chain_files: &[PathBuf],
    window: &KdmWindowTimes,
    issue_date: DateTime<Utc>,
) -> CheckReport {
    let mut report = CheckReport::default();
    let chain = match chain_from_files(signer_chain_files) {
        Ok(chain) => chain,
        Err(e) => {
            report.unreadable(SIGNER_CHAIN_NAME, SIGNER_CHAIN_NAME, e);
            return report;
        }
    };
    let desired_times = [issue_date, window.start, window.end];
    let context = ChainContext {
        leaf_roles: LeafRoles::ZeroOrMore,
        desired_times: &desired_times,
        best_effort_rules: &[],
    };
    report.add_chain(
        SIGNER_CHAIN_NAME,
        SIGNER_CHAIN_NAME,
        check_chain(&chain, &context),
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdm_distribution::cinema::{AuthorizedDevice, CertSource, cinema_from_flm};
    use crate::kdm_distribution::flm::parse_flm;
    use crate::kdm_distribution::test_support::{fixtures, read};
    use crate::kdm_distribution::window::{LocalWindow, kdm_window_in_time_zone};

    const SCREEN: &str = "Rex / 1";

    fn window_in_days(start: i64, end: i64) -> KdmWindowTimes {
        let today = Utc::now().date_naive();
        let at = |days: i64| {
            (today + chrono::Duration::days(days))
                .and_hms_opt(18, 0, 0)
                .unwrap()
        };
        kdm_window_in_time_zone(
            &LocalWindow {
                start: at(start),
                end: at(end),
            },
            "Europe/London",
        )
        .unwrap()
    }

    fn screen_with(recipient: &std::path::Path, devices: &[(&str, &std::path::Path)]) -> Screen {
        let f = fixtures();
        let mut screen =
            Screen::new("1", CertSource::Inline(f.vendor_chain_pem(recipient))).unwrap();
        screen.authorized_devices = devices
            .iter()
            .map(|(device_type, leaf)| AuthorizedDevice {
                device_type: device_type.to_string(),
                serial: None,
                certificate: f.vendor_chain_pem(leaf),
            })
            .collect();
        screen
    }

    fn checks(report: &CheckReport) -> Vec<CertificateCheck> {
        report
            .failures
            .iter()
            .map(|finding| finding.check)
            .collect()
    }

    #[test]
    fn a_suite_of_conforming_chains_passes_and_names_what_it_did_not_check() {
        let f = fixtures();
        let screen = screen_with(
            &f.security_managers[0].certificate,
            &[
                ("LD", &f.link_decryptor.certificate),
                ("PR", &f.projector.certificate),
            ],
        );
        let report = check_screen(SCREEN, &screen, &window_in_days(2, 9), Utc::now());
        assert!(report.passed(), "{:#?}", report.failures);
        assert!(
            report
                .not_checked
                .iter()
                .any(|note| note.contains("rule 12")),
            "{:?}",
            report.not_checked
        );
    }

    #[test]
    fn a_recipient_without_the_sm_role_is_named_with_its_screen_certificate_and_rule() {
        let f = fixtures();
        let screen = screen_with(&f.link_decryptor.certificate, &[]);
        let report = check_screen(SCREEN, &screen, &window_in_days(2, 9), Utc::now());
        let failure = &report.failures[0];
        assert_eq!(failure.check, CertificateCheck::Rule(ChainRule::Role));
        let text = failure.to_string();
        assert!(
            text.starts_with("Rex / 1: recipient certificate "),
            "{text}"
        );
        assert!(text.contains("LD.Vendor.LDB.2001"), "{text}");
        assert!(text.contains("ST 430-2 rule 8 (role)"), "{text}");
    }

    #[test]
    fn a_device_without_the_ld_or_pr_role_fails_rule_8_in_its_own_chain() {
        let f = fixtures();
        let screen = screen_with(
            &f.security_managers[0].certificate,
            &[("LD", &f.security_managers[1].certificate)],
        );
        let report = check_screen(SCREEN, &screen, &window_in_days(2, 9), Utc::now());
        assert_eq!(
            checks(&report),
            vec![CertificateCheck::Rule(ChainRule::Role)]
        );
        assert_eq!(report.failures[0].chain, "authorized device LD");
    }

    #[test]
    fn the_certificate_status_needs_no_window() {
        let f = fixtures();
        let good = screen_with(
            &f.security_managers[0].certificate,
            &[("LD", &f.link_decryptor.certificate)],
        );
        assert!(check_screen_certificates(SCREEN, &good, Utc::now()).passed());
        let wrong_role = screen_with(&f.projector.certificate, &[]);
        let report = check_screen_certificates(SCREEN, &wrong_role, Utc::now());
        assert_eq!(
            checks(&report),
            vec![CertificateCheck::Rule(ChainRule::Role)]
        );
    }

    #[test]
    fn a_window_past_the_recipient_expiry_is_refused() {
        let f = fixtures();
        let screen = screen_with(&f.security_managers[0].certificate, &[]);
        let report = check_screen(SCREEN, &screen, &window_in_days(2, 20 * 365), Utc::now());
        assert_eq!(
            checks(&report),
            vec![CertificateCheck::WindowInsideRecipient]
        );
    }

    #[test]
    fn a_recipient_given_without_its_chain_does_not_reach_a_root() {
        let f = fixtures();
        let screen = Screen::new(
            "1",
            CertSource::Path(f.security_managers[0].certificate.clone()),
        )
        .unwrap();
        let report = check_screen(SCREEN, &screen, &window_in_days(2, 9), Utc::now());
        assert!(
            checks(&report).contains(&CertificateCheck::Rule(ChainRule::Issuer)),
            "{:#?}",
            report.failures
        );
    }

    #[test]
    fn the_smpte_example_suite_fails_on_its_cut_intermediate_its_role_and_its_expiry() {
        let xml = read(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/flm/st430-16b-2017.xml"
        )));
        let cinema = cinema_from_flm(&parse_flm(&xml).unwrap()).unwrap();
        let report = check_screen(
            SCREEN,
            &cinema.screens[0],
            &window_in_days(2, 9),
            Utc::now(),
        );
        let failed = checks(&report);
        for check in [
            CertificateCheck::Rule(ChainRule::DerEncoding),
            CertificateCheck::Rule(ChainRule::Role),
            CertificateCheck::Rule(ChainRule::DesiredTime),
            CertificateCheck::WindowInsideRecipient,
        ] {
            assert!(
                failed.contains(&check),
                "{check} missing from {:#?}",
                report.failures
            );
        }
    }

    #[test]
    fn the_signer_chain_has_to_cover_issue_date_and_window() {
        let f = fixtures();
        let mut chain = vec![f.distributor_signer.clone()];
        chain.extend(f.distributor_chain.iter().cloned());
        assert!(check_signer(&chain, &window_in_days(2, 9), Utc::now()).passed());
        let too_long = check_signer(&chain, &window_in_days(2, 20 * 365), Utc::now());
        assert_eq!(
            checks(&too_long),
            vec![CertificateCheck::Rule(ChainRule::DesiredTime); 3]
        );
        assert_eq!(too_long.failures[0].screen, "signer chain");
    }
}
