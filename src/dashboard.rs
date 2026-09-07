use crate::rest_api::RouteResponse;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DistributionRow {
    pub territory: String,
    pub cells: Vec<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DistributionMatrix {
    pub titles: Vec<String>,
    pub rows: Vec<DistributionRow>,
}

pub fn distribution_matrix_at(db_path: &Path) -> DistributionMatrix {
    let versions = list_versions_at(db_path, None, None);

    let mut territories: Vec<String> = versions.iter().map(|v| v.territory.clone()).collect();
    territories.sort();
    territories.dedup();

    let mut titles: Vec<String> = versions.iter().map(|v| v.title.clone()).collect();
    titles.sort();
    titles.dedup();

    let rows = territories
        .into_iter()
        .map(|territory| DistributionRow {
            cells: titles
                .iter()
                .map(|title| {
                    versions
                        .iter()
                        .any(|v| v.territory == territory && v.title == *title)
                })
                .collect(),
            territory,
        })
        .collect();

    DistributionMatrix { titles, rows }
}

/// Generate a distribution matrix (territory × version grid) as CSV.
pub fn export_distribution_matrix(output_csv: &Path) -> i32 {
    export_distribution_matrix_at(&default_db_path(), output_csv)
}

pub fn export_distribution_matrix_at(db_path: &Path, output_csv: &Path) -> i32 {
    let matrix = distribution_matrix_at(db_path);
    if matrix.rows.is_empty() {
        tracing::warn!("No versions found");
        return -1;
    }

    let mut csv = String::from("Territory");
    for title in &matrix.titles {
        csv.push(',');
        csv.push_str(title);
    }
    csv.push('\n');

    for row in &matrix.rows {
        csv.push_str(&row.territory);
        for cell in &row.cells {
            csv.push(',');
            csv.push_str(if *cell { "✓" } else { "" });
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
    // answers {"titles":["Feature A"],"rows":[{"territory":"US","cells":[true]}]}, one cell per title
    "/api/matrix",
];

const DASHBOARD_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>DCP version dashboard</title>
<style>
body { font-family: system-ui, sans-serif; margin: 2rem; color: #222; }
h1 { font-size: 1.4rem; }
h2 { font-size: 1.1rem; margin-top: 2rem; }
table { border-collapse: collapse; }
th, td { border: 1px solid #bbb; padding: 0.25rem 0.6rem; text-align: left; }
th { background: #f0f0f0; }
#summary { font-size: 1.1rem; }
</style>
</head>
<body>
<h1>DCP version dashboard</h1>
<p id="summary">loading</p>
<h2>Versions</h2>
<table id="versions"></table>
<h2>Territory by title</h2>
<table id="matrix"></table>
<script>
function cell(tag, value) {
  const element = document.createElement(tag);
  element.textContent = value;
  return element;
}

function fillTable(id, headers, rows) {
  const table = document.getElementById(id);
  table.replaceChildren();
  const headerRow = table.insertRow();
  headers.forEach(header => headerRow.append(cell('th', header)));
  rows.forEach(values => {
    const row = table.insertRow();
    values.forEach(value => row.append(cell('td', value)));
  });
}

async function load() {
  const paths = ['/api/summary', '/api/versions', '/api/territories', '/api/matrix'];
  const [summary, versions, territories, matrix] = await Promise.all(
    paths.map(path => fetch(path).then(response => response.json()))
  );

  const statuses = Object.entries(summary.by_status)
    .map(([status, count]) => status + ': ' + count)
    .join(', ');
  document.getElementById('summary').textContent =
    summary.total_versions + ' versions in ' + summary.total_territories +
    ' territories (' + statuses + ')';

  fillTable(
    'versions',
    ['Title', 'Type', 'Territory', 'Language', 'Standard', 'Status', 'UUID', 'KDM recipients'],
    versions.map(version => [
      version.title, version.version_type, version.territory, version.language,
      version.standard, version.status, version.uuid, version.kdm_recipients.length
    ])
  );

  const names = new Map(territories.map(territory => [territory.code, territory.name]));
  fillTable(
    'matrix',
    ['Territory'].concat(matrix.titles),
    matrix.rows.map(row => [names.get(row.territory) || row.territory].concat(
      row.cells.map(present => present ? 'yes' : '')
    ))
  );
}

load();
</script>
</body>
</html>
"#;

const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

pub fn dashboard_response(db_path: &Path, path: &str) -> RouteResponse {
    let json = |body: String| RouteResponse::json(200, body);
    match path {
        "/" => RouteResponse {
            status: 200,
            content_type: HTML_CONTENT_TYPE,
            body: DASHBOARD_PAGE.to_string(),
        },
        "/health" => json(
            serde_json::json!({ "status": "ok", "endpoints": DASHBOARD_ENDPOINTS }).to_string(),
        ),
        "/api/versions" => {
            let versions = list_versions_at(db_path, None, None);
            json(serde_json::to_string(&versions).unwrap_or_else(|_| "[]".to_string()))
        }
        "/api/territories" => {
            let territories = list_territories_at(db_path);
            json(serde_json::to_string(&territories).unwrap_or_else(|_| "[]".to_string()))
        }
        "/api/summary" => json(summary_json(db_path)),
        "/api/matrix" => {
            let matrix = distribution_matrix_at(db_path);
            json(
                serde_json::to_string(&matrix)
                    .unwrap_or_else(|_| r#"{"titles":[],"rows":[]}"#.to_string()),
            )
        }
        _ => RouteResponse::json(404, r#"{"error":"not found"}"#.to_string()),
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

/// Start the web dashboard: a blocking HTTP server serving the page at `/` and
/// the version and distribution data as JSON.
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
        server.route_with_content_type(
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
    use crate::rest_api::JSON_CONTENT_TYPE;

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

        let response = dashboard_response(&db, "/api/versions");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, JSON_CONTENT_TYPE);
        assert!(response.body.contains("Feature A"));
        assert!(response.body.contains("\"territory\":\"US\""));
        assert!(response.body.contains("\"territory\":\"FR\""));
    }

    #[test]
    fn test_dashboard_response_territories() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let response = dashboard_response(&db, "/api/territories");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("United States"));
        assert!(response.body.contains("France"));
    }

    #[test]
    fn test_dashboard_response_summary() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let response = dashboard_response(&db, "/api/summary");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"total_versions\":2"));
        assert!(response.body.contains("\"total_territories\":2"));
        assert!(response.body.contains("\"released\":1"));
        assert!(response.body.contains("\"draft\":1"));
    }

    #[test]
    fn test_dashboard_response_index_and_404() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let response = dashboard_response(&db, "/health");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("/api/versions"));

        let response = dashboard_response(&db, "/nope");
        assert_eq!(response.status, 404);
        assert_eq!(response.content_type, JSON_CONTENT_TYPE);
    }

    fn http_get(port: u16, path: &str) -> String {
        use std::io::{Read, Write};
        for _ in 0..100 {
            let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            };
            stream
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            return response;
        }
        panic!("dashboard never answered on port {port}");
    }

    #[test]
    fn test_serve_dashboard_answers_over_http() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let opts = DashboardOptions {
            database_path: db,
            http_port: u32::from(port),
            bind_address: "127.0.0.1".to_string(),
        };
        std::thread::spawn(move || serve_dashboard(&opts));

        let index = http_get(port, "/");
        assert!(index.starts_with("HTTP/1.1 200 OK"), "{index}");
        assert!(
            index.contains("Content-Type: text/html; charset=utf-8"),
            "{index}"
        );
        assert!(index.contains("<table"), "{index}");

        let matrix = http_get(port, "/api/matrix");
        assert!(
            matrix.contains("Content-Type: application/json"),
            "{matrix}"
        );
        assert!(matrix.contains(r#""titles":["Feature A"]"#), "{matrix}");
    }

    #[test]
    fn test_dashboard_index_is_an_html_page() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let response = dashboard_response(&db, "/");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(response.body.contains("<table"));
        assert!(response.body.contains("/api/matrix"));
        assert!(response.body.contains("/api/summary"));
        assert!(response.body.contains("/api/versions"));
        assert!(response.body.contains("/api/territories"));
    }

    #[test]
    fn test_matrix_endpoint_agrees_with_the_csv() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        seed_db(&db);

        let response = dashboard_response(&db, "/api/matrix");
        assert_eq!(response.status, 200);
        let matrix: DistributionMatrix = serde_json::from_str(&response.body).unwrap();
        assert_eq!(matrix.titles, vec!["Feature A"]);
        assert_eq!(
            matrix
                .rows
                .iter()
                .map(|r| (r.territory.as_str(), r.cells.clone()))
                .collect::<Vec<_>>(),
            vec![("FR", vec![true]), ("US", vec![true])]
        );

        let csv_path = dir.path().join("matrix.csv");
        assert_eq!(export_distribution_matrix_at(&db, &csv_path), 0);
        let csv = std::fs::read_to_string(&csv_path).unwrap();

        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!("Territory,{}", matrix.titles.join(","))
        );
        for (line, row) in lines.zip(&matrix.rows) {
            let expected: Vec<&str> = row
                .cells
                .iter()
                .map(|c| if *c { "✓" } else { "" })
                .collect();
            assert_eq!(line, format!("{},{}", row.territory, expected.join(",")));
        }
    }
}
