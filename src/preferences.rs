use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Preferences schema migration.
/// Each migration upgrades from `(version - 1)` to `version`.
pub struct PrefsMigration {
    /// Target version after this migration.
    pub version: u32,
    /// Human-readable description, e.g. "Add gpu_device field".
    pub description: String,
    /// Mutate the raw JSON string to apply the migration.
    pub apply: Box<dyn Fn(&str) -> String>,
}

/// Read the `"version"` field from a JSON preferences string.
/// Returns 0 if the field is missing (pre-versioning config).
pub fn prefs_version(json: &str) -> u32 {
    #[derive(Deserialize)]
    struct V {
        #[serde(default)]
        version: u32,
    }
    serde_json::from_str::<V>(json)
        .map(|v| v.version)
        .unwrap_or(0)
}

/// Set the `"version"` field in a JSON preferences string.
pub fn prefs_set_version(json: &str, version: u32) -> String {
    let mut val: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    if let Some(obj) = val.as_object_mut() {
        obj.insert("version".to_string(), serde_json::Value::from(version));
    }
    serde_json::to_string_pretty(&val).unwrap_or_else(|_| json.to_string())
}

/// Run all applicable migrations on a JSON preferences string.
/// Applies migrations where `migration.version > current_version`,
/// in ascending order. Returns the migrated JSON (with updated version).
pub fn migrate_preferences(json: &str, migrations: &[PrefsMigration]) -> String {
    let current = prefs_version(json);
    let mut result = json.to_string();

    let mut sorted: Vec<&PrefsMigration> = migrations.iter().collect();
    sorted.sort_by_key(|m| m.version);

    let mut latest = current;
    for m in sorted {
        if m.version > current {
            result = (m.apply)(&result);
            latest = m.version;
        }
    }

    prefs_set_version(&result, latest)
}

/// Insert a key-value pair into a JSON object string if the key doesn't exist.
/// `value` should be a valid JSON literal (quoted string, number, bool, etc.).
pub fn json_insert_if_missing(json: &str, key: &str, value: &str) -> String {
    let mut val: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return json.to_string(),
    };
    if let Some(obj) = val.as_object_mut()
        && !obj.contains_key(key)
    {
        let parsed: serde_json::Value =
            serde_json::from_str(value).unwrap_or(serde_json::Value::Null);
        obj.insert(key.to_string(), parsed);
    }
    serde_json::to_string_pretty(&val).unwrap_or_else(|_| json.to_string())
}

/// Get the platform-specific config directory for an app.
///
/// - Linux: `$XDG_CONFIG_HOME/<app>` or `~/.config/<app>`
/// - macOS: `~/Library/Application Support/<app>`
/// - Windows: `%APPDATA%/<app>`
pub fn config_dir(app_name: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(app_name)
}

pub fn read_preferences_file(path: &Path) -> io::Result<Option<String>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_preferences_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())?;
    file.flush()
}

pub fn set_json_preference<T>(preferences: &T, name: &str, value: &str) -> Result<T, String>
where
    T: DeserializeOwned + Serialize,
{
    let mut json = serde_json::to_value(preferences).map_err(|error| error.to_string())?;
    let object = json
        .as_object_mut()
        .ok_or_else(|| "preferences must be a JSON object".to_string())?;
    let normalized_name = preference_name(name);
    let name = if object.contains_key(name) {
        name
    } else {
        normalized_name.as_str()
    };
    let current = object
        .get(name)
        .ok_or_else(|| format!("unknown preference: {name}"))?;
    let parsed = match current {
        serde_json::Value::Bool(_) => serde_json::Value::Bool(
            value
                .parse()
                .map_err(|_| format!("{name} requires true or false"))?,
        ),
        serde_json::Value::Number(_) => {
            let parsed: serde_json::Value =
                serde_json::from_str(value).map_err(|_| format!("{name} requires a number"))?;
            if !parsed.is_number() {
                return Err(format!("{name} requires a number"));
            }
            parsed
        }
        serde_json::Value::String(_) => serde_json::Value::String(value.to_string()),
        _ => serde_json::from_str(value).map_err(|error| error.to_string())?,
    };
    object.insert(name.to_string(), parsed);
    serde_json::from_value(json).map_err(|error| error.to_string())
}

fn preference_name(name: &str) -> String {
    let mut uppercase_next = false;
    name.chars()
        .filter_map(|character| {
            if character == '-' || character == '_' {
                uppercase_next = true;
                return None;
            }
            if uppercase_next {
                uppercase_next = false;
                return Some(character.to_ascii_uppercase());
            }
            Some(character)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_from_json() {
        assert_eq!(prefs_version(r#"{"version": 3, "foo": "bar"}"#), 3);
        assert_eq!(prefs_version(r#"{"foo": "bar"}"#), 0);
        assert_eq!(prefs_version("invalid"), 0);
    }

    #[test]
    fn set_version() {
        let json = r#"{"version": 1, "name": "test"}"#;
        let result = prefs_set_version(json, 5);
        assert_eq!(prefs_version(&result), 5);
        assert!(result.contains("\"name\""));
    }

    #[test]
    fn set_version_missing_field() {
        let json = r#"{"name": "test"}"#;
        let result = prefs_set_version(json, 2);
        assert_eq!(prefs_version(&result), 2);
    }

    #[test]
    fn migrate() {
        let json = r#"{"version": 1, "name": "test"}"#;
        let migrations = vec![
            PrefsMigration {
                version: 2,
                description: "Add colour field".to_string(),
                apply: Box::new(|j| {
                    crate::preferences::json_insert_if_missing(j, "colour", "\"rec709\"")
                }),
            },
            PrefsMigration {
                version: 3,
                description: "Add gpu field".to_string(),
                apply: Box::new(|j| crate::preferences::json_insert_if_missing(j, "gpu", "0")),
            },
        ];
        let result = migrate_preferences(json, &migrations);
        assert_eq!(prefs_version(&result), 3);
        assert!(result.contains("\"colour\""));
        assert!(result.contains("\"gpu\""));
    }

    #[test]
    fn insert_if_missing_adds() {
        let json = r#"{"name": "test"}"#;
        let result = json_insert_if_missing(json, "fps", "24");
        assert!(result.contains("\"fps\""));
        assert!(result.contains("24"));
    }

    #[test]
    fn insert_if_missing_no_overwrite() {
        let json = r#"{"name": "test", "fps": 30}"#;
        let result = json_insert_if_missing(json, "fps", "24");
        assert!(result.contains("30"));
    }

    #[test]
    fn config_dir_nonempty() {
        let dir = config_dir("postkit-test");
        assert!(dir.to_string_lossy().contains("postkit-test"));
    }

    #[test]
    fn set_json_preference_preserves_types() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct TestPreferences {
            enabled: bool,
            bandwidth: u32,
            output_dir: String,
        }

        let preferences = TestPreferences {
            enabled: false,
            bandwidth: 230,
            output_dir: String::new(),
        };
        let preferences = set_json_preference(&preferences, "enabled", "true").unwrap();
        let preferences = set_json_preference(&preferences, "bandwidth", "180").unwrap();
        let preferences = set_json_preference(&preferences, "output-dir", "/tmp/out").unwrap();

        assert_eq!(
            preferences,
            TestPreferences {
                enabled: true,
                bandwidth: 180,
                output_dir: "/tmp/out".to_string(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn preferences_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        write_preferences_file(&path, "{}").unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
