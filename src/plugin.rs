//! Plugin system — discover and execute hook scripts from a plugins directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use tracing::{error, info};

/// Plugin lifecycle hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PluginHook {
    PreEncode,
    PostEncode,
    PreWrap,
    PostWrap,
    PreValidate,
    PostValidate,
    PreCreate,
    PostCreate,
}

impl PluginHook {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreEncode => "pre_encode",
            Self::PostEncode => "post_encode",
            Self::PreWrap => "pre_wrap",
            Self::PostWrap => "post_wrap",
            Self::PreValidate => "pre_validate",
            Self::PostValidate => "post_validate",
            Self::PreCreate => "pre_create",
            Self::PostCreate => "post_create",
        }
    }
}

/// Information about a discovered plugin.
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub path: PathBuf,
}

/// Discover plugins in a directory (each plugin is a subdirectory with plugin.json).
pub fn discover_plugins(plugins_dir: &Path) -> Vec<PluginInfo> {
    let mut plugins = Vec::new();

    let Ok(entries) = fs::read_dir(plugins_dir) else {
        return plugins;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let manifest = path.join("plugin.json");
        let Ok(content) = fs::read_to_string(&manifest) else {
            continue;
        };

        let name = extract_json_string(&content, "name").unwrap_or_default();
        if name.is_empty() {
            continue;
        }

        plugins.push(PluginInfo {
            name,
            version: extract_json_string(&content, "version").unwrap_or_default(),
            description: extract_json_string(&content, "description").unwrap_or_default(),
            author: extract_json_string(&content, "author").unwrap_or_default(),
            path,
        });
    }

    plugins
}

/// Execute all matching hook scripts in the plugins directory.
/// Returns true if all hooks succeeded.
pub fn execute_hook(hook: PluginHook, plugins_dir: &Path, context_json: &str) -> bool {
    let Ok(entries) = fs::read_dir(plugins_dir) else {
        return true;
    };

    let hook_name = hook.name();
    let mut all_ok = true;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let script = path.join("hooks").join(format!("{hook_name}.py"));
        if !script.exists() {
            continue;
        }

        let plugin_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        info!("Plugin: executing {}/{}", plugin_name, hook_name);

        let ctx_file = std::env::temp_dir().join("postkit_plugin_ctx.json");
        if fs::write(&ctx_file, context_json).is_err() {
            error!("Failed to write plugin context file");
            all_ok = false;
            continue;
        }

        let status = Command::new("python3").arg(&script).arg(&ctx_file).status();

        let _ = fs::remove_file(&ctx_file);

        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                error!(
                    "Plugin {} hook {} failed with exit code {:?}",
                    plugin_name,
                    hook_name,
                    s.code()
                );
                all_ok = false;
            }
            Err(e) => {
                error!("Failed to execute plugin {}: {}", plugin_name, e);
                all_ok = false;
            }
        }
    }

    all_ok
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"");
    let pos = json.find(&pattern)?;
    let after_key = pos + pattern.len();
    let colon = json[after_key..].find(':')? + after_key;
    let quote1 = json[colon..].find('"')? + colon;
    let quote2 = json[quote1 + 1..].find('"')? + quote1 + 1;
    Some(json[quote1 + 1..quote2].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_hook_names() {
        assert_eq!(PluginHook::PreEncode.name(), "pre_encode");
        assert_eq!(PluginHook::PostEncode.name(), "post_encode");
        assert_eq!(PluginHook::PreWrap.name(), "pre_wrap");
        assert_eq!(PluginHook::PostWrap.name(), "post_wrap");
        assert_eq!(PluginHook::PreValidate.name(), "pre_validate");
        assert_eq!(PluginHook::PostValidate.name(), "post_validate");
        assert_eq!(PluginHook::PreCreate.name(), "pre_create");
        assert_eq!(PluginHook::PostCreate.name(), "post_create");
    }

    #[test]
    fn test_discover_plugins_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let plugins = discover_plugins(tmp.path());
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_discover_plugins_with_manifest() {
        let tmp = TempDir::new().unwrap();
        let plugin_dir = tmp.path().join("my_plugin");
        fs::create_dir(&plugin_dir).unwrap();

        let manifest = r#"{
            "name": "Test Plugin",
            "version": "1.0.0",
            "description": "A test plugin",
            "author": "Test Author"
        }"#;
        fs::write(plugin_dir.join("plugin.json"), manifest).unwrap();

        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "Test Plugin");
        assert_eq!(plugins[0].version, "1.0.0");
        assert_eq!(plugins[0].description, "A test plugin");
        assert_eq!(plugins[0].author, "Test Author");
    }

    #[test]
    fn test_discover_plugins_skips_invalid() {
        let tmp = TempDir::new().unwrap();

        fs::create_dir(tmp.path().join("no_manifest")).unwrap();

        let bad = tmp.path().join("bad_plugin");
        fs::create_dir(&bad).unwrap();
        fs::write(bad.join("plugin.json"), r#"{"name": "", "version": "1.0"}"#).unwrap();

        fs::write(tmp.path().join("not_a_dir.txt"), "hello").unwrap();

        let plugins = discover_plugins(tmp.path());
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_discover_nonexistent_dir() {
        let plugins = discover_plugins(Path::new("/nonexistent/plugins"));
        assert!(plugins.is_empty());
    }

    #[test]
    fn test_execute_hook_no_plugins() {
        let tmp = TempDir::new().unwrap();
        assert!(execute_hook(PluginHook::PreEncode, tmp.path(), "{}"));
    }

    #[test]
    fn test_execute_hook_nonexistent_dir() {
        assert!(execute_hook(
            PluginHook::PreEncode,
            Path::new("/nonexistent"),
            "{}"
        ));
    }

    #[test]
    fn test_execute_hook_success() {
        let tmp = TempDir::new().unwrap();
        let plugin_dir = tmp.path().join("test_plugin");
        let hooks_dir = plugin_dir.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();

        fs::write(hooks_dir.join("pre_encode.py"), "import sys; sys.exit(0)").unwrap();

        assert!(execute_hook(
            PluginHook::PreEncode,
            tmp.path(),
            r#"{"test":true}"#
        ));
    }
}
