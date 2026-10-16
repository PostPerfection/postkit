use super::{Verdict, refusal};

const NOTHING_TURNS_HDCP_ON: &str = "On GNOME or KDE nothing turns HDCP on";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ContentProtection {
    Undesired,
    Desired,
    Enabled,
}

impl ContentProtection {
    pub fn from_property_name(name: &str) -> Option<ContentProtection> {
        match name {
            "Undesired" => Some(ContentProtection::Undesired),
            "Desired" => Some(ContentProtection::Desired),
            "Enabled" => Some(ContentProtection::Enabled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputProtection {
    // the connector name, such as DP-9
    pub output: String,
    // None when the driver has no Content Protection property, nvidia-drm among them
    pub protection: Option<ContentProtection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputReading {
    NotFound { monitor: Option<String> },
    // every connector the player window's monitor could be, all must be Enabled
    Outputs(Vec<OutputProtection>),
}

// unencrypted content and the setting off never read the output
pub fn verdict(
    require_hdcp: bool,
    encrypted: bool,
    read_output: impl FnOnce() -> OutputReading,
) -> Verdict {
    if !require_hdcp || !encrypted {
        return Verdict::Play;
    }
    let outputs = match read_output() {
        OutputReading::NotFound { monitor } => {
            let monitor = monitor.map_or("the player window's monitor".to_string(), |name| {
                format!("monitor {name}")
            });
            return refusal(format!(
                "no display output matches {monitor}, so it counts as not protected"
            ));
        }
        OutputReading::Outputs(outputs) => outputs,
    };
    let unprotected = outputs
        .iter()
        .find(|output| output.protection != Some(ContentProtection::Enabled));
    match unprotected {
        None => Verdict::Play,
        Some(OutputProtection {
            output,
            protection: Some(state),
        }) => refusal(format!(
            "{output} reads Content Protection {state:?}. {NOTHING_TURNS_HDCP_ON}."
        )),
        Some(OutputProtection {
            output,
            protection: None,
        }) => refusal(format!(
            "{output} has no Content Protection property, so it counts as not protected"
        )),
    }
}

const EDID_BLOCK_LENGTH: usize = 128;
const EDID_DESCRIPTOR_OFFSETS: [usize; 4] = [54, 72, 90, 108];
const EDID_DESCRIPTOR_LENGTH: usize = 18;
const EDID_DESCRIPTOR_TEXT_START: usize = 5;
const EDID_PRODUCT_NAME_TAG: u8 = 0xFC;
const EDID_PRODUCT_CODE_OFFSET: usize = 10;
const EDID_TEXT_END: u8 = b'\n';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    // card1 for /dev/dri/card1
    pub card: String,
    // DP-9 for card1-DP-9
    pub name: String,
    pub id: u32,
    pub connected: bool,
    pub edid: Vec<u8>,
}

// what mutter and so GDK call the monitor: the EDID product name, or the product code in hex
pub fn edid_model_name(edid: &[u8]) -> Option<String> {
    if edid.len() < EDID_BLOCK_LENGTH {
        return None;
    }
    let product_name = EDID_DESCRIPTOR_OFFSETS.iter().find_map(|&offset| {
        let descriptor = &edid[offset..offset + EDID_DESCRIPTOR_LENGTH];
        let is_product_name =
            descriptor[..3] == [0, 0, 0] && descriptor[3] == EDID_PRODUCT_NAME_TAG;
        is_product_name.then(|| {
            let text = &descriptor[EDID_DESCRIPTOR_TEXT_START..];
            let end = text
                .iter()
                .position(|&byte| byte == EDID_TEXT_END)
                .unwrap_or(text.len());
            String::from_utf8_lossy(&text[..end]).trim().to_string()
        })
    });
    let product_code = u16::from_le_bytes([
        edid[EDID_PRODUCT_CODE_OFFSET],
        edid[EDID_PRODUCT_CODE_OFFSET + 1],
    ]);
    Some(product_name.unwrap_or_else(|| format!("0x{product_code:04x}")))
}

// GDK names a monitor by its connector on X11 and by its EDID model on Wayland
pub fn matching_connectors<'a>(monitor: &str, connectors: &'a [Connector]) -> Vec<&'a Connector> {
    connectors
        .iter()
        .filter(|connector| connector.connected)
        .filter(|connector| {
            connector.name == monitor
                || edid_model_name(&connector.edid).as_deref() == Some(monitor)
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub use device::{connectors, content_protection, reading_for_monitor};

#[cfg(target_os = "linux")]
mod device {
    use super::{
        Connector, ContentProtection, OutputProtection, OutputReading, matching_connectors,
    };
    use drm::control::{Device as ControlDevice, connector, property};
    use std::os::fd::{AsFd, BorrowedFd};
    use std::path::{Path, PathBuf};

    const SYSFS_DRM: &str = "/sys/class/drm";
    const DEVICE_DIRECTORY: &str = "/dev/dri";
    const CARD_PREFIX: &str = "card";
    const CONNECTED_STATUS: &str = "connected";
    const CONTENT_PROTECTION_PROPERTY: &[u8] = b"Content Protection";

    struct Card(std::fs::File);

    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    impl drm::Device for Card {}
    impl ControlDevice for Card {}

    fn read_trimmed(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| text.trim().to_string())
    }

    pub fn connectors() -> Result<Vec<Connector>, String> {
        let entries =
            std::fs::read_dir(SYSFS_DRM).map_err(|error| format!("{SYSFS_DRM}: {error}"))?;
        let mut connectors = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some((card, name)) = file_name.split_once('-') else {
                continue;
            };
            if !card.starts_with(CARD_PREFIX) {
                continue;
            }
            let directory = entry.path();
            let Some(id) =
                read_trimmed(&directory.join("connector_id")).and_then(|text| text.parse().ok())
            else {
                continue;
            };
            connectors.push(Connector {
                card: card.to_string(),
                name: name.to_string(),
                id,
                connected: read_trimmed(&directory.join("status")).as_deref()
                    == Some(CONNECTED_STATUS),
                edid: std::fs::read(directory.join("edid")).unwrap_or_default(),
            });
        }
        connectors
            .sort_by(|first, second| (&first.card, &first.name).cmp(&(&second.card, &second.name)));
        Ok(connectors)
    }

    pub fn content_protection(connector: &Connector) -> Result<Option<ContentProtection>, String> {
        let device: PathBuf = Path::new(DEVICE_DIRECTORY).join(&connector.card);
        let failed = |error: std::io::Error| format!("{}: {error}", device.display());
        let card = Card(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&device)
                .map_err(failed)?,
        );
        let handle: connector::Handle = drm::control::from_u32(connector.id)
            .ok_or_else(|| format!("{} has connector id 0", connector.name))?;
        let properties = card.get_properties(handle).map_err(failed)?;
        for (property_handle, raw_value) in properties.iter() {
            let info = card.get_property(*property_handle).map_err(failed)?;
            if info.name().to_bytes() != CONTENT_PROTECTION_PROPERTY {
                continue;
            }
            let property::ValueType::Enum(values) = info.value_type() else {
                return Err(format!(
                    "{} has a Content Protection property that is not an enum",
                    connector.name
                ));
            };
            let name = values
                .get_value_from_raw_value(*raw_value)
                .map(|value| value.name().to_string_lossy().into_owned())
                .unwrap_or_default();
            return ContentProtection::from_property_name(&name)
                .map(Some)
                .ok_or_else(|| format!("{} reads Content Protection {name:?}", connector.name));
        }
        Ok(None)
    }

    pub fn reading_for_monitor(monitor: &str) -> OutputReading {
        let connectors = match connectors() {
            Ok(connectors) => connectors,
            Err(error) => {
                eprintln!("[hdcp] {error}");
                return OutputReading::NotFound {
                    monitor: Some(monitor.to_string()),
                };
            }
        };
        let matching = matching_connectors(monitor, &connectors);
        if matching.is_empty() {
            return OutputReading::NotFound {
                monitor: Some(monitor.to_string()),
            };
        }
        let outputs = matching
            .into_iter()
            .map(|connector| OutputProtection {
                output: connector.name.clone(),
                protection: content_protection(connector).unwrap_or_else(|error| {
                    eprintln!("[hdcp] {error}");
                    None
                }),
            })
            .collect();
        OutputReading::Outputs(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, protection: Option<ContentProtection>) -> OutputProtection {
        OutputProtection {
            output: name.to_string(),
            protection,
        }
    }

    fn unread() -> OutputReading {
        panic!("the output was read when the verdict did not need it")
    }

    #[test]
    fn unencrypted_content_and_the_setting_off_play_without_reading_the_output() {
        assert_eq!(verdict(true, false, unread), Verdict::Play);
        assert_eq!(verdict(false, true, unread), Verdict::Play);
    }

    #[test]
    fn encrypted_content_plays_only_where_every_matching_output_reads_enabled() {
        let enabled = || {
            OutputReading::Outputs(vec![
                output("DP-1", Some(ContentProtection::Enabled)),
                output("DP-2", Some(ContentProtection::Enabled)),
            ])
        };
        assert_eq!(verdict(true, true, enabled), Verdict::Play);

        let one_desired = || {
            OutputReading::Outputs(vec![
                output("DP-1", Some(ContentProtection::Enabled)),
                output("DP-2", Some(ContentProtection::Desired)),
            ])
        };
        assert_eq!(
        verdict(true, true, one_desired),
        Verdict::Refuse(
            "HDCP is required for encrypted content, and DP-2 reads Content Protection Desired. On GNOME or KDE nothing turns HDCP on."
                .to_string()
        )
    );
    }

    #[test]
    fn an_output_without_the_property_or_one_not_found_counts_as_not_protected() {
        assert_eq!(
        verdict(true, true, || OutputReading::Outputs(vec![output("HDMI-A-1", None)])),
        Verdict::Refuse(
            "HDCP is required for encrypted content, and HDMI-A-1 has no Content Protection property, so it counts as not protected"
                .to_string()
        )
    );
        assert_eq!(
        verdict(true, true, || OutputReading::NotFound {
            monitor: Some("LG Ultra HD".to_string())
        }),
        Verdict::Refuse(
            "HDCP is required for encrypted content, and no display output matches monitor LG Ultra HD, so it counts as not protected"
                .to_string()
        )
    );
    }

    mod drm_connector_tests {
        use super::super::*;

        const EDID_LENGTH: usize = 128;
        const FIRST_DESCRIPTOR: usize = 54;
        const SECOND_DESCRIPTOR: usize = 72;

        fn edid_with(product_code: u16, product_name: Option<&str>) -> Vec<u8> {
            let mut edid = vec![0u8; EDID_LENGTH];
            edid[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
            edid[10..12].copy_from_slice(&product_code.to_le_bytes());
            // a timing descriptor first, as real EDIDs have
            edid[FIRST_DESCRIPTOR] = 0x01;
            if let Some(name) = product_name {
                let descriptor = &mut edid[SECOND_DESCRIPTOR..SECOND_DESCRIPTOR + 18];
                descriptor[3] = 0xFC;
                let mut text = name.as_bytes().to_vec();
                text.push(b'\n');
                text.resize(13, b' ');
                descriptor[5..].copy_from_slice(&text);
            }
            edid
        }

        fn connector(name: &str, connected: bool, edid: Vec<u8>) -> Connector {
            Connector {
                card: "card1".to_string(),
                name: name.to_string(),
                id: 1,
                connected,
                edid,
            }
        }

        #[test]
        fn the_edid_model_is_the_product_name_or_the_product_code() {
            assert_eq!(
                edid_model_name(&edid_with(0x5b08, Some("LG Ultra HD"))).as_deref(),
                Some("LG Ultra HD")
            );
            assert_eq!(
                edid_model_name(&edid_with(0x0a9b, None)).as_deref(),
                Some("0x0a9b")
            );
            assert_eq!(edid_model_name(&[]), None);
        }

        #[test]
        fn a_monitor_matches_connected_connectors_by_connector_name_or_edid_model() {
            let connectors = [
                connector("eDP-1", true, edid_with(0x0a9b, None)),
                connector("DP-9", true, edid_with(0x5b08, Some("LG Ultra HD"))),
                connector("DP-10", false, edid_with(0x5b08, Some("LG Ultra HD"))),
            ];
            let names = |monitor: &str| -> Vec<String> {
                matching_connectors(monitor, &connectors)
                    .iter()
                    .map(|connector| connector.name.clone())
                    .collect()
            };

            assert_eq!(names("LG Ultra HD"), ["DP-9"]);
            assert_eq!(names("0x0a9b"), ["eDP-1"]);
            assert_eq!(names("DP-9"), ["DP-9"]);
            assert_eq!(names("Xvfb"), Vec::<String>::new());
        }

        #[cfg(target_os = "linux")]
        const SYSFS_DRM_DIRECTORY: &str = "/sys/class/drm";

        #[cfg(target_os = "linux")]
        fn card_driver(card: &str) -> Option<String> {
            let driver = std::fs::read_link(
                std::path::Path::new(SYSFS_DRM_DIRECTORY)
                    .join(card)
                    .join("device/driver"),
            )
            .ok()?;
            Some(driver.file_name()?.to_string_lossy().into_owned())
        }

        #[cfg(target_os = "linux")]
        const AMDGPU_DRIVER: &str = "amdgpu";
        // a writeback connector feeds memory, not a display, so it has no HDCP
        #[cfg(target_os = "linux")]
        const WRITEBACK_CONNECTOR_PREFIX: &str = "Writeback";

        #[cfg(target_os = "linux")]
        #[test]
        #[ignore = "GitHub runners have no /dev/dri, run with --ignored on a machine with an amdgpu card"]
        fn every_amdgpu_connector_reads_its_content_protection_as_this_user() {
            let connectors = connectors().expect("/sys/class/drm lists no connectors");
            let amdgpu: Vec<&Connector> = connectors
                .iter()
                .filter(|connector| card_driver(&connector.card).as_deref() == Some(AMDGPU_DRIVER))
                .collect();
            assert!(
                !amdgpu.is_empty(),
                "no amdgpu card under /sys/class/drm, this test needs one: {connectors:?}"
            );
            for connector in amdgpu
                .into_iter()
                .filter(|connector| !connector.name.starts_with(WRITEBACK_CONNECTOR_PREFIX))
            {
                let protection = content_protection(connector).unwrap_or_else(|error| {
                    panic!(
                        "{} on {} did not read: {error}",
                        connector.name, connector.card
                    )
                });
                assert!(
                    matches!(
                        protection,
                        Some(
                            ContentProtection::Undesired
                                | ContentProtection::Desired
                                | ContentProtection::Enabled
                        )
                    ),
                    "{} on {} has no Content Protection property",
                    connector.name,
                    connector.card
                );
            }
        }
    }
}
