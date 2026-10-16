use super::{Verdict, refusal};
use aes::Aes128;
use cmac::{Cmac, KeyInit, Mac};
use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::{OsRng, RngCore};
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, RsaPublicKey};
use sha2::Sha512;
use std::fmt;
use std::time::Duration;
use x509_parser::certificate::X509Certificate;
use x509_parser::der_parser::asn1_rs::{Any, Class, FromDer, Tag};

pub const RANDOM_NUMBER_SIZE: usize = 16;
const SESSION_KEY_SIZE: usize = 16;
const OMAC_SIZE: usize = 16;
const GUID_SIZE: usize = 16;
const ULONG_SIZE: usize = 4;
const PARAMETERS_SIZE: usize = 4056;
const REQUESTED_INFORMATION_DATA_SIZE: usize = 4076;
pub const ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE: usize = 256;
const INITIALIZATION_PARAMETERS_SIZE: usize =
    RANDOM_NUMBER_SIZE + SESSION_KEY_SIZE + 2 * ULONG_SIZE;
pub const GET_INFO_PARAMETERS_SIZE: usize =
    OMAC_SIZE + RANDOM_NUMBER_SIZE + GUID_SIZE + 2 * ULONG_SIZE + PARAMETERS_SIZE;
pub const CONFIGURE_PARAMETERS_SIZE: usize =
    OMAC_SIZE + GUID_SIZE + 2 * ULONG_SIZE + PARAMETERS_SIZE;
pub const REQUESTED_INFORMATION_SIZE: usize =
    OMAC_SIZE + ULONG_SIZE + REQUESTED_INFORMATION_DATA_SIZE;
const STANDARD_INFORMATION_SIZE: usize = RANDOM_NUMBER_SIZE + 4 * ULONG_SIZE;

pub const GET_CONNECTOR_TYPE: u128 = 0x81d0bfd5_6afe_48c2_99c0_95a08f97c5da;
pub const GET_VIRTUAL_PROTECTION_LEVEL: u128 = 0xb2075857_3eda_4d5d_88db_748f8c1a0549;
pub const SET_PROTECTION_LEVEL: u128 = 0x9bb9327c_4eb5_4727_9f00_b42b0919c0da;
pub const PROTECTION_TYPE_HDCP: u32 = 8;
pub const HDCP_OFF: u32 = 0;
pub const HDCP_ON: u32 = 1;
pub const STATUS_NORMAL: u32 = 0;
pub const STATUS_LINK_LOST: u32 = 1;
pub const STATUS_RENEGOTIATION_REQUIRED: u32 = 2;
pub const STATUS_TAMPERING_DETECTED: u32 = 4;
pub const STATUS_REVOKED_HDCP_DEVICE_ATTACHED: u32 = 8;
const STATUS_FLAG_NAMES: [(u32, &str); 4] = [
    (STATUS_LINK_LOST, "link lost"),
    (STATUS_RENEGOTIATION_REQUIRED, "renegotiation required"),
    (STATUS_TAMPERING_DETECTED, "tampering detected"),
    (
        STATUS_REVOKED_HDCP_DEVICE_ATTACHED,
        "revoked HDCP device attached",
    ),
];
pub const CONNECTOR_TYPE_LVDS: u32 = 6;
pub const CONNECTOR_TYPE_DISPLAYPORT_EXTERNAL: u32 = 10;
pub const CONNECTOR_TYPE_DISPLAYPORT_EMBEDDED: u32 = 11;
pub const CONNECTOR_TYPE_UDI_EMBEDDED: u32 = 13;
// Microsoft says never to ask these internal connectors for HDCP
const INTERNAL_CONNECTORS: [(u32, &str); 3] = [
    (CONNECTOR_TYPE_LVDS, "LVDS"),
    (CONNECTOR_TYPE_DISPLAYPORT_EMBEDDED, "embedded DisplayPort"),
    (CONNECTOR_TYPE_UDI_EMBEDDED, "embedded UDI"),
];
pub const COPP_COMPATIBLE_CONNECTOR_TYPE_INTERNAL: u32 = 0x8000_0000;
// HDCP usually takes 100 to 200 ms to come on
const HDCP_ENABLE_ATTEMPTS: usize = 10;
const HDCP_ENABLE_WAIT: Duration = Duration::from_millis(100);
// PKCS #7 signedData, 1.2.840.113549.1.7.2
const SIGNED_DATA_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];

pub type RandomNumber = [u8; RANDOM_NUMBER_SIZE];

// the in-memory layout of a Windows GUID
pub fn guid_bytes(guid: u128) -> [u8; GUID_SIZE] {
    let mut bytes = [0; GUID_SIZE];
    bytes[..4].copy_from_slice(&((guid >> 96) as u32).to_le_bytes());
    bytes[4..6].copy_from_slice(&((guid >> 80) as u16).to_le_bytes());
    bytes[6..8].copy_from_slice(&((guid >> 64) as u16).to_le_bytes());
    bytes[8..].copy_from_slice(&(guid as u64).to_be_bytes());
    bytes
}

fn omac_signer(key: &[u8; SESSION_KEY_SIZE]) -> Cmac<Aes128> {
    <Cmac<Aes128> as KeyInit>::new(&(*key).into())
}

// OPM's OMAC-1 is AES-128 CMAC
pub fn omac1(key: &[u8; SESSION_KEY_SIZE], data: &[u8]) -> [u8; OMAC_SIZE] {
    let mut signer = omac_signer(key);
    signer.update(data);
    signer.finalize().into_bytes().into()
}

pub fn omac1_matches(key: &[u8; SESSION_KEY_SIZE], data: &[u8], omac: &[u8]) -> bool {
    let mut signer = omac_signer(key);
    signer.update(data);
    signer.verify_slice(omac).is_ok()
}

fn random_bytes<const SIZE: usize>() -> Result<[u8; SIZE], String> {
    let mut bytes = [0; SIZE];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| format!("the system random number generator failed: {error}"))?;
    Ok(bytes)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut word = [0; ULONG_SIZE];
    word.copy_from_slice(&bytes[offset..offset + ULONG_SIZE]);
    u32::from_le_bytes(word)
}

fn der_element(bytes: &[u8]) -> Result<(&[u8], Any<'_>), String> {
    Any::from_der(bytes).map_err(|error| format!("the certificate chain is not DER: {error}"))
}

// the chain comes either as certificates back to back or as a PKCS #7 holding them
fn chain_certificates(chain: &[u8]) -> Result<&[u8], String> {
    let (_, outer) = der_element(chain)?;
    let (after_content_type, content_type) = der_element(outer.data)?;
    if content_type.header.tag() != Tag::Oid {
        return Ok(chain);
    }
    if content_type.data != SIGNED_DATA_OID {
        return Err("the certificate chain is a PKCS #7 without signed data".to_string());
    }
    let (_, explicit_content) = der_element(after_content_type)?;
    let (_, signed_data) = der_element(explicit_content.data)?;
    let mut remaining = signed_data.data;
    while !remaining.is_empty() {
        let (next, element) = der_element(remaining)?;
        if element.header.class() == Class::ContextSpecific && element.header.tag() == Tag(0) {
            return Ok(element.data);
        }
        remaining = next;
    }
    Err("the certificate chain is a PKCS #7 without certificates".to_string())
}

// the driver's key is in the leaf, the one certificate that signs no other
pub fn driver_public_key(chain: &[u8]) -> Result<RsaPublicKey, String> {
    let mut remaining = chain_certificates(chain)?;
    let mut certificates = Vec::new();
    while !remaining.is_empty() {
        let (next, certificate) = X509Certificate::from_der(remaining)
            .map_err(|error| format!("the certificate chain does not parse: {error}"))?;
        certificates.push(certificate);
        remaining = next;
    }
    let leaves: Vec<&X509Certificate> = certificates
        .iter()
        .enumerate()
        .filter(|(index, certificate)| {
            !certificates.iter().enumerate().any(|(other_index, other)| {
                other_index != *index && other.issuer().as_raw() == certificate.subject().as_raw()
            })
        })
        .map(|(_, certificate)| certificate)
        .collect();
    let [leaf] = leaves.as_slice() else {
        return Err(format!(
            "the certificate chain has {} leaf certificates, not one",
            leaves.len()
        ));
    };
    let key = RsaPublicKey::from_public_key_der(leaf.public_key().raw)
        .map_err(|error| format!("the driver certificate holds no RSA key: {error}"))?;
    if key.size() != ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE {
        return Err(format!(
            "the driver key is {} bits, OPM uses {}",
            key.size() * 8,
            ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE * 8
        ));
    }
    Ok(key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInformation {
    pub random: RandomNumber,
    pub status_flags: u32,
    pub information: u32,
}

impl StandardInformation {
    pub fn parse(bytes: &[u8]) -> Result<StandardInformation, String> {
        if bytes.len() != STANDARD_INFORMATION_SIZE {
            return Err(format!(
                "the status answer is {} bytes, not {STANDARD_INFORMATION_SIZE}",
                bytes.len()
            ));
        }
        let mut random = [0; RANDOM_NUMBER_SIZE];
        random.copy_from_slice(&bytes[..RANDOM_NUMBER_SIZE]);
        Ok(StandardInformation {
            random,
            status_flags: read_u32(bytes, RANDOM_NUMBER_SIZE),
            information: read_u32(bytes, RANDOM_NUMBER_SIZE + ULONG_SIZE),
        })
    }
}

// the application side of one OPM session: the AES key that signs both ways and the two sequence numbers
pub struct Session {
    key: [u8; SESSION_KEY_SIZE],
    status_sequence: u32,
    command_sequence: u32,
}

impl Session {
    pub fn new() -> Result<Session, String> {
        Ok(Session {
            key: random_bytes()?,
            status_sequence: u32::from_le_bytes(random_bytes()?),
            command_sequence: u32::from_le_bytes(random_bytes()?),
        })
    }

    fn initialization_parameters(
        &self,
        driver_random: &RandomNumber,
    ) -> [u8; INITIALIZATION_PARAMETERS_SIZE] {
        let mut parameters = [0; INITIALIZATION_PARAMETERS_SIZE];
        let (random, rest) = parameters.split_at_mut(RANDOM_NUMBER_SIZE);
        random.copy_from_slice(driver_random);
        let (key, sequences) = rest.split_at_mut(SESSION_KEY_SIZE);
        key.copy_from_slice(&self.key);
        sequences[..ULONG_SIZE].copy_from_slice(&self.status_sequence.to_le_bytes());
        sequences[ULONG_SIZE..].copy_from_slice(&self.command_sequence.to_le_bytes());
        parameters
    }

    // RSAES-OAEP with SHA-512, as Microsoft's example encrypts it
    pub fn encrypted_initialization_parameters(
        &self,
        driver_key: &RsaPublicKey,
        driver_random: &RandomNumber,
    ) -> Result<[u8; ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE], String> {
        let encrypted = driver_key
            .encrypt(
                &mut OsRng,
                Oaep::new::<Sha512>(),
                &self.initialization_parameters(driver_random),
            )
            .map_err(|error| format!("the session key does not encrypt: {error}"))?;
        encrypted.try_into().map_err(|encrypted: Vec<u8>| {
            format!(
                "the encrypted session key is {} bytes, not {ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE}",
                encrypted.len()
            )
        })
    }

    pub fn status_request(
        &mut self,
        random: &RandomNumber,
        information: u128,
        parameters: &[u8],
    ) -> [u8; GET_INFO_PARAMETERS_SIZE] {
        let mut request = [0; GET_INFO_PARAMETERS_SIZE];
        let mut offset = OMAC_SIZE;
        for field in [
            random.as_slice(),
            &guid_bytes(information),
            &self.status_sequence.to_le_bytes(),
            &(parameters.len() as u32).to_le_bytes(),
            parameters,
        ] {
            request[offset..offset + field.len()].copy_from_slice(field);
            offset += field.len();
        }
        let omac = omac1(&self.key, &request[OMAC_SIZE..]);
        request[..OMAC_SIZE].copy_from_slice(&omac);
        self.status_sequence = self.status_sequence.wrapping_add(1);
        request
    }

    pub fn command(&mut self, setting: u128, parameters: &[u8]) -> [u8; CONFIGURE_PARAMETERS_SIZE] {
        let mut command = [0; CONFIGURE_PARAMETERS_SIZE];
        let mut offset = OMAC_SIZE;
        for field in [
            guid_bytes(setting).as_slice(),
            &self.command_sequence.to_le_bytes(),
            &(parameters.len() as u32).to_le_bytes(),
            parameters,
        ] {
            command[offset..offset + field.len()].copy_from_slice(field);
            offset += field.len();
        }
        let omac = omac1(&self.key, &command[OMAC_SIZE..]);
        command[..OMAC_SIZE].copy_from_slice(&omac);
        self.command_sequence = self.command_sequence.wrapping_add(1);
        command
    }

    // the driver signs everything after the omac, the size field included
    pub fn verified_information<'a>(
        &self,
        response: &'a [u8; REQUESTED_INFORMATION_SIZE],
    ) -> Result<&'a [u8], String> {
        let (omac, signed) = response.split_at(OMAC_SIZE);
        if !omac1_matches(&self.key, signed, omac) {
            return Err("the driver's signature on the answer does not match".to_string());
        }
        let size = read_u32(signed, 0) as usize;
        let information = &signed[ULONG_SIZE..];
        information.get(..size).ok_or_else(|| {
            format!(
                "the answer claims {size} bytes, more than OPM's {REQUESTED_INFORMATION_DATA_SIZE}"
            )
        })
    }
}

// one physical output of a monitor as IOPMVideoOutput sees it, so the protocol runs against a fake driver in tests
pub trait VideoOutput {
    fn start_initialization(&self) -> Result<(RandomNumber, Vec<u8>), String>;
    fn finish_initialization(
        &self,
        parameters: &[u8; ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE],
    ) -> Result<(), String>;
    fn get_information(
        &self,
        request: &[u8; GET_INFO_PARAMETERS_SIZE],
    ) -> Result<[u8; REQUESTED_INFORMATION_SIZE], String>;
    fn configure(&self, command: &[u8; CONFIGURE_PARAMETERS_SIZE]) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    OpenOutputs,
    Certificate,
    KeyExchange,
    ConnectorType,
    HdcpCommand,
    HdcpStatus,
}

impl fmt::Display for Step {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Step::OpenOutputs => "opening its OPM outputs",
            Step::Certificate => "reading the driver certificate",
            Step::KeyExchange => "the OPM session key exchange",
            Step::ConnectorType => "the connector type request",
            Step::HdcpCommand => "the request to turn HDCP on",
            Step::HdcpStatus => "the HDCP status request",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputState {
    HdcpOn,
    HdcpOff { level: u32 },
    Abnormal { status_flags: u32 },
    Internal { connector: &'static str },
    Failed { step: Step, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputReading {
    // the monitor and the output's place on it, such as \\.\DISPLAY1 output 1
    pub output: String,
    pub state: OutputState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorReading {
    NotFound { monitor: Option<String> },
    // every output of the player window's monitor, all must read HDCP on
    Outputs(Vec<OutputReading>),
}

fn status_flag_names(status_flags: u32) -> String {
    let known = STATUS_FLAG_NAMES
        .iter()
        .fold(0, |known, (flag, _)| known | flag);
    let mut names: Vec<String> = STATUS_FLAG_NAMES
        .iter()
        .filter(|(flag, _)| status_flags & flag != 0)
        .map(|(_, name)| name.to_string())
        .collect();
    if status_flags & !known != 0 {
        names.push(format!("status flags {:#x}", status_flags & !known));
    }
    names.join(", ")
}

// unencrypted content and the setting off never open OPM
pub fn verdict(
    require_hdcp: bool,
    encrypted: bool,
    read_outputs: impl FnOnce() -> MonitorReading,
) -> Verdict {
    if !require_hdcp || !encrypted {
        return Verdict::Play;
    }
    let outputs = match read_outputs() {
        MonitorReading::NotFound { monitor } => {
            let monitor = monitor.map_or("the player window's monitor".to_string(), |name| {
                format!("monitor {name}")
            });
            return refusal(format!(
                "no display output matches {monitor}, so it counts as not protected"
            ));
        }
        MonitorReading::Outputs(outputs) => outputs,
    };
    let Some(OutputReading { output, state }) = outputs
        .into_iter()
        .find(|output| output.state != OutputState::HdcpOn)
    else {
        return Verdict::Play;
    };
    refusal(match state {
        OutputState::HdcpOn => unreachable!("the search skips outputs with HDCP on"),
        OutputState::HdcpOff { level: HDCP_OFF } => format!("{output} reads HDCP off"),
        OutputState::HdcpOff { level } => format!("{output} reads HDCP level {level}"),
        OutputState::Abnormal { status_flags } => {
            format!("{output} reports {}", status_flag_names(status_flags))
        }
        OutputState::Internal { connector } => format!(
            "{output} is an internal {connector} panel, which takes no HDCP, so it counts as not protected"
        ),
        OutputState::Failed { step, error } => format!("{output} failed {step}: {error}"),
    })
}

fn failed(step: Step) -> impl FnOnce(String) -> OutputState {
    move |error| OutputState::Failed { step, error }
}

pub struct OutputSession<V> {
    output: V,
    session: Session,
    hdcp_requested: bool,
}

impl<V: VideoOutput> OutputSession<V> {
    pub fn open(output: V) -> Result<OutputSession<V>, OutputState> {
        let (driver_random, certificate_chain) = output
            .start_initialization()
            .map_err(failed(Step::Certificate))?;
        let driver_key =
            driver_public_key(&certificate_chain).map_err(failed(Step::Certificate))?;
        let session = Session::new().map_err(failed(Step::KeyExchange))?;
        let parameters = session
            .encrypted_initialization_parameters(&driver_key, &driver_random)
            .map_err(failed(Step::KeyExchange))?;
        output
            .finish_initialization(&parameters)
            .map_err(failed(Step::KeyExchange))?;
        Ok(OutputSession {
            output,
            session,
            hdcp_requested: false,
        })
    }

    // the information of a signed status answer whose random number and status flags check out
    fn status(
        &mut self,
        information: u128,
        parameters: &[u8],
        step: Step,
    ) -> Result<u32, OutputState> {
        let random: RandomNumber = random_bytes().map_err(failed(step))?;
        let request = self
            .session
            .status_request(&random, information, parameters);
        let response = self
            .output
            .get_information(&request)
            .map_err(failed(step))?;
        let answer = self
            .session
            .verified_information(&response)
            .and_then(StandardInformation::parse)
            .map_err(failed(step))?;
        if answer.random != random {
            return Err(failed(step)(
                "the answer carries a different random number than the request".to_string(),
            ));
        }
        if answer.status_flags != STATUS_NORMAL {
            return Err(OutputState::Abnormal {
                status_flags: answer.status_flags,
            });
        }
        Ok(answer.information)
    }

    fn hdcp_level(&mut self) -> Result<u32, OutputState> {
        self.status(
            GET_VIRTUAL_PROTECTION_LEVEL,
            &PROTECTION_TYPE_HDCP.to_le_bytes(),
            Step::HdcpStatus,
        )
    }

    fn turn_hdcp_on(&mut self) -> Result<u32, OutputState> {
        let mut parameters = [0; 4 * ULONG_SIZE];
        parameters[..ULONG_SIZE].copy_from_slice(&PROTECTION_TYPE_HDCP.to_le_bytes());
        parameters[ULONG_SIZE..2 * ULONG_SIZE].copy_from_slice(&HDCP_ON.to_le_bytes());
        let mut level = HDCP_OFF;
        for attempt in 0..HDCP_ENABLE_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(HDCP_ENABLE_WAIT);
            }
            let command = self.session.command(SET_PROTECTION_LEVEL, &parameters);
            self.output
                .configure(&command)
                .map_err(failed(Step::HdcpCommand))?;
            level = self.hdcp_level()?;
            if level == HDCP_ON {
                break;
            }
        }
        Ok(level)
    }

    fn read_state(&mut self) -> Result<u32, OutputState> {
        if self.hdcp_requested {
            return self.hdcp_level();
        }
        let connector = self.status(GET_CONNECTOR_TYPE, &[], Step::ConnectorType)?
            & !COPP_COMPATIBLE_CONNECTOR_TYPE_INTERNAL;
        if let Some((_, name)) = INTERNAL_CONNECTORS
            .iter()
            .find(|(internal, _)| *internal == connector)
        {
            return Err(OutputState::Internal { connector: name });
        }
        let level = self.turn_hdcp_on()?;
        self.hdcp_requested = true;
        Ok(level)
    }

    // the first read turns HDCP on, later reads only check it stayed on
    pub fn read(&mut self) -> OutputState {
        match self.read_state() {
            Ok(HDCP_ON) => OutputState::HdcpOn,
            Ok(level) => OutputState::HdcpOff { level },
            Err(state) => state,
        }
    }
}

// the OPM sessions of the monitor last checked, kept while every output reads HDCP on
pub struct MonitorSessions<M, V> {
    monitor: Option<M>,
    outputs: Vec<(String, OutputSession<V>)>,
}

impl<M, V> Default for MonitorSessions<M, V> {
    fn default() -> Self {
        MonitorSessions {
            monitor: None,
            outputs: Vec::new(),
        }
    }
}

impl<M: PartialEq, V: VideoOutput> MonitorSessions<M, V> {
    fn close(&mut self) {
        self.monitor = None;
        self.outputs.clear();
    }

    fn open(
        &mut self,
        monitor: M,
        monitor_name: &str,
        open_outputs: impl FnOnce() -> Result<Vec<V>, String>,
    ) -> Result<(), MonitorReading> {
        let outputs = open_outputs().map_err(|error| {
            MonitorReading::Outputs(vec![OutputReading {
                output: monitor_name.to_string(),
                state: OutputState::Failed {
                    step: Step::OpenOutputs,
                    error,
                },
            }])
        })?;
        if outputs.is_empty() {
            return Err(MonitorReading::NotFound {
                monitor: Some(monitor_name.to_string()),
            });
        }
        let mut failures = Vec::new();
        for (index, output) in outputs.into_iter().enumerate() {
            let name = format!("{monitor_name} output {}", index + 1);
            match OutputSession::open(output) {
                Ok(session) => self.outputs.push((name, session)),
                Err(state) => failures.push(OutputReading {
                    output: name,
                    state,
                }),
            }
        }
        if !failures.is_empty() {
            self.close();
            return Err(MonitorReading::Outputs(failures));
        }
        self.monitor = Some(monitor);
        Ok(())
    }

    pub fn read(
        &mut self,
        monitor: M,
        monitor_name: &str,
        open_outputs: impl FnOnce() -> Result<Vec<V>, String>,
    ) -> MonitorReading {
        if self.monitor.as_ref() != Some(&monitor) {
            self.close();
            if let Err(reading) = self.open(monitor, monitor_name, open_outputs) {
                return reading;
            }
        }
        let readings: Vec<OutputReading> = self
            .outputs
            .iter_mut()
            .map(|(name, session)| OutputReading {
                output: name.clone(),
                state: session.read(),
            })
            .collect();
        if readings
            .iter()
            .any(|reading| reading.state != OutputState::HdcpOn)
        {
            self.close();
        }
        MonitorReading::Outputs(readings)
    }
}

#[cfg(windows)]
pub use windows_outputs::OpmSessions;

#[cfg(windows)]
mod windows_outputs {
    use super::*;
    use windows::Win32::Graphics::Gdi::HMONITOR;
    use windows::Win32::Media::MediaFoundation::{
        IOPMVideoOutput, OPM_CONFIGURE_PARAMETERS, OPM_ENCRYPTED_INITIALIZATION_PARAMETERS,
        OPM_GET_CONNECTOR_TYPE, OPM_GET_INFO_PARAMETERS, OPM_GET_VIRTUAL_PROTECTION_LEVEL,
        OPM_HDCP_OFF, OPM_HDCP_ON, OPM_PROTECTION_TYPE_HDCP, OPM_RANDOM_NUMBER,
        OPM_REQUESTED_INFORMATION, OPM_SET_PROTECTION_LEVEL, OPM_STATUS_LINK_LOST,
        OPM_STATUS_NORMAL, OPM_STATUS_RENEGOTIATION_REQUIRED,
        OPM_STATUS_REVOKED_HDCP_DEVICE_ATTACHED, OPM_STATUS_TAMPERING_DETECTED,
        OPM_VOS_OPM_SEMANTICS, OPMGetVideoOutputsFromHMONITOR,
    };
    use windows::Win32::System::Com::CoTaskMemFree;

    const _: () = assert!(OPM_GET_CONNECTOR_TYPE.to_u128() == GET_CONNECTOR_TYPE);
    const _: () =
        assert!(OPM_GET_VIRTUAL_PROTECTION_LEVEL.to_u128() == GET_VIRTUAL_PROTECTION_LEVEL);
    const _: () = assert!(OPM_SET_PROTECTION_LEVEL.to_u128() == SET_PROTECTION_LEVEL);
    const _: () = assert!(OPM_PROTECTION_TYPE_HDCP.0 as u32 == PROTECTION_TYPE_HDCP);
    const _: () = assert!(OPM_HDCP_OFF.0 as u32 == HDCP_OFF);
    const _: () = assert!(OPM_HDCP_ON.0 as u32 == HDCP_ON);
    const _: () = assert!(OPM_STATUS_NORMAL.0 as u32 == STATUS_NORMAL);
    const _: () = assert!(OPM_STATUS_LINK_LOST.0 as u32 == STATUS_LINK_LOST);
    const _: () =
        assert!(OPM_STATUS_RENEGOTIATION_REQUIRED.0 as u32 == STATUS_RENEGOTIATION_REQUIRED);
    const _: () = assert!(OPM_STATUS_TAMPERING_DETECTED.0 as u32 == STATUS_TAMPERING_DETECTED);
    const _: () = assert!(
        OPM_STATUS_REVOKED_HDCP_DEVICE_ATTACHED.0 as u32 == STATUS_REVOKED_HDCP_DEVICE_ATTACHED
    );

    fn hresult_text(error: windows::core::Error) -> String {
        error.to_string()
    }

    pub struct WindowsVideoOutput(IOPMVideoOutput);

    impl VideoOutput for WindowsVideoOutput {
        fn start_initialization(&self) -> Result<(RandomNumber, Vec<u8>), String> {
            let mut random = OPM_RANDOM_NUMBER {
                abRandomNumber: [0; RANDOM_NUMBER_SIZE],
            };
            let mut certificate: *mut u8 = std::ptr::null_mut();
            let mut certificate_length = 0u32;
            // SAFETY: every out pointer points at a live local
            unsafe {
                self.0
                    .StartInitialization(&mut random, &mut certificate, &mut certificate_length)
            }
            .map_err(hresult_text)?;
            if certificate.is_null() {
                return Err("the driver returned no certificate".to_string());
            }
            // SAFETY: the driver allocated certificate_length bytes at certificate, which we own and free here
            let chain = unsafe {
                let chain =
                    std::slice::from_raw_parts(certificate, certificate_length as usize).to_vec();
                CoTaskMemFree(Some(certificate as *const core::ffi::c_void));
                chain
            };
            Ok((random.abRandomNumber, chain))
        }

        fn finish_initialization(
            &self,
            parameters: &[u8; ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE],
        ) -> Result<(), String> {
            let parameters = OPM_ENCRYPTED_INITIALIZATION_PARAMETERS {
                abEncryptedInitializationParameters: *parameters,
            };
            // SAFETY: the parameters outlive the call
            unsafe { self.0.FinishInitialization(&parameters) }.map_err(hresult_text)
        }

        fn get_information(
            &self,
            request: &[u8; GET_INFO_PARAMETERS_SIZE],
        ) -> Result<[u8; REQUESTED_INFORMATION_SIZE], String> {
            // SAFETY: a packed struct of plain integers the same size as the bytes, any bit pattern is valid
            let request = unsafe {
                std::mem::transmute::<[u8; GET_INFO_PARAMETERS_SIZE], OPM_GET_INFO_PARAMETERS>(
                    *request,
                )
            };
            let mut response = OPM_REQUESTED_INFORMATION::default();
            // SAFETY: both structs outlive the call
            unsafe { self.0.GetInformation(&request, &mut response) }.map_err(hresult_text)?;
            // SAFETY: as above, the other way round
            Ok(unsafe {
                std::mem::transmute::<OPM_REQUESTED_INFORMATION, [u8; REQUESTED_INFORMATION_SIZE]>(
                    response,
                )
            })
        }

        fn configure(&self, command: &[u8; CONFIGURE_PARAMETERS_SIZE]) -> Result<(), String> {
            // SAFETY: a packed struct of plain integers the same size as the bytes, any bit pattern is valid
            let command = unsafe {
                std::mem::transmute::<[u8; CONFIGURE_PARAMETERS_SIZE], OPM_CONFIGURE_PARAMETERS>(
                    *command,
                )
            };
            // SAFETY: the command outlives the call
            unsafe { self.0.Configure(&command, None) }.map_err(hresult_text)
        }
    }

    fn video_outputs(monitor: HMONITOR) -> Result<Vec<WindowsVideoOutput>, String> {
        let mut count = 0u32;
        let mut array: *mut Option<IOPMVideoOutput> = std::ptr::null_mut();
        // SAFETY: both out pointers point at live locals
        unsafe {
            OPMGetVideoOutputsFromHMONITOR(monitor, OPM_VOS_OPM_SEMANTICS, &mut count, &mut array)
        }
        .map_err(hresult_text)?;
        if array.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: the array holds count interface pointers, each moved out once, then the array is freed
        let outputs = unsafe {
            let outputs = (0..count as usize)
                .filter_map(|index| array.add(index).read())
                .map(WindowsVideoOutput)
                .collect();
            CoTaskMemFree(Some(array as *const core::ffi::c_void));
            outputs
        };
        Ok(outputs)
    }

    #[derive(Default)]
    pub struct OpmSessions(MonitorSessions<isize, WindowsVideoOutput>);

    // SAFETY: dxva2 hands out the OPM objects directly, outside any COM apartment, and callers keep them behind a Mutex
    unsafe impl Send for OpmSessions {}

    impl OpmSessions {
        pub fn read(&mut self, monitor: HMONITOR, monitor_name: &str) -> MonitorReading {
            self.0
                .read(monitor.0 as isize, monitor_name, || video_outputs(monitor))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_byte_layouts_match_the_windows_structs() {
            let mut session = Session::new().unwrap();
            let random = [7; RANDOM_NUMBER_SIZE];
            let request = session.status_request(
                &random,
                GET_VIRTUAL_PROTECTION_LEVEL,
                &PROTECTION_TYPE_HDCP.to_le_bytes(),
            );
            let request = unsafe {
                std::mem::transmute::<[u8; GET_INFO_PARAMETERS_SIZE], OPM_GET_INFO_PARAMETERS>(
                    request,
                )
            };
            let information = request.guidInformation;
            let parameters_size = request.cbParametersSize;
            let parameters = request.abParameters;
            let request_random = request.rnRandomNumber;
            assert_eq!(information, OPM_GET_VIRTUAL_PROTECTION_LEVEL);
            assert_eq!(parameters_size, ULONG_SIZE as u32);
            assert_eq!(parameters[..ULONG_SIZE], PROTECTION_TYPE_HDCP.to_le_bytes());
            assert_eq!(request_random.abRandomNumber, random);

            let command = session.command(SET_PROTECTION_LEVEL, &[1, 2, 3, 4]);
            let command = unsafe {
                std::mem::transmute::<[u8; CONFIGURE_PARAMETERS_SIZE], OPM_CONFIGURE_PARAMETERS>(
                    command,
                )
            };
            let setting = command.guidSetting;
            let command_size = command.cbParametersSize;
            assert_eq!(setting, OPM_SET_PROTECTION_LEVEL);
            assert_eq!(command_size, 4);

            let response = OPM_REQUESTED_INFORMATION {
                cbRequestedInformationSize: STANDARD_INFORMATION_SIZE as u32,
                ..Default::default()
            };
            let bytes = unsafe {
                std::mem::transmute::<OPM_REQUESTED_INFORMATION, [u8; REQUESTED_INFORMATION_SIZE]>(
                    response,
                )
            };
            assert_eq!(
                read_u32(&bytes, OMAC_SIZE),
                STANDARD_INFORMATION_SIZE as u32
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use std::cell::{Cell, RefCell};

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn omac1_matches_the_rfc_4493_aes_cmac_vectors() {
        let key: [u8; 16] = hex("2b7e1516 28aed2a6 abf71588 09cf4f3c")
            .try_into()
            .unwrap();
        let message = hex(
            "6bc1bee2 2e409f96 e93d7e11 7393172a ae2d8a57 1e03ac9c 9eb76fac 45af8e51
             30c81c46 a35ce411 e5fbc119 1a0a52ef f69f2445 df4f9b17 ad2b417b e66c3710",
        );
        for (length, expected) in [
            (0, "bb1d6929 e9593728 7fa37d12 9b756746"),
            (16, "070a16b4 6b4d4144 f79bdd9d d04a287c"),
            (40, "dfa66747 de9ae630 30ca3261 1497c827"),
            (64, "51f0bebf 7e3b9d92 fc497417 79363cfe"),
        ] {
            assert_eq!(
                omac1(&key, &message[..length]).as_slice(),
                hex(expected),
                "{length} byte message"
            );
            assert!(omac1_matches(&key, &message[..length], &hex(expected)));
        }
        let mut wrong = hex("51f0bebf 7e3b9d92 fc497417 79363cfe");
        wrong[0] ^= 1;
        assert!(!omac1_matches(&key, &message, &wrong));
    }

    #[test]
    fn a_guid_lays_out_as_windows_stores_it() {
        assert_eq!(
            guid_bytes(GET_CONNECTOR_TYPE),
            hex("d5bfd081 fe6a c248 99c0 95a08f97c5da").as_slice()
        );
    }

    fn standard_information_bytes(
        random: &RandomNumber,
        status_flags: u32,
        information: u32,
    ) -> Vec<u8> {
        let mut bytes = random.to_vec();
        for word in [status_flags, information, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn standard_information_parses_from_its_32_bytes() {
        let random = [9; RANDOM_NUMBER_SIZE];
        let bytes = standard_information_bytes(&random, STATUS_LINK_LOST, HDCP_ON);
        assert_eq!(
            StandardInformation::parse(&bytes),
            Ok(StandardInformation {
                random,
                status_flags: STATUS_LINK_LOST,
                information: HDCP_ON,
            })
        );
        assert_eq!(
            StandardInformation::parse(&bytes[..31]),
            Err("the status answer is 31 bytes, not 32".to_string())
        );
    }

    fn test_session() -> Session {
        Session {
            key: [3; SESSION_KEY_SIZE],
            status_sequence: u32::MAX,
            command_sequence: 41,
        }
    }

    #[test]
    fn a_status_request_is_signed_and_numbered() {
        let mut session = test_session();
        let random = [5; RANDOM_NUMBER_SIZE];
        let request = session.status_request(&random, GET_VIRTUAL_PROTECTION_LEVEL, &[8, 0, 0, 0]);
        assert_eq!(
            request[..OMAC_SIZE],
            omac1(&session.key, &request[OMAC_SIZE..])
        );
        assert_eq!(request[16..32], random);
        assert_eq!(request[32..48], guid_bytes(GET_VIRTUAL_PROTECTION_LEVEL));
        assert_eq!(read_u32(&request, 48), u32::MAX);
        assert_eq!(read_u32(&request, 52), 4);
        assert_eq!(request[56..60], [8, 0, 0, 0]);
        assert!(request[60..].iter().all(|&byte| byte == 0));
        let next = session.status_request(&random, GET_VIRTUAL_PROTECTION_LEVEL, &[]);
        assert_eq!(read_u32(&next, 48), 0, "the sequence number wraps");

        let command = session.command(SET_PROTECTION_LEVEL, &[1, 2]);
        assert_eq!(
            command[..OMAC_SIZE],
            omac1(&session.key, &command[OMAC_SIZE..])
        );
        assert_eq!(command[16..32], guid_bytes(SET_PROTECTION_LEVEL));
        assert_eq!(read_u32(&command, 32), 41);
        assert_eq!(read_u32(&command, 36), 2);
        assert_eq!(command[40..42], [1, 2]);
        assert_eq!(
            read_u32(&session.command(SET_PROTECTION_LEVEL, &[]), 32),
            42
        );
    }

    fn signed_response(
        key: &[u8; SESSION_KEY_SIZE],
        information: &[u8],
    ) -> [u8; REQUESTED_INFORMATION_SIZE] {
        let mut response = [0; REQUESTED_INFORMATION_SIZE];
        response[OMAC_SIZE..OMAC_SIZE + ULONG_SIZE]
            .copy_from_slice(&(information.len() as u32).to_le_bytes());
        response[OMAC_SIZE + ULONG_SIZE..OMAC_SIZE + ULONG_SIZE + information.len()]
            .copy_from_slice(information);
        let omac = omac1(key, &response[OMAC_SIZE..]);
        response[..OMAC_SIZE].copy_from_slice(&omac);
        response
    }

    #[test]
    fn an_answer_is_trusted_only_with_the_session_signature() {
        let session = test_session();
        let information = standard_information_bytes(&[1; RANDOM_NUMBER_SIZE], 0, HDCP_ON);
        let response = signed_response(&session.key, &information);
        assert_eq!(
            session.verified_information(&response),
            Ok(information.as_slice())
        );

        let mut tampered = response;
        tampered[OMAC_SIZE + ULONG_SIZE + RANDOM_NUMBER_SIZE + ULONG_SIZE] = HDCP_OFF as u8;
        assert_eq!(
            session.verified_information(&tampered),
            Err("the driver's signature on the answer does not match".to_string())
        );

        let mut oversized = [0; REQUESTED_INFORMATION_SIZE];
        oversized[OMAC_SIZE..OMAC_SIZE + ULONG_SIZE].copy_from_slice(&u32::MAX.to_le_bytes());
        let omac = omac1(&session.key, &oversized[OMAC_SIZE..]);
        oversized[..OMAC_SIZE].copy_from_slice(&omac);
        assert!(session.verified_information(&oversized).is_err());
    }

    struct DriverKeys {
        private_key: RsaPrivateKey,
        // a self-signed root that issued the leaf, which holds the driver key
        root: Vec<u8>,
        leaf: Vec<u8>,
    }

    fn driver_keys() -> DriverKeys {
        let private_key = RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        let pem = private_key.to_pkcs8_pem(LineEnding::LF).unwrap();
        let leaf_key = rcgen::KeyPair::from_pem(&pem).unwrap();
        let root_key = rcgen::KeyPair::generate().unwrap();
        let mut root_parameters = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        root_parameters
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test OPM root");
        root_parameters.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let root = root_parameters.self_signed(&root_key).unwrap();
        let mut leaf_parameters = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        leaf_parameters
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test OPM driver");
        let leaf = leaf_parameters
            .signed_by(&leaf_key, &root, &root_key)
            .unwrap();
        DriverKeys {
            private_key,
            root: root.der().to_vec(),
            leaf: leaf.der().to_vec(),
        }
    }

    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut element = vec![tag];
        match content.len() {
            length @ 0..=0x7f => element.push(length as u8),
            length @ 0x80..=0xff => element.extend([0x81, length as u8]),
            length => element.extend([0x82, (length >> 8) as u8, length as u8]),
        }
        element.extend_from_slice(content);
        element
    }

    // a certificates-only PKCS #7, the shape a .p7b file has
    fn pkcs7(certificates: &[&[u8]]) -> Vec<u8> {
        const SEQUENCE: u8 = 0x30;
        const SET: u8 = 0x31;
        const INTEGER: u8 = 0x02;
        const OID: u8 = 0x06;
        const CONTEXT_CONSTRUCTED_0: u8 = 0xa0;
        const DATA_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
        let signed_data = [
            der(INTEGER, &[1]),
            der(SET, &[]),
            der(SEQUENCE, &der(OID, DATA_OID)),
            der(CONTEXT_CONSTRUCTED_0, &certificates.concat()),
            der(SET, &[]),
        ]
        .concat();
        der(
            SEQUENCE,
            &[
                der(OID, SIGNED_DATA_OID),
                der(CONTEXT_CONSTRUCTED_0, &der(SEQUENCE, &signed_data)),
            ]
            .concat(),
        )
    }

    #[test]
    fn the_driver_key_comes_from_the_leaf_in_either_chain_encoding() {
        let keys = driver_keys();
        let public_key = keys.private_key.to_public_key();
        for chain in [
            [keys.root.as_slice(), &keys.leaf].concat(),
            [keys.leaf.as_slice(), &keys.root].concat(),
            keys.leaf.clone(),
            pkcs7(&[&keys.root, &keys.leaf]),
        ] {
            assert_eq!(driver_public_key(&chain), Ok(public_key.clone()));
        }
        let root_alone = driver_public_key(&keys.root).unwrap_err();
        assert!(
            root_alone.starts_with("the driver certificate holds no RSA key"),
            "{root_alone}"
        );
        assert!(driver_public_key(&[0x30, 0x03, 0x02, 0x01]).is_err());
    }

    // a driver that keeps OPM's side of the protocol, checking every signature it is sent
    struct FakeDriver {
        keys: DriverKeys,
        driver_random: RandomNumber,
        session_key: Cell<Option<[u8; SESSION_KEY_SIZE]>>,
        status_sequence: Cell<u32>,
        command_sequence: Cell<u32>,
        connector: u32,
        // HDCP reads on once this many commands asked for it
        commands_until_on: u32,
        commands: Cell<u32>,
        status_flags: u32,
        answer_with_wrong_key: bool,
        log: RefCell<Vec<&'static str>>,
    }

    impl FakeDriver {
        fn new(keys: DriverKeys) -> FakeDriver {
            FakeDriver {
                keys,
                driver_random: [0x5a; RANDOM_NUMBER_SIZE],
                session_key: Cell::new(None),
                status_sequence: Cell::new(0),
                command_sequence: Cell::new(0),
                connector: CONNECTOR_TYPE_DISPLAYPORT_EXTERNAL,
                commands_until_on: 1,
                commands: Cell::new(0),
                status_flags: STATUS_NORMAL,
                answer_with_wrong_key: false,
                log: RefCell::new(Vec::new()),
            }
        }

        fn key(&self) -> [u8; SESSION_KEY_SIZE] {
            self.session_key
                .get()
                .expect("a request before the key exchange")
        }
    }

    impl VideoOutput for &FakeDriver {
        fn start_initialization(&self) -> Result<(RandomNumber, Vec<u8>), String> {
            self.log.borrow_mut().push("start");
            Ok((
                self.driver_random,
                [self.keys.root.as_slice(), &self.keys.leaf].concat(),
            ))
        }

        fn finish_initialization(
            &self,
            parameters: &[u8; ENCRYPTED_INITIALIZATION_PARAMETERS_SIZE],
        ) -> Result<(), String> {
            let plain = self
                .keys
                .private_key
                .decrypt(Oaep::new::<Sha512>(), parameters)
                .map_err(|error| error.to_string())?;
            assert_eq!(plain.len(), INITIALIZATION_PARAMETERS_SIZE);
            assert_eq!(plain[..RANDOM_NUMBER_SIZE], self.driver_random);
            self.session_key
                .set(Some(plain[16..32].try_into().unwrap()));
            self.status_sequence.set(read_u32(&plain, 32));
            self.command_sequence.set(read_u32(&plain, 36));
            Ok(())
        }

        fn get_information(
            &self,
            request: &[u8; GET_INFO_PARAMETERS_SIZE],
        ) -> Result<[u8; REQUESTED_INFORMATION_SIZE], String> {
            let key = self.key();
            assert!(omac1_matches(
                &key,
                &request[OMAC_SIZE..],
                &request[..OMAC_SIZE]
            ));
            assert_eq!(read_u32(request, 48), self.status_sequence.get());
            self.status_sequence
                .set(self.status_sequence.get().wrapping_add(1));
            let random: RandomNumber = request[16..32].try_into().unwrap();
            let information = if request[32..48] == guid_bytes(GET_CONNECTOR_TYPE) {
                self.log.borrow_mut().push("connector");
                self.connector
            } else {
                assert_eq!(request[32..48], guid_bytes(GET_VIRTUAL_PROTECTION_LEVEL));
                assert_eq!(read_u32(request, 56), PROTECTION_TYPE_HDCP);
                self.log.borrow_mut().push("level");
                if self.commands.get() >= self.commands_until_on {
                    HDCP_ON
                } else {
                    HDCP_OFF
                }
            };
            let signing_key = if self.answer_with_wrong_key {
                [0; SESSION_KEY_SIZE]
            } else {
                key
            };
            Ok(signed_response(
                &signing_key,
                &standard_information_bytes(&random, self.status_flags, information),
            ))
        }

        fn configure(&self, command: &[u8; CONFIGURE_PARAMETERS_SIZE]) -> Result<(), String> {
            let key = self.key();
            assert!(omac1_matches(
                &key,
                &command[OMAC_SIZE..],
                &command[..OMAC_SIZE]
            ));
            assert_eq!(command[16..32], guid_bytes(SET_PROTECTION_LEVEL));
            assert_eq!(read_u32(command, 32), self.command_sequence.get());
            assert_eq!(read_u32(command, 40), PROTECTION_TYPE_HDCP);
            assert_eq!(read_u32(command, 44), HDCP_ON);
            self.command_sequence
                .set(self.command_sequence.get().wrapping_add(1));
            self.commands.set(self.commands.get() + 1);
            self.log.borrow_mut().push("configure");
            Ok(())
        }
    }

    fn first_read(driver: &FakeDriver) -> OutputState {
        OutputSession::open(driver).map_or_else(|state| state, |mut session| session.read())
    }

    #[test]
    fn a_session_turns_hdcp_on_then_only_checks_it() {
        let mut driver = FakeDriver::new(driver_keys());
        driver.commands_until_on = 2;
        let mut session = OutputSession::open(&driver).unwrap();
        assert_eq!(session.read(), OutputState::HdcpOn);
        assert_eq!(session.read(), OutputState::HdcpOn);
        assert_eq!(
            *driver.log.borrow(),
            [
                "start",
                "connector",
                "configure",
                "level",
                "configure",
                "level",
                "level"
            ]
        );
    }

    #[test]
    fn an_output_that_never_turns_hdcp_on_reads_off() {
        let mut driver = FakeDriver::new(driver_keys());
        driver.commands_until_on = u32::MAX;
        assert_eq!(
            first_read(&driver),
            OutputState::HdcpOff { level: HDCP_OFF }
        );
        assert_eq!(driver.commands.get(), HDCP_ENABLE_ATTEMPTS as u32);
    }

    #[test]
    fn an_internal_panel_is_never_asked_for_hdcp() {
        let mut driver = FakeDriver::new(driver_keys());
        driver.connector =
            CONNECTOR_TYPE_DISPLAYPORT_EMBEDDED | COPP_COMPATIBLE_CONNECTOR_TYPE_INTERNAL;
        assert_eq!(
            first_read(&driver),
            OutputState::Internal {
                connector: "embedded DisplayPort"
            }
        );
        assert_eq!(driver.commands.get(), 0);
    }

    #[test]
    fn an_unsigned_answer_or_abnormal_status_is_not_trusted() {
        let mut driver = FakeDriver::new(driver_keys());
        driver.answer_with_wrong_key = true;
        assert_eq!(
            first_read(&driver),
            OutputState::Failed {
                step: Step::ConnectorType,
                error: "the driver's signature on the answer does not match".to_string()
            }
        );
        driver.answer_with_wrong_key = false;
        driver.status_flags = STATUS_LINK_LOST | STATUS_REVOKED_HDCP_DEVICE_ATTACHED;
        assert_eq!(
            first_read(&driver),
            OutputState::Abnormal {
                status_flags: STATUS_LINK_LOST | STATUS_REVOKED_HDCP_DEVICE_ATTACHED
            }
        );
    }

    #[test]
    fn sessions_stay_open_for_one_monitor_and_reopen_for_another() {
        let driver = FakeDriver::new(driver_keys());
        let mut sessions = MonitorSessions::default();
        let opened = Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            Ok(vec![&driver])
        };
        let on = MonitorReading::Outputs(vec![OutputReading {
            output: "DISPLAY1 output 1".to_string(),
            state: OutputState::HdcpOn,
        }]);
        assert_eq!(sessions.read(1, "DISPLAY1", open), on);
        assert_eq!(sessions.read(1, "DISPLAY1", open), on);
        assert_eq!(opened.get(), 1);
        assert_eq!(
            sessions.read(2, "DISPLAY2", open),
            MonitorReading::Outputs(vec![OutputReading {
                output: "DISPLAY2 output 1".to_string(),
                state: OutputState::HdcpOn,
            }])
        );
        assert_eq!(opened.get(), 2);
        assert_eq!(
            sessions.read(3, "DISPLAY3", || Ok(Vec::new())),
            MonitorReading::NotFound {
                monitor: Some("DISPLAY3".to_string())
            }
        );
        assert_eq!(
            sessions.read(3, "DISPLAY3", || Err("E_ACCESSDENIED".to_string())),
            MonitorReading::Outputs(vec![OutputReading {
                output: "DISPLAY3".to_string(),
                state: OutputState::Failed {
                    step: Step::OpenOutputs,
                    error: "E_ACCESSDENIED".to_string()
                },
            }])
        );
    }

    fn output(name: &str, state: OutputState) -> OutputReading {
        OutputReading {
            output: name.to_string(),
            state,
        }
    }

    fn unread() -> MonitorReading {
        panic!("OPM was opened when the verdict did not need it")
    }

    #[test]
    fn unencrypted_content_and_the_setting_off_play_without_opening_opm() {
        assert_eq!(verdict(true, false, unread), Verdict::Play);
        assert_eq!(verdict(false, true, unread), Verdict::Play);
    }

    #[test]
    fn encrypted_content_plays_only_where_every_output_reads_hdcp_on() {
        let refused = |reading: MonitorReading| match verdict(true, true, || reading) {
            Verdict::Refuse(message) => message,
            Verdict::Play => "plays".to_string(),
        };
        assert_eq!(
            verdict(true, true, || MonitorReading::Outputs(vec![
                output("DISPLAY1 output 1", OutputState::HdcpOn),
                output("DISPLAY1 output 2", OutputState::HdcpOn),
            ])),
            Verdict::Play
        );
        assert_eq!(
            refused(MonitorReading::Outputs(vec![
                output("DISPLAY1 output 1", OutputState::HdcpOn),
                output(
                    "DISPLAY1 output 2",
                    OutputState::HdcpOff { level: HDCP_OFF }
                ),
            ])),
            "HDCP is required for encrypted content, and DISPLAY1 output 2 reads HDCP off"
        );
        assert_eq!(
            refused(MonitorReading::NotFound {
                monitor: Some("DISPLAY1".to_string())
            }),
            "HDCP is required for encrypted content, and no display output matches monitor DISPLAY1, so it counts as not protected"
        );
        assert_eq!(
            refused(MonitorReading::Outputs(vec![output(
                "DISPLAY1 output 1",
                OutputState::Failed {
                    step: Step::KeyExchange,
                    error: "ERROR_GRAPHICS_OPM_INVALID_ENCRYPTED_PARAMETERS".to_string()
                }
            )])),
            "HDCP is required for encrypted content, and DISPLAY1 output 1 failed the OPM session key exchange: ERROR_GRAPHICS_OPM_INVALID_ENCRYPTED_PARAMETERS"
        );
        assert_eq!(
            refused(MonitorReading::Outputs(vec![output(
                "DISPLAY1 output 1",
                OutputState::Abnormal {
                    status_flags: STATUS_LINK_LOST | 0x100
                }
            )])),
            "HDCP is required for encrypted content, and DISPLAY1 output 1 reports link lost, status flags 0x100"
        );
        assert_eq!(
            refused(MonitorReading::Outputs(vec![output(
                "DISPLAY1 output 1",
                OutputState::Internal {
                    connector: "embedded DisplayPort"
                }
            )])),
            "HDCP is required for encrypted content, and DISPLAY1 output 1 is an internal embedded DisplayPort panel, which takes no HDCP, so it counts as not protected"
        );
    }
}
