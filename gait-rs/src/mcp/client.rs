use crate::config::{McpServerDef, McpToolDef};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{service::RunningService, RoleClient, ServiceExt};
use serde_json::{Map, Value};

pub struct McpClient {
    server_name: String,
    handle: RunningService<RoleClient, ()>,
}

impl McpClient {
    pub async fn connect(def: &McpServerDef) -> anyhow::Result<Self> {
        let handle: RunningService<RoleClient, ()> = if let Some(ref cmd) = def.command {
            // Ecosystem-standard stdio: command + args + env
            let args: &[String] = def.args.as_deref().unwrap_or(&[]);
            connect_stdio(cmd, args, def.env.as_ref(), &def.name, "command").await?
        } else if let Some(ref url) = def.url {
            // Ecosystem-standard HTTP transport
            let transport = StreamableHttpClientTransport::from_uri(url.as_str());
            ().serve(transport)
                .await
                .map_err(|e| anyhow::anyhow!("MCP handshake failed (url {}): {e}", def.name))?
        } else {
            anyhow::bail!("MCP server {} has neither command nor url", def.name);
        };
        Ok(Self {
            server_name: def.name.clone(),
            handle,
        })
    }

    pub async fn discover_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        let tools = self
            .handle
            .list_all_tools()
            .await
            .map_err(|e| anyhow::anyhow!("tools/list failed for {}: {e}", self.server_name))?;
        Ok(tools
            .into_iter()
            .map(|t| McpToolDef {
                server_name: self.server_name.clone(),
                name: t.name.to_string(),
                description: t.description.as_deref().unwrap_or("").to_string(),
                schema: serde_json::Value::Object((*t.input_schema).clone()),
                output_schema: t
                    .output_schema
                    .as_ref()
                    .map(|s| serde_json::Value::Object((**s).clone())),
            })
            .collect())
    }

    pub async fn call_tool(
        &self,
        name: &str,
        args: Value,
        output_schema: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let arguments: Option<Map<String, Value>> = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => anyhow::bail!("MCP tool args must be a JSON object, got: {other}"),
        };
        let mut req = CallToolRequestParams::new(name.to_string());
        if let Some(map) = arguments {
            req = req.with_arguments(map);
        }
        let result = self.handle.call_tool(req).await.map_err(|e| {
            anyhow::anyhow!("tools/call {} on {} failed: {e}", name, self.server_name)
        })?;

        let text: String = result
            .content
            .iter()
            .filter_map(|item| match &**item {
                RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        process_call_tool_result(
            name,
            result.structured_content,
            text,
            result.is_error == Some(true),
            output_schema,
        )
    }
}

fn process_call_tool_result(
    name: &str,
    structured_content: Option<Value>,
    text: String,
    is_error: bool,
    output_schema: Option<&Value>,
) -> anyhow::Result<Value> {
    if let Some(structured) = structured_content {
        if is_error {
            anyhow::bail!("MCP tool {} returned error: {}", name, structured);
        }
        return Ok(structured);
    }
    // Check the tool's own failure first: an error response naturally has no
    // structured_content, and its real message is more useful than a generic
    // contract-violation report. Only treat missing structured_content as a
    // schema contract violation when the call otherwise succeeded.
    if is_error {
        anyhow::bail!("MCP tool {} returned error: {}", name, text);
    }
    if output_schema.is_some() {
        anyhow::bail!(
            "MCP tool {} declares output_schema but returned no structured_content — contract violation",
            name
        );
    }
    Ok(Value::String(text))
}

async fn connect_stdio(
    prog: &str,
    args: &[String],
    env: Option<&std::collections::HashMap<String, String>>,
    name: &str,
    label: &str,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let cmd = stdio_command(prog, args, env);
    let transport =
        TokioChildProcess::new(cmd).map_err(|e| anyhow::anyhow!("MCP spawn {name} failed: {e}"))?;
    ().serve(transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP handshake failed ({label} {name}): {e}"))
}

fn stdio_command(
    prog: &str,
    args: &[String],
    env: Option<&std::collections::HashMap<String, String>>,
) -> tokio::process::Command {
    tokio::process::Command::new(prog).configure(|c| {
        c.args(args);
        c.kill_on_drop(true);
        // MCP servers are third-party tools and carry no causes: never inherit
        // our own trace context. An explicit config entry still wins below.
        c.env_remove("TRACEPARENT");
        c.env_remove("BAGGAGE");
        if let Some(env) = env {
            for (k, v) in env {
                c.env(k, v);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_command_removes_trace_and_baggage_but_config_env_wins() {
        let cmd = stdio_command("x", &[], None);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        for key in ["TRACEPARENT", "BAGGAGE"] {
            let entry = envs
                .iter()
                .find(|(k, _)| *k == std::ffi::OsStr::new(key))
                .expect("removal entry present");
            assert_eq!(entry.1, None, "{key} must be removed");
        }

        let mut map = std::collections::HashMap::new();
        map.insert("BAGGAGE".to_string(), "explicit".to_string());
        let cmd = stdio_command("x", &[], Some(&map));
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        let baggage = envs
            .iter()
            .find(|(k, _)| *k == std::ffi::OsStr::new("BAGGAGE"))
            .expect("BAGGAGE entry present");
        assert_eq!(baggage.1, Some(std::ffi::OsStr::new("explicit")));
        let traceparent = envs
            .iter()
            .find(|(k, _)| *k == std::ffi::OsStr::new("TRACEPARENT"))
            .expect("removal entry present");
        assert_eq!(traceparent.1, None, "TRACEPARENT must stay removed");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stdio_server_sees_neither_trace_var_even_when_gait_has_them() {
        unsafe {
            std::env::set_var("TRACEPARENT", "00-trace-span-01");
            std::env::set_var("BAGGAGE", "key=value");
        }

        let mut cmd = stdio_command(
            "sh",
            &[
                "-c".into(),
                "printf '%s|%s' \"${TRACEPARENT-unset}\" \"${BAGGAGE-unset}\"".into(),
            ],
            None,
        );
        let output = cmd.output().await.expect("child runs");
        unsafe {
            std::env::remove_var("TRACEPARENT");
            std::env::remove_var("BAGGAGE");
        }
        assert_eq!(String::from_utf8_lossy(&output.stdout), "unset|unset");

        unsafe {
            std::env::set_var("TRACEPARENT", "00-trace-span-01");
            std::env::set_var("BAGGAGE", "key=value");
        }
        let mut map = std::collections::HashMap::new();
        map.insert("BAGGAGE".to_string(), "explicit".to_string());
        let mut cmd = stdio_command(
            "sh",
            &[
                "-c".into(),
                "printf '%s|%s' \"${TRACEPARENT-unset}\" \"${BAGGAGE-unset}\"".into(),
            ],
            Some(&map),
        );
        let output = cmd.output().await.expect("child runs");
        unsafe {
            std::env::remove_var("TRACEPARENT");
            std::env::remove_var("BAGGAGE");
        }
        assert_eq!(String::from_utf8_lossy(&output.stdout), "unset|explicit");
    }

    #[test]
    fn declared_output_schema_rejects_missing_structured_content() {
        let schema = serde_json::json!({"type": "object"});
        let error = process_call_tool_result(
            "contract_tool",
            None,
            "text fallback".into(),
            false,
            Some(&schema),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "MCP tool contract_tool declares output_schema but returned no structured_content — contract violation"
        );
    }

    #[test]
    fn tool_error_surfaces_its_own_message_even_with_a_declared_schema() {
        let schema = serde_json::json!({"type": "object"});
        let error = process_call_tool_result(
            "failing_tool",
            None,
            "disk full".into(),
            true,
            Some(&schema),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "MCP tool failing_tool returned error: disk full"
        );
    }

    #[test]
    fn output_schema_preserves_structured_results_and_no_schema_fallback() {
        let schema = serde_json::json!({"type": "object"});
        assert_eq!(
            process_call_tool_result(
                "structured_tool",
                Some(serde_json::json!({"ok": true})),
                "ignored".into(),
                false,
                Some(&schema),
            )
            .unwrap(),
            serde_json::json!({"ok": true})
        );
        assert_eq!(
            process_call_tool_result("text_tool", None, "plain text".into(), false, None).unwrap(),
            Value::String("plain text".into())
        );
    }

    #[tokio::test]
    async fn connect_fails_when_neither_command_nor_url() {
        let def = McpServerDef {
            name: "test".into(),
            command: None,
            args: None,
            env: None,
            url: None,
            enabled: true,
            description: None,
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
            enabled: true,
            description: None,
        };
        assert!(McpClient::connect(&def).await.is_err());
    }

    #[tokio::test]
    async fn connect_fails_neither_command_nor_url() {
        let def = McpServerDef {
            name: "test".into(),
            command: None,
            args: None,
            env: None,
            url: None,
            enabled: true,
            description: None,
        };
        match McpClient::connect(&def).await {
            Err(e) => assert!(e.to_string().contains("neither command"), "got: {e}"),
            Ok(_) => panic!("expected error"),
        }
    }
}
