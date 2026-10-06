// one JSON line per KDM issued, metadata only: never key material or KDM bodies
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    // utc, RFC 3339
    pub timestamp: String,
    pub cpl_id: String,
    pub content_title: String,
    pub recipient_subject: String,
    pub recipient_serial: String,
    pub valid_from: String,
    pub valid_to: String,
    pub output_path: String,
}

impl Record {
    pub fn now(
        cpl_id: &str,
        content_title: &str,
        recipient_subject: &str,
        recipient_serial: &str,
        valid_from: &str,
        valid_to: &str,
        output_path: &str,
    ) -> Self {
        Record {
            timestamp: chrono::Utc::now().to_rfc3339(),
            cpl_id: cpl_id.to_string(),
            content_title: content_title.to_string(),
            recipient_subject: recipient_subject.to_string(),
            recipient_serial: recipient_serial.to_string(),
            valid_from: valid_from.to_string(),
            valid_to: valid_to.to_string(),
            output_path: output_path.to_string(),
        }
    }
}

// O_APPEND keeps each short line whole when two writers append at once
pub fn append(path: &Path, rec: &Record) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut line = serde_json::to_string(rec).map_err(|e| format!("serialize record: {e}"))?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open history {}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("cannot append history: {e}"))
}

// a missing file is an empty history, a malformed line fails loud
pub fn read_all(path: &Path) -> Result<Vec<Record>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read history {}: {e}", path.display())),
    };
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: Record = serde_json::from_str(line)
            .map_err(|e| format!("corrupt history line {}: {e}", i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

// date bounds match the RFC 3339 timestamp prefix, so "2026-07" or a full date works
pub fn filter(
    records: Vec<Record>,
    title: Option<&str>,
    recipient: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Vec<Record> {
    let lc = |s: &str| s.to_lowercase();
    records
        .into_iter()
        .filter(|r| {
            title
                .map(|t| lc(&r.content_title).contains(&lc(t)))
                .unwrap_or(true)
                && recipient
                    .map(|q| {
                        lc(&r.recipient_subject).contains(&lc(q))
                            || lc(&r.recipient_serial).contains(&lc(q))
                    })
                    .unwrap_or(true)
                && since.map(|s| r.timestamp.as_str() >= s).unwrap_or(true)
                // inclusive: everything up to the end of the `until` prefix
                && until
                    .map(|u| r.timestamp <= format!("{u}\u{7f}"))
                    .unwrap_or(true)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(title: &str, serial: &str, ts: &str) -> Record {
        Record {
            timestamp: ts.to_string(),
            cpl_id: "cpl".into(),
            content_title: title.into(),
            recipient_subject: "CN=Screen".into(),
            recipient_serial: serial.into(),
            valid_from: "a".into(),
            valid_to: "b".into(),
            output_path: "/out.kdm.xml".into(),
        }
    }

    #[test]
    fn append_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("h.jsonl");
        append(
            &p,
            &Record::now("cpl", "Feature", "CN=S", "4ca4", "f", "t", "/o"),
        )
        .unwrap();
        append(&p, &rec("Other", "abcd", "2026-07-23T10:00:00+00:00")).unwrap();
        let all = read_all(&p).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].content_title, "Feature");
    }

    #[test]
    fn record_never_contains_key_material() {
        let r = Record::now("cpl", "Feature", "CN=S", "4ca4", "f", "t", "/o");
        let j = serde_json::to_string(&r).unwrap();
        for banned in ["key", "cipher", "secret", "private"] {
            assert!(
                !j.to_lowercase().contains(banned),
                "history leaked '{banned}'"
            );
        }
    }

    #[test]
    fn filter_by_title_recipient_and_date() {
        let recs = vec![
            rec("Feature One", "aaaa", "2026-07-01T00:00:00+00:00"),
            rec("Feature Two", "bbbb", "2026-07-20T00:00:00+00:00"),
            rec("Trailer", "cccc", "2026-08-01T00:00:00+00:00"),
        ];
        assert_eq!(
            filter(recs.clone(), Some("feature"), None, None, None).len(),
            2
        );
        assert_eq!(
            filter(recs.clone(), None, Some("bbbb"), None, None).len(),
            1
        );
        let july = filter(recs.clone(), None, None, Some("2026-07"), Some("2026-07"));
        assert_eq!(july.len(), 2);
    }

    #[test]
    fn record_written_before_the_format_field_was_dropped_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.jsonl");
        std::fs::write(
            &p,
            r#"{"timestamp":"2026-07-01T00:00:00+00:00","cpl_id":"cpl","content_title":"Feature","recipient_subject":"CN=S","recipient_serial":"4ca4","valid_from":"f","valid_to":"t","output_path":"/o","format":"smpte"}
"#,
        )
        .unwrap();
        let all = read_all(&p).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content_title, "Feature");
    }

    #[test]
    fn corrupt_line_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.jsonl");
        std::fs::write(&p, "{not json}\n").unwrap();
        assert!(read_all(&p).is_err());
    }
}
