use crate::certificate::KdmFormulation;
use serde::{Deserialize, Serialize};

// ISDCF Doc 5: an assume-trust KDM does not play on a suite with several remote or projector SPBs
const MULTIPLE_DEVICES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentStandard {
    Smpte,
    Interop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormulationChoice {
    pub formulation: KdmFormulation,
    pub warnings: Vec<String>,
}

// how one front end spells its formulation and device certificate flags in an error
#[derive(Debug, Clone, Copy)]
pub struct FormulationFlagNames {
    pub formulation: &'static str,
    pub device_certificate: &'static str,
}

pub fn default_formulation(authorized_device_count: usize) -> KdmFormulation {
    let assume_trust = KdmFormulation::default();
    if authorized_device_count > 0 {
        assume_trust.device_list_counterpart()
    } else {
        assume_trust
    }
}

// a choice that disagrees with the device list is refused, naming the flags to change
pub fn resolve_formulation(
    explicit: Option<KdmFormulation>,
    device_cert_count: usize,
    flags: FormulationFlagNames,
) -> Result<KdmFormulation, String> {
    let has_devices = device_cert_count > 0;
    let Some(formulation) = explicit else {
        return Ok(default_formulation(device_cert_count));
    };
    if formulation.lists_supplied_devices() == has_devices {
        return Ok(formulation);
    }
    let counterpart = formulation.device_list_counterpart();
    let FormulationFlagNames {
        formulation: formulation_flag,
        device_certificate: device_flag,
    } = flags;
    Err(if has_devices {
        format!(
            "{formulation_flag} {formulation} carries the assume-trust thumbprint instead of a \
             device list, so the {device_cert_count} {device_flag} certificate(s) would be \
             dropped: use {formulation_flag} {counterpart}, or drop {device_flag}"
        )
    } else {
        format!(
            "{formulation_flag} {formulation} lists the devices given by {device_flag}, but none \
             were given: pass {device_flag}, or use {formulation_flag} {counterpart}"
        )
    })
}

// the formulation one screen gets from its authorized devices, per ISDCF Doc 5
pub fn choose_formulation(
    requested: Option<KdmFormulation>,
    authorized_device_count: usize,
    standard: Option<ContentStandard>,
) -> Result<FormulationChoice, String> {
    let formulation = requested.unwrap_or_else(|| default_formulation(authorized_device_count));
    let mut warnings = Vec::new();

    if formulation.lists_supplied_devices() && authorized_device_count == 0 {
        return Err(format!(
            "{formulation} lists the screen's authorized devices, and the screen has none: use {}",
            formulation.device_list_counterpart()
        ));
    }
    if !formulation.lists_supplied_devices() && authorized_device_count >= MULTIPLE_DEVICES {
        warnings.push(format!(
            "{formulation} names no devices, and the screen has {authorized_device_count}: ISDCF \
             Doc 5 says it will not play on a suite with several SPBs, {} lists them",
            formulation.device_list_counterpart()
        ));
    }

    let is_dci = matches!(
        formulation,
        KdmFormulation::DciAny | KdmFormulation::DciSpecific
    );
    match (is_dci, standard) {
        (true, Some(ContentStandard::Interop)) => {
            return Err(format!(
                "{formulation} carries a ContentAuthenticator, which ISDCF Doc 5 allows for SMPTE \
                 content only, and this title is Interop"
            ));
        }
        (true, None) => warnings.push(format!(
            "{formulation} works for SMPTE content only, and this title's standard is not known"
        )),
        _ => {}
    }

    Ok(FormulationChoice {
        formulation,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND_LINE: FormulationFlagNames = FormulationFlagNames {
        formulation: "--formulation",
        device_certificate: "--device-cert",
    };

    #[test]
    fn a_suite_without_devices_gets_assume_trust_and_may_use_any_assume_trust_formulation() {
        let choice = choose_formulation(None, 0, Some(ContentStandard::Smpte)).unwrap();
        assert_eq!(choice.formulation, KdmFormulation::ModifiedTransitional1);
        assert!(choice.warnings.is_empty());
        let dci = choose_formulation(
            Some(KdmFormulation::DciAny),
            0,
            Some(ContentStandard::Smpte),
        )
        .unwrap();
        assert!(dci.warnings.is_empty());
        assert!(
            choose_formulation(
                Some(KdmFormulation::DciSpecific),
                0,
                Some(ContentStandard::Smpte)
            )
            .is_err(),
            "a device list cannot list nothing"
        );
    }

    #[test]
    fn a_suite_with_devices_lists_them_by_default() {
        let choice = choose_formulation(None, 3, Some(ContentStandard::Interop)).unwrap();
        assert_eq!(
            choice.formulation,
            KdmFormulation::MultipleModifiedTransitional1
        );
        assert!(choice.warnings.is_empty());
    }

    #[test]
    fn assume_trust_on_several_devices_is_warned_about() {
        let choice =
            choose_formulation(Some(KdmFormulation::ModifiedTransitional1), 2, None).unwrap();
        assert_eq!(choice.formulation, KdmFormulation::ModifiedTransitional1);
        assert_eq!(choice.warnings.len(), 1);
        assert!(
            choice.warnings[0].contains("several SPBs"),
            "{:?}",
            choice.warnings
        );
        let single =
            choose_formulation(Some(KdmFormulation::ModifiedTransitional1), 1, None).unwrap();
        assert!(
            single.warnings.is_empty(),
            "one SPB plays with assume trust"
        );
    }

    #[test]
    fn dci_formulations_are_for_smpte_content_only() {
        let error = choose_formulation(
            Some(KdmFormulation::DciSpecific),
            2,
            Some(ContentStandard::Interop),
        )
        .unwrap_err();
        assert!(error.contains("SMPTE content only"), "{error}");
        let unknown = choose_formulation(Some(KdmFormulation::DciSpecific), 2, None).unwrap();
        assert!(
            unknown.warnings[0].contains("not known"),
            "{:?}",
            unknown.warnings
        );
    }

    #[test]
    fn a_command_line_formulation_that_contradicts_its_devices_names_the_flags() {
        assert_eq!(
            resolve_formulation(None, 2, COMMAND_LINE).unwrap(),
            KdmFormulation::MultipleModifiedTransitional1
        );
        let dropped =
            resolve_formulation(Some(KdmFormulation::DciAny), 1, COMMAND_LINE).unwrap_err();
        assert_eq!(
            dropped,
            "--formulation dci-any carries the assume-trust thumbprint instead of a device list, \
             so the 1 --device-cert certificate(s) would be dropped: use --formulation \
             dci-specific, or drop --device-cert"
        );
        let missing =
            resolve_formulation(Some(KdmFormulation::DciSpecific), 0, COMMAND_LINE).unwrap_err();
        assert_eq!(
            missing,
            "--formulation dci-specific lists the devices given by --device-cert, but none were \
             given: pass --device-cert, or use --formulation dci-any"
        );
    }
}
