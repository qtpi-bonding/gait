use std::sync::Arc;
use serde_json::{Value, Map};
use crate::config::{McpServerDef, McpToolDef};
use rmcp::{ServiceExt, RoleClient, service::RunningService};
use rmcp::transport::{TokioChildProcess, ConfigureCommandExt, StreamableHttpClientTransport};
use rmcp::model::{CallToolRequestParams, RawContent};

pub struct McpClient {
    server_name: String,
    handle: RunningService<RoleClient, ()>,
}

impl McpClient {
    pub async fn connect(def: &McpServerDef) -> anyhow::Result<Self> {
        let handle: RunningService<RoleClient, ()> = if let Some(ref cmd) = def.command {
            // Ecosystem-standard stdio: command + args + env
            let args: &[String] = def.args.as_deref().unwrap_or(&[]);
            let transport = TokioChildProcess::new(
                tokio::process::Command::new(cmd).configure(|c| {
                    c.args(args);
                    if let Some(env) = &def.env {
                        for (k, v) in env { c.env(k, v); }
                    }
                })
            ).map_err(|e| anyhow::anyhow!("MCP spawn {} failed: {e}", def.name))?;
            ().serve(transport).await
                .map_err(|e| anyhow::anyhow!("MCP handshake failed (command {}): {e}", def.name))?
        } else if let Some(ref cmd_str) = def.spawn {
            // Compat: split "prog arg1 arg2" into command + args
            let mut parts = cmd_str.split_whitespace();
            let prog = parts.next()
                .ok_or_else(|| anyhow::anyhow!("empty spawn command for MCP server {}", def.name))?;
            let rest: Vec<&str> = parts.collect();
            let transport = TokioChildProcess::new(
                tokio::process::Command::new(prog).configure(|cmd| { cmd.args(&rest); })
            ).map_err(|e| anyhow::anyhow!("MCP spawn {} failed: {e}", def.name))?;
            ().serve(transport).await
                .map_err(|e| anyhow::anyhow!("MCP handshake failed (spawn {}): {e}", def.name))?
        } else if let Some(ref url) = def.url {
            // Ecosystem-standard HTTP transport
            let transport = StreamableHttpClientTransport::from_uri(url.as_str());
            ().serve(transport).await
                .map_err(|e| anyhow::anyhow!("MCP handshake failed (url {}): {e}", def.name))?
        } else if let Some(ref url) = def.connect {
            // Compat: connect → url
            let transport = StreamableHttpClientTransport::from_uri(url.as_str());
            ().serve(transport).await
                .map_err(|e| anyhow::anyhow!("MCP handshake failed (connect {}): {e}", def.name))?
        } else {
            anyhow::bail!("MCP server {} has neither command, spawn, url, nor connect", def.name);
        };
        Ok(Self { server_name: def.name.clone(), handle })
    }

    pub async fn discover_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        let tools = self.handle.list_all_tools().await
            .map_err(|e| anyhow::anyhow!("tools/list failed for {}: {e}", self.server_name))?;
        Ok(tools.into_iter().map(|t| McpToolDef {
            server_name: self.server_name.clone(),
            name: t.name.to_string(),
            description: t.description.as_deref().unwrap_or("").to_string(),
            schema: serde_json::Value::Object((*t.input_schema).clone()),
        }).collect())
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let arguments: Option<Map<String, Value>> = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => anyhow::bail!("MCP tool args must be a JSON object, got: {other}"),
        };
        let mut req = CallToolRequestParams::new(name.to_string());
        if let Some(map) = arguments {
            req = req.with_arguments(map);
        }
        let result = self.handle.call_tool(req).await
            .map_err(|e| anyhow::anyhow!("tools/call {} on {} failed: {e}", name, self.server_name))?;

        let text: String = result.content.iter()
            .filter_map(|item| match &**item {
                RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error == Some(true) {
            anyhow::bail!("MCP tool {} returned error: {}", name, text);
        }
        Ok(Value::String(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_fails_gracefully_when_binary_missing() {
        let def = McpServerDef {
            name: "test".into(),
            command: None,
            args: None,
            env: None,
            url: None,
            spawn: Some("no-such-binary-xyz-abc --foo".into()),
            connect: None,
            enabled: true,
            description: None,
        };
        let result = McpClient::connect(&def).await;
        assert!(result.is_err(), "expected error for missing binary");
    }

    #[tokio::test]
    async fn connect_fails_gracefully_when_url_unreachable() {
        let def = McpServerDef {
            name: "test".into(),
            command: None,
            args: None,
            env: None,
            url: None,
            spawn: None,
            connect: Some("http://127.0.0.1:19191/mcp".into()),
            enabled: true,
            description: None,
        };
        let result = McpClient::connect(&def).await;
        assert!(result.is_err(), "expected error for unreachable URL");
    }

    #[tokio::test]
    async fn connect_fails_when_neither_spawn_nor_connect() {
        let def = McpServerDef {
            name: "test".into(), command: None, args: None, env: None,
            url: None, spawn: None, connect: None, enabled: true, description: None,
        };
        let result = McpClient::connect(&def).await;
        match result {
            Err(e) => assert!(e.to_string().contains("neither command"), "got: {e}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn connect_fails_gracefully_with_command_missing_binary() {
        let def = McpServerDef {
            name: "test".into(),
            command: Some("no-such-binary-xyz-abc".into()),
            args: Some(vec!["--foo".into()]),
            env: None,
            url: None,
            spawn: None,
            connect: None,
            enabled: true,
            description: None,
        };
        assert!(McpClient::connect(&def).await.is_err());
    }

    #[tokio::test]
    async fn connect_fails_neither_command_nor_url() {
        let def = McpServerDef {
            name: "test".into(), command: None, args: None, env: None,
            url: None, spawn: None, connect: None, enabled: true, description: None,
        };
        match McpClient::connect(&def).await {
            Err(e) => assert!(e.to_string().contains("neither command"), "got: {e}"),
            Ok(_) => panic!("expected error"),
        }
    }
}
