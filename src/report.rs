use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// Report output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportFormat {
    Text,
    Json,
    Html,
}

/// A single report entry (finding, metric, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportEntry {
    pub severity: String,
    pub category: String,
    pub message: String,
    pub details: String,
}

/// QC / validation report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub title: String,
    pub timestamp: String,
    pub summary: String,
    pub entries: Vec<ReportEntry>,
    pub pass_count: u32,
    pub warning_count: u32,
    pub error_count: u32,
}

impl Report {
    pub fn add_entry(&mut self, entry: ReportEntry) {
        match entry.severity.to_ascii_lowercase().as_str() {
            "pass" => self.pass_count += 1,
            "warning" => self.warning_count += 1,
            "error" => self.error_count += 1,
            _ => {}
        }
        self.entries.push(entry);
    }

    /// Render the report to a string in the specified format.
    pub fn render(&self, format: ReportFormat) -> String {
        match format {
            ReportFormat::Text => self.render_text(),
            ReportFormat::Json => self.render_json(),
            ReportFormat::Html => self.render_html(),
        }
    }

    /// Write the report to a file.
    pub fn write_to_file(&self, path: &Path, format: ReportFormat) -> std::io::Result<()> {
        let content = self.render(format);
        let mut file = std::fs::File::create(path)?;
        file.write_all(content.as_bytes())
    }

    fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("=== {} ===\n", self.title));
        out.push_str(&format!("Date: {}\n", self.timestamp));
        out.push_str(&format!(
            "Summary: {} pass, {} warnings, {} errors\n\n",
            self.pass_count, self.warning_count, self.error_count
        ));
        for entry in &self.entries {
            out.push_str(&format!(
                "[{}] {}: {}\n",
                entry.severity, entry.category, entry.message
            ));
            if !entry.details.is_empty() {
                out.push_str(&format!("  {}\n", entry.details));
            }
        }
        out
    }

    fn render_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    fn render_html(&self) -> String {
        let mut out = String::new();
        out.push_str("<!DOCTYPE html><html><head><meta charset='utf-8'>");
        out.push_str("<title>");
        out.push_str(&self.title);
        out.push_str("</title>");
        out.push_str("<style>body{font-family:sans-serif;margin:2em}");
        out.push_str("table{border-collapse:collapse;width:100%}");
        out.push_str("th,td{border:1px solid #ccc;padding:8px;text-align:left}");
        out.push_str(".error{color:#c00}.warning{color:#c80}.pass{color:#0a0}");
        out.push_str("</style></head><body>");
        out.push_str(&format!("<h1>{}</h1>", self.title));
        out.push_str(&format!("<p>Date: {}</p>", self.timestamp));
        out.push_str(&format!(
            "<p><span class='pass'>{} pass</span> | <span class='warning'>{} warnings</span> | <span class='error'>{} errors</span></p>",
            self.pass_count, self.warning_count, self.error_count
        ));
        out.push_str(
            "<table><tr><th>Severity</th><th>Category</th><th>Message</th><th>Details</th></tr>",
        );
        for entry in &self.entries {
            let class = match entry.severity.to_lowercase().as_str() {
                "error" => "error",
                "warning" => "warning",
                _ => "pass",
            };
            out.push_str(&format!(
                "<tr><td class='{class}'>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                entry.severity, entry.category, entry.message, entry.details
            ));
        }
        out.push_str("</table></body></html>");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(severity: &str) -> ReportEntry {
        ReportEntry {
            severity: severity.into(),
            category: "picture".into(),
            message: "finding".into(),
            details: String::new(),
        }
    }

    #[test]
    fn adding_an_entry_updates_its_counter() {
        let mut report = Report::default();
        report.add_entry(entry("pass"));
        report.add_entry(entry("warning"));
        report.add_entry(entry("error"));
        report.add_entry(entry("info"));

        assert_eq!(report.pass_count, 1);
        assert_eq!(report.warning_count, 1);
        assert_eq!(report.error_count, 1);
        assert_eq!(report.entries.len(), 4);
    }
}
