use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A version entry in the OV/VF management system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionEntry {
    pub uuid: String,
    pub title: String,
    /// "OV" or "VF"
    pub version_type: String,
    /// ISO 3166-1 alpha-2 (e.g. "US", "GB", "FR")
    pub territory: String,
    /// RFC 5646
    pub language: String,
    /// "SMPTE" or "Interop"
    pub standard: String,
    pub dcp_path: PathBuf,
    /// For VFs: the referenced OV UUID
    pub ov_uuid: String,
    pub created_date: String,
    /// "draft", "released", "archived"
    pub status: String,
    /// Theater names
    pub kdm_recipients: Vec<String>,
}

/// Territory distribution info.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerritoryInfo {
    /// "US", "GB", etc.
    pub code: String,
    /// "United States", "United Kingdom"
    pub name: String,
    pub version_count: u32,
    pub languages: Vec<String>,
}

/// Dashboard database options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardOptions {
    pub database_path: PathBuf,
    pub http_port: u32,
    pub bind_address: String,
}

impl Default for DashboardOptions {
    fn default() -> Self {
        Self {
            database_path: PathBuf::new(),
            http_port: 9090,
            bind_address: "127.0.0.1".to_string(),
        }
    }
}

/// Initialize the version management database.
pub fn init_database(db_path: &Path) -> i32 {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to open database: {e}");
            return -1;
        }
    };

    let rc = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS versions (
            uuid TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            version_type TEXT NOT NULL DEFAULT 'OV',
            territory TEXT NOT NULL DEFAULT '',
            language TEXT NOT NULL DEFAULT '',
            standard TEXT NOT NULL DEFAULT 'SMPTE',
            dcp_path TEXT NOT NULL DEFAULT '',
            ov_uuid TEXT NOT NULL DEFAULT '',
            created_date TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'draft',
            kdm_recipients TEXT NOT NULL DEFAULT '[]'
        );
        CREATE INDEX IF NOT EXISTS idx_territory ON versions(territory);
        CREATE INDEX IF NOT EXISTS idx_status ON versions(status);",
    );

    match rc {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("Failed to create tables: {e}");
            -1
        }
    }
}

/// Register a new DCP version (OV or VF).
pub fn register_version(entry: &VersionEntry) -> i32 {
    let db_path = default_db_path();
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to open database: {e}");
            return -1;
        }
    };

    let recipients_json = serde_json::to_string(&entry.kdm_recipients).unwrap_or_default();
    let rc = conn.execute(
        "INSERT OR REPLACE INTO versions (uuid, title, version_type, territory, language, standard, dcp_path, ov_uuid, created_date, status, kdm_recipients)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            entry.uuid,
            entry.title,
            entry.version_type,
            entry.territory,
            entry.language,
            entry.standard,
            entry.dcp_path.to_string_lossy(),
            entry.ov_uuid,
            entry.created_date,
            entry.status,
            recipients_json,
        ],
    );

    match rc {
        Ok(_) => 0,
        Err(e) => {
            tracing::error!("Failed to insert version: {e}");
            -1
        }
    }
}

/// List all versions, optionally filtered.
pub fn list_versions(territory: Option<&str>, status: Option<&str>) -> Vec<VersionEntry> {
    list_versions_at(&default_db_path(), territory, status)
}

/// List versions from a specific database file.
pub fn list_versions_at(
    db_path: &Path,
    territory: Option<&str>,
    status: Option<&str>,
) -> Vec<VersionEntry> {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut sql = "SELECT uuid, title, version_type, territory, language, standard, dcp_path, ov_uuid, created_date, status, kdm_recipients FROM versions WHERE 1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(t) = territory {
        sql.push_str(" AND territory = ?");
        params.push(Box::new(t.to_string()));
    }
    if let Some(s) = status {
        sql.push_str(" AND status = ?");
        params.push(Box::new(s.to_string()));
    }
    sql.push_str(" ORDER BY created_date DESC");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let kdm_json: String = row.get(10)?;
            let kdm_recipients: Vec<String> = serde_json::from_str(&kdm_json).unwrap_or_default();
            let dcp_path_str: String = row.get(6)?;
            Ok(VersionEntry {
                uuid: row.get(0)?,
                title: row.get(1)?,
                version_type: row.get(2)?,
                territory: row.get(3)?,
                language: row.get(4)?,
                standard: row.get(5)?,
                dcp_path: PathBuf::from(dcp_path_str),
                ov_uuid: row.get(7)?,
                created_date: row.get(8)?,
                status: row.get(9)?,
                kdm_recipients,
            })
        })
        .ok();

    rows.map(|r| r.flatten().collect()).unwrap_or_default()
}

/// List territories with version counts.
pub fn list_territories() -> Vec<TerritoryInfo> {
    list_territories_at(&default_db_path())
}

/// List territories with version counts from a specific database file.
pub fn list_territories_at(db_path: &Path) -> Vec<TerritoryInfo> {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut stmt = match conn.prepare(
        "SELECT territory, COUNT(*), GROUP_CONCAT(DISTINCT language) FROM versions GROUP BY territory ORDER BY territory",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt
        .query_map([], |row| {
            let code: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            let langs: String = row.get::<_, String>(2).unwrap_or_default();
            Ok(TerritoryInfo {
                code: code.clone(),
                name: territory_name(&code).to_string(),
                version_count: count,
                languages: langs
                    .split(',')
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            })
        })
        .ok();

    rows.map(|r| r.flatten().collect()).unwrap_or_default()
}

/// Update version status (draft → released → archived).
pub fn update_status(uuid: &str, new_status: &str) -> i32 {
    let db_path = default_db_path();
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to open database: {e}");
            return -1;
        }
    };

    match conn.execute(
        "UPDATE versions SET status = ?1 WHERE uuid = ?2",
        rusqlite::params![new_status, uuid],
    ) {
        Ok(0) => -1, // no rows affected
        Ok(_) => 0,
        Err(e) => {
            tracing::error!("Failed to update status: {e}");
            -1
        }
    }
}

/// Generate a distribution matrix (territory × version grid) as CSV.
pub fn export_distribution_matrix(output_csv: &Path) -> i32 {
    let versions = list_versions(None, None);
    if versions.is_empty() {
        tracing::warn!("No versions found");
        return -1;
    }

    let mut territories: Vec<String> = versions.iter().map(|v| v.territory.clone()).collect();
    territories.sort();
    territories.dedup();

    let mut titles: Vec<String> = versions.iter().map(|v| v.title.clone()).collect();
    titles.sort();
    titles.dedup();

    let mut csv = String::from("Territory");
    for title in &titles {
        csv.push(',');
        csv.push_str(title);
    }
    csv.push('\n');

    for territory in &territories {
        csv.push_str(territory);
        for title in &titles {
            let has = versions
                .iter()
                .any(|v| v.territory == *territory && v.title == *title);
            csv.push(',');
            csv.push_str(if has { "✓" } else { "" });
        }
        csv.push('\n');
    }

    match std::fs::write(output_csv, csv) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("Failed to write CSV: {e}");
            -1
        }
    }
}

/// Endpoints served by the dashboard, for the index/discovery response.
const DASHBOARD_ENDPOINTS: &[&str] = &[
    "/",
    "/health",
    "/api/versions",
    "/api/territories",
    "/api/summary",
];

/// Build the (status, json) response for a dashboard API path against `db_path`.
///
/// This is the handler the HTTP server dispatches to; kept separate so it can be
/// tested directly against a real database without binding a socket.
pub fn dashboard_response(db_path: &Path, path: &str) -> (u16, String) {
    match path {
        "/" | "/health" => (
            200,
            serde_json::json!({ "status": "ok", "endpoints": DASHBOARD_ENDPOINTS }).to_string(),
        ),
        "/api/versions" => {
            let versions = list_versions_at(db_path, None, None);
            (
                200,
                serde_json::to_string(&versions).unwrap_or_else(|_| "[]".to_string()),
            )
        }
        "/api/territories" => {
            let territories = list_territories_at(db_path);
            (
                200,
                serde_json::to_string(&territories).unwrap_or_else(|_| "[]".to_string()),
            )
        }
        "/api/summary" => (200, summary_json(db_path)),
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    }
}

/// Aggregate analytics: totals, per-status and per-territory counts.
fn summary_json(db_path: &Path) -> String {
    let versions = list_versions_at(db_path, None, None);
    let territories = list_territories_at(db_path);

    let mut by_status: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for v in &versions {
        *by_status.entry(v.status.clone()).or_insert(0) += 1;
    }

    serde_json::json!({
        "total_versions": versions.len(),
        "total_territories": territories.len(),
        "by_status": by_status,
    })
    .to_string()
}

/// Start the web dashboard: a blocking HTTP server serving the version and
/// distribution data as JSON, built on the shared rest_api server.
pub fn serve_dashboard(opts: &DashboardOptions) -> i32 {
    let db_path = if opts.database_path.as_os_str().is_empty() {
        default_db_path()
    } else {
        opts.database_path.clone()
    };

    let bind = format!("{}:{}", opts.bind_address, opts.http_port);
    let mut server = crate::rest_api::RestServer::new(&bind);

    for path in DASHBOARD_ENDPOINTS {
        let db = db_path.clone();
        server.route(
            "GET",
            path,
            Box::new(move |_method, req_path| dashboard_response(&db, req_path)),
        );
    }

    match server.start() {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("Dashboard server failed to start on {bind}: {e}");
            -1
        }
    }
}

fn default_db_path() -> PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("postkit");
    let _ = std::fs::create_dir_all(&config_dir);
    config_dir.join("versions.db")
}

fn territory_name(code: &str) -> &str {
    match code {
        "US" => "United States",
        "GB" => "United Kingdom",
        "FR" => "France",
        "DE" => "Germany",
        "JP" => "Japan",
        "CN" => "China",
        "KR" => "South Korea",
        "AU" => "Australia",
        "CA" => "Canada",
        "IT" => "Italy",
        "ES" => "Spain",
        "BR" => "Brazil",
        "IN" => "India",
        "MX" => "Mexico",
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_and_register() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.db");
        assert_eq!(init_database(&db), 0);

        // Override the default db path by opening directly
        let conn = rusqlite::Connection::open(&db).unwrap();
        let entry = VersionEntry {
            uuid: "test-uuid-1".into(),
            title: "Test Feature".into(),
            version_type: "OV".into(),
            territory: "US".into(),
            language: "en".into(),
            standard: "SMPTE".into(),
            status: "draft".into(),
            ..Default::default()
        };
        let recipients_json = serde_json::to_string(&entry.kdm_recipients).unwrap();
        conn.execute(
            "INSERT INTO versions (uuid, title, version_type, territory, language, standard, dcp_path, ov_uuid, created_date, status, kdm_recipients) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![entry.uuid, entry.title, entry.version_type, entry.territory, entry.language, entry.standard, "", "", "", entry.status, recipients_json],
        ).unwrap();

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_territory_name() {
        assert_eq!(territory_name("US"), "United States");
        assert_eq!(territory_name("ZZ"), "ZZ");
    }

    fn seed_db(db: &Path) {
        assert_eq!(init_database(db), 0);
        let conn = rusqlite::Connection::open(db).unwrap();
        for (uuid, title, terr, lang, status) in [
            ("u1", "Feature A", "US", "en", "released"),
            ("u2", "Feature A", "FR", "fr", "draft"),
        ] {
            conn.execute(
                "INSERT INTO versions (uuid, title, version_type, territory, language, standard, dcp_path, ov_uuid, created_date, status, kdm_recipients) VALUES (?1,?2,'OV',?3,?4,'SMPTE','','','',?5,'[]')",
                rusqlite::params![uuid, title, terr, lang, status],
            )
            .unwrap();
        }
    }

    #[test]
    fn test_dashboard_response_versions() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let (status, body) = dashboard_response(&db, "/api/versions");
        assert_eq!(status, 200);
        assert!(body.contains("Feature A"));
        assert!(body.contains("\"territory\":\"US\""));
        assert!(body.contains("\"territory\":\"FR\""));
    }

    #[test]
    fn test_dashboard_response_territories() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let (status, body) = dashboard_response(&db, "/api/territories");
        assert_eq!(status, 200);
        assert!(body.contains("United States"));
        assert!(body.contains("France"));
    }

    #[test]
    fn test_dashboard_response_summary() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let (status, body) = dashboard_response(&db, "/api/summary");
        assert_eq!(status, 200);
        assert!(body.contains("\"total_versions\":2"));
        assert!(body.contains("\"total_territories\":2"));
        assert!(body.contains("\"released\":1"));
        assert!(body.contains("\"draft\":1"));
    }

    #[test]
    fn test_dashboard_response_index_and_404() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let (status, body) = dashboard_response(&db, "/");
        assert_eq!(status, 200);
        assert!(body.contains("/api/versions"));

        let (status, _) = dashboard_response(&db, "/nope");
        assert_eq!(status, 404);
    }
}
