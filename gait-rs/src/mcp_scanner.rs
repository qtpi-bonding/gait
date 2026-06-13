use std::path::Path;
use crate::config::McpServerDef;

/// Scan `<gait_dir>/mcp/<name>.json` and return one `McpServerDef` per valid file.
///
/// A file is valid if it is parseable JSON and has at least one of: `command`,
/// `spawn`, `url`, or `connect`. The server name is the filename stem (e.g.
/// `mcp/slack.json` → name "slack"). Non-.json files are silently skipped.
pub fn scan_mcp_servers(gait_dir: &Path) -> Vec<McpServerDef> {
    let mcp_dir = gait_dir.join("mcp");
    if !mcp_dir.is_dir() { return vec![]; }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&mcp_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[gait] warning: could not read mcp dir {}: {err}", mcp_dir.display());
            return vec![];
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        match parse_mcp_file(&path, &name) {
            Some(def) => out.push(def),
            None => eprintln!("[gait] warning: skipping invalid mcp file {}", path.display()),
        }
    }
    out
}

fn parse_mcp_file(path: &Path, name: &str) -> Option<McpServerDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = val.as_object()?;

    let command = obj.get("command").and_then(|v| v.as_str()).map(|s| s.to_string());
    let args = obj.get("args").and_then(|v| v.as_array()).map(|a| {
        a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
    });
    let env = obj.get("env").and_then(|v| v.as_object()).map(|m| {
        m.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    });
    let url = obj.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
    let spawn = obj.get("spawn").and_then(|v| v.as_str()).map(|s| s.to_string());
    let connect = obj.get("connect").and_then(|v| v.as_str()).map(|s| s.to_string());

    // Must have at least one connection method
    if command.is_none() && spawn.is_none() && url.is_none() && connect.is_none() {
        return None;
    }

    Some(McpServerDef {
        name: name.to_string(),
        command,
        args,
        env,
        url,
        spawn,
        connect,
        enabled: obj.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        description: obj.get("description").and_then(|v| v.as_str()).map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_mcp(dir: &TempDir, name: &str, content: &str) {
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(mcp_dir.join(format!("{}.json", name)), content).unwrap();
    }

    #[test]
    fn scanner_returns_empty_when_no_mcp_dir() {
        let dir = TempDir::new().unwrap();
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_reads_command_args_env() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "slack", r#"{
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-slack"],
            "env": {"SLACK_TOKEN": "xoxb-test"},
            "enabled": false,
            "description": "Slack tools"
        }"#);
        let servers = scan_mcp_servers(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "slack");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert_eq!(servers[0].args.as_deref(), Some(&["-y".to_string(), "@modelcontextprotocol/server-slack".to_string()][..]));
        assert_eq!(servers[0].env.as_ref().unwrap()["SLACK_TOKEN"], "xoxb-test");
        assert!(!servers[0].enabled);
        assert_eq!(servers[0].description.as_deref(), Some("Slack tools"));
    }

    #[test]
    fn scanner_reads_url_server() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "remote", r#"{"url": "http://localhost:3001/mcp", "enabled": true}"#);
        let servers = scan_mcp_servers(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "remote");
        assert_eq!(servers[0].url.as_deref(), Some("http://localhost:3001/mcp"));
        assert!(servers[0].enabled);
    }

    #[test]
    fn scanner_enabled_defaults_to_true_when_absent() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "noenabled", r#"{"command": "npx", "args": []}"#);
        let servers = scan_mcp_servers(dir.path());
        assert!(servers[0].enabled);
    }

    #[test]
    fn scanner_skips_file_with_no_connection() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "bad", r#"{"description": "no command or url"}"#);
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_invalid_json() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "broken", "{ not valid json");
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_non_json_files() {
        let dir = TempDir::new().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(mcp_dir.join("README.md"), "# docs").unwrap();
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_accepts_legacy_spawn_field() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "legacy", r#"{"spawn": "node mcp.js", "enabled": false}"#);
        let servers = scan_mcp_servers(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "legacy");
        assert_eq!(servers[0].spawn.as_deref(), Some("node mcp.js"));
    }
}
