pub mod drm_outputs;
pub mod opm_outputs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Play,
    Refuse(String),
}

fn refusal(reason: String) -> Verdict {
    Verdict::Refuse(format!(
        "HDCP is required for encrypted content, and {reason}"
    ))
}
