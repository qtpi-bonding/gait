use crate::agent_frontmatter::split_frontmatter;
use crate::config::WasmPluginDef;
use std::path::Path;

/// Scan `<gait_dir>/plugins/<name>/PLUGIN.md` and return one `WasmPluginDef` per valid file.
///
/// A file is valid if it has a non-empty `name` and `wasm` key in its YAML frontmatter.
/// The `wasm` field is a path relative to the plugin directory; the scanner resolves it
/// to an absolute path and stores that in `WasmPluginDef.path`.
/// `enabled` defaults to `false` (plugins start disabled until explicitly enabled).
pub fn scan_plugins(gait_dir: &Path) -> Vec<WasmPluginDef> {
    let plugins_dir = gait_dir.join("plugins");
    if !plugins_dir.is_dir() {
        return vec![];
    }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&plugins_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!(
                "[gait] warning: could not read plugins dir {}: {err}",
                plugins_dir.display()
            );
            return vec![];
        }
    };
    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        let plugin_md = plugin_dir.join("PLUGIN.md");
        if !plugin_md.is_file() {
            continue;
        }
        match parse_plugin_file(&plugin_md, &plugin_dir) {
            Some(def) => out.push(def),
            None => eprintln!(
                "[gait] warning: skipping invalid plugin file {}",
                plugin_md.display()
            ),
        }
    }
    out
}

fn parse_plugin_file(path: &Path, plugin_dir: &Path) -> Option<WasmPluginDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() {
        return None;
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).ok()?;
    let name = yaml
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let wasm_rel = yaml
        .get("wasm")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let wasm_abs = plugin_dir.join(wasm_rel);
    let allow_exec = yaml
        .get("allow_exec")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let allow_fs = yaml
        .get("allow_fs")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Some(WasmPluginDef {
        name,
        path: wasm_abs.to_string_lossy().to_string(),
        enabled: yaml
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        allow_exec,
        allow_fs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_plugin(dir: &TempDir, name: &str, fm: &str, body: &str) {
        let plugin_dir = dir.path().join("plugins").join(name);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        // Create a dummy .wasm file so path resolution works
        std::fs::write(plugin_dir.join(format!("{}.wasm", name)), b"").unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            format!("---\n{}---\n{}", fm, body),
        )
        .unwrap();
    }

    #[test]
    fn scanner_returns_empty_when_no_plugins_dir() {
        let dir = TempDir::new().unwrap();
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn scanner_reads_one_plugin() {
        let dir = TempDir::new().unwrap();
        write_plugin(
            &dir,
            "formatter",
            "name: formatter\nwasm: formatter.wasm\nenabled: false\nallow_exec: []\nallow_fs:\n  - \"read:src/\"\n",
            "# Formatter\nFormats code.\n",
        );
        let plugins = scan_plugins(dir.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "formatter");
        assert!(
            plugins[0].path.ends_with("formatter.wasm"),
            "path: {}",
            plugins[0].path
        );
        assert!(!plugins[0].enabled);
        assert_eq!(plugins[0].allow_fs, vec!["read:src/"]);
        assert!(plugins[0].allow_exec.is_empty());
    }

    #[test]
    fn scanner_skips_dir_without_plugin_md() {
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("plugins").join("orphan");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("README.md"), "hi").unwrap();
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_plugin_with_no_name() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "bad", "wasm: bad.wasm\nenabled: false\n", "body\n");
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_plugin_with_no_wasm_field() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "nowasm", "name: nowasm\nenabled: false\n", "body\n");
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn enabled_defaults_to_false_when_absent() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "plug", "name: plug\nwasm: plug.wasm\n", "body\n");
        let plugins = scan_plugins(dir.path());
        assert!(!plugins[0].enabled, "plugins default to disabled");
    }

    #[test]
    fn allow_exec_and_fs_default_to_empty() {
        let dir = TempDir::new().unwrap();
        write_plugin(
            &dir,
            "minimal",
            "name: minimal\nwasm: minimal.wasm\nenabled: false\n",
            "# Minimal\n",
        );
        let plugins = scan_plugins(dir.path());
        assert!(plugins[0].allow_exec.is_empty());
        assert!(plugins[0].allow_fs.is_empty());
    }
}
