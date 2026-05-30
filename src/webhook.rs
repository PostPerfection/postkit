//! Webhook notification system for job lifecycle events.

use std::path::Path;
use std::process::Command;

use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum WebhookError {
    #[error("curl not available")]
    CurlNotFound,
    #[error("HTTP {0}")]
    HttpError(u16),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookConfig {
    pub url: String,
    pub secret: String,
    pub event_filter: String,
    pub timeout_seconds: u32,
    pub max_retries: u32,
    pub verify_ssl: bool,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            secret: String::new(),
            event_filter: String::new(),
            timeout_seconds: 10,
            max_retries: 3,
            verify_ssl: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookEvent {
    pub event_type: String,
    pub job_id: String,
    pub payload_json: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookResult {
    pub success: bool,
    pub http_status: u16,
    pub error: String,
    pub attempts: u32,
}

fn iso_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 10);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the JSON body for a webhook event.
pub fn build_event_body(event: &WebhookEvent) -> String {
    let timestamp = if event.timestamp.is_empty() {
        iso_timestamp()
    } else {
        event.timestamp.clone()
    };
    let payload = if event.payload_json.is_empty() {
        "{}".to_string()
    } else {
        event.payload_json.clone()
    };
    format!(
        r#"{{"type":"{}","job_id":"{}","timestamp":"{}","data":{}}}"#,
        escape_json_string(&event.event_type),
        escape_json_string(&event.job_id),
        escape_json_string(&timestamp),
        payload
    )
}

/// Send a webhook notification via curl.
pub fn send_webhook(config: &WebhookConfig, event: &WebhookEvent) -> WebhookResult {
    let json_body = build_event_body(event);
    let mut result = WebhookResult {
        success: false,
        http_status: 0,
        error: String::new(),
        attempts: 0,
    };

    for attempt in 0..=config.max_retries {
        result.attempts = attempt + 1;

        let mut cmd = Command::new("curl");
        cmd.args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST"]);
        cmd.args(["-H", "Content-Type: application/json"]);
        cmd.args(["-H", &format!("X-Webhook-Event: {}", event.event_type)]);

        if !config.secret.is_empty() {
            cmd.args(["-H", &format!("X-Webhook-Secret: {}", config.secret)]);
        }

        if !config.verify_ssl {
            cmd.arg("-k");
        }

        cmd.args(["--max-time", &config.timeout_seconds.to_string()]);
        cmd.args(["-d", &json_body]);
        cmd.arg(&config.url);

        match cmd.output() {
            Ok(output) => {
                let code_str = String::from_utf8_lossy(&output.stdout);
                if let Ok(status) = code_str.trim().parse::<u16>() {
                    result.http_status = status;
                    if (200..300).contains(&status) {
                        result.success = true;
                        return result;
                    }
                    result.error = format!("HTTP {status}");
                } else {
                    result.error = format!("curl returned: {code_str}");
                }
            }
            Err(e) => {
                result.error = format!("Failed to execute curl: {e}");
            }
        }
    }

    result
}

/// Build a JSON payload for a completed job.
pub fn build_job_completed_payload(
    job_id: &str,
    output_dir: &Path,
    duration_seconds: f64,
) -> String {
    format!(
        r#"{{"job_id":"{}","status":"completed","output_dir":"{}","duration_seconds":{}}}"#,
        escape_json_string(job_id),
        escape_json_string(&output_dir.display().to_string()),
        duration_seconds
    )
}

/// Build a JSON payload for a failed job.
pub fn build_job_failed_payload(job_id: &str, error: &str) -> String {
    format!(
        r#"{{"job_id":"{}","status":"failed","error":"{}"}}"#,
        escape_json_string(job_id),
        escape_json_string(error)
    )
}

/// Build a JSON payload for a package validation result.
pub fn build_validation_payload(
    package_dir: &Path,
    valid: bool,
    errors: u32,
    warnings: u32,
) -> String {
    format!(
        r#"{{"package_dir":"{}","valid":{},"errors":{},"warnings":{}}}"#,
        escape_json_string(&package_dir.display().to_string()),
        valid,
        errors,
        warnings
    )
}

/// Test a webhook endpoint with a ping event.
pub fn test_webhook(config: &WebhookConfig) -> WebhookResult {
    let event = WebhookEvent {
        event_type: "ping".to_string(),
        job_id: String::new(),
        payload_json: r#"{"message":"postkit webhook test"}"#.to_string(),
        timestamp: iso_timestamp(),
    };
    send_webhook(config, &event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_json_string() {
        assert_eq!(escape_json_string("hello"), "hello");
        assert_eq!(escape_json_string("a\"b"), "a\\\"b");
        assert_eq!(escape_json_string("a\\b"), "a\\\\b");
        assert_eq!(escape_json_string("a\nb"), "a\\nb");
        assert_eq!(escape_json_string("a\tb"), "a\\tb");
    }

    #[test]
    fn test_build_event_body() {
        let event = WebhookEvent {
            event_type: "job.completed".to_string(),
            job_id: "abc-123".to_string(),
            payload_json: r#"{"result":"ok"}"#.to_string(),
            timestamp: "2024-01-15T10:00:00Z".to_string(),
        };
        let body = build_event_body(&event);
        assert!(body.contains(r#""type":"job.completed""#));
        assert!(body.contains(r#""job_id":"abc-123""#));
        assert!(body.contains(r#""timestamp":"2024-01-15T10:00:00Z""#));
        assert!(body.contains(r#""data":{"result":"ok"}"#));
    }

    #[test]
    fn test_build_event_body_defaults() {
        let event = WebhookEvent {
            event_type: "test".to_string(),
            job_id: String::new(),
            payload_json: String::new(),
            timestamp: String::new(),
        };
        let body = build_event_body(&event);
        assert!(body.contains(r#""data":{}"#));
        assert!(body.contains('T'));
        assert!(body.contains('Z'));
    }

    #[test]
    fn test_build_job_completed_payload() {
        let payload = build_job_completed_payload("job-1", Path::new("/output/pkg"), 125.5);
        assert!(payload.contains(r#""job_id":"job-1""#));
        assert!(payload.contains(r#""status":"completed""#));
        assert!(payload.contains(r#""output_dir":"/output/pkg""#));
        assert!(payload.contains(r#""duration_seconds":125.5"#));
    }

    #[test]
    fn test_build_job_failed_payload() {
        let payload = build_job_failed_payload("job-2", "Encode failed");
        assert!(payload.contains(r#""job_id":"job-2""#));
        assert!(payload.contains(r#""status":"failed""#));
        assert!(payload.contains(r#""error":"Encode failed""#));
    }

    #[test]
    fn test_build_validation_payload() {
        let payload = build_validation_payload(Path::new("/data/pkg"), true, 0, 2);
        assert!(payload.contains(r#""package_dir":"/data/pkg""#));
        assert!(payload.contains(r#""valid":true"#));
        assert!(payload.contains(r#""errors":0"#));
        assert!(payload.contains(r#""warnings":2"#));
    }

    #[test]
    fn test_build_validation_payload_invalid() {
        let payload = build_validation_payload(Path::new("/data/pkg"), false, 3, 1);
        assert!(payload.contains(r#""valid":false"#));
        assert!(payload.contains(r#""errors":3"#));
    }

    #[test]
    fn test_send_webhook_to_invalid_url() {
        let config = WebhookConfig {
            url: "http://127.0.0.1:1/nonexistent".to_string(),
            timeout_seconds: 1,
            max_retries: 0,
            ..Default::default()
        };
        let event = WebhookEvent {
            event_type: "test".to_string(),
            job_id: "j1".to_string(),
            payload_json: "{}".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        };
        let result = send_webhook(&config, &event);
        assert!(!result.success);
        assert_eq!(result.attempts, 1);
    }

    #[test]
    fn test_webhook_config_default() {
        let config = WebhookConfig::default();
        assert_eq!(config.timeout_seconds, 10);
        assert_eq!(config.max_retries, 3);
        assert!(config.verify_ssl);
    }
}
