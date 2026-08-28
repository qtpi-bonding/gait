use std::sync::Arc;
mod error;
pub use error::ToolError;
pub mod tiers;
pub use tiers::{EnabledTools, PermissionedTools, RegisteredTools};

pub mod annotate;
pub mod apply_patch;
pub mod clarify;
pub mod code;
pub mod codesearch;
pub mod fs_backend;
pub mod glob;
pub mod grep;
pub mod inform;
pub mod list_agents;
pub mod load;
pub mod ls;
mod output;
pub mod preview;
pub mod prune;
pub mod read;
pub mod shell;
pub mod spawn_agent;
pub mod structured_done;
pub mod webfetch;
pub mod write;

pub use annotate::AnnotateTool;
pub use clarify::ClarifyTool;
pub use code::CodeTool;
pub use codesearch::CodesearchTool;
pub mod ctx;
pub use apply_patch::ApplyPatchTool;
pub use ctx::ToolCtx;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use inform::InformTool;
pub use list_agents::ListAgentsTool;
pub use load::LoadTool;
pub use ls::LsTool;
#[cfg(test)]
pub(crate) use output::full_output_from_text;
pub(crate) use output::{
    archive_and_prepare_model_value, archive_text, ensure_archived_model_value,
    full_output_for_existing, full_output_ref_from_text, full_output_ref_from_value,
    model_text_for_inline, model_text_for_inline_with_note, prepare_tool_output_path, FullOutput,
    MODEL_TOOL_OUTPUT_MAX_BYTES,
};
pub use prune::PruneTool;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use spawn_agent::SpawnAgentTool;
pub use structured_done::StructuredDoneTool;
pub use webfetch::WebfetchTool;
pub use write::WriteTool;

use crate::config::{McpToolDef, SkillDef};
use crate::types::Tool;

/// Generate a JSON Schema for type T and strip the `$schema` and `title` meta-keys
/// that schemars adds by default. Anthropic's `input_schema` field accepts a plain
/// JSON Schema object — the extra meta-keys cause 400 errors against the live API.
pub fn make_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    // Inline subschemas so the emitted schema is fully self-contained — no
    // `$ref`/`definitions` indirection. Weak OpenAI-compatible models (e.g.
    // MiniMax) mis-handle `$ref` and emit malformed tool arguments for nested
    // types (the `edit` tool — our only `$ref` schema — was the only tool that
    // broke). Inlining is strictly more compatible; the only cost is a slightly
    // larger schema. (Safe for our tool param types — none are recursive.)
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| s.inline_subschemas = true);
    let schema = settings.into_generator().into_root_schema_for::<T>();
    let mut v = serde_json::to_value(&schema).expect("schema serialization");
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("definitions");
    }
    v
}

/// Inject optional filter fields into an existing JSON Schema `properties` map.
/// `Parse` adds a `parse` array field (rg/sd pipeline, applied in-process).
/// `Curate` adds a `curate_prompt` string field (LLM annotation subagent).
/// `Both` adds both fields.
/// `None` returns the schema unchanged.
pub fn inject_filter(
    mut schema: serde_json::Value,
    filter: crate::types::OutputFilter,
) -> serde_json::Value {
    use crate::types::OutputFilter;
    if matches!(filter, OutputFilter::None) {
        return schema;
    }
    if matches!(filter, OutputFilter::Parse | OutputFilter::Both) {
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert("parse".into(), serde_json::json!({
                "type": "array",
                "description": "Optional pipeline of filter operations applied in-process before \
                                the result reaches the model. Operations run in order. \
                                {\"rg\": \"<pattern>\"} keeps only lines matching the regex \
                                (linear-time finite automaton, no backtracking, no ReDoS). \
                                {\"sd\": [\"<from>\", \"<to>\"]} replaces all regex matches of \
                                <from> with <to> (multiline mode).",
                "items": {
                    "type": "object",
                    "description": "One operation: {\"rg\": \"pattern\"} or {\"sd\": [\"from\", \"to\"]}"
                }
            }));
        }
    }
    if matches!(filter, OutputFilter::Curate | OutputFilter::Both) {
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert(
                "curate_prompt".into(),
                serde_json::json!({
                    "type": "string",
                    "description": "Route this tool's output through a 1-turn annotation subagent \
                                    before the model sees it. Describe what counts as relevant. \
                                    Only the relevant items are returned; dropped count is noted."
                }),
            );
        }
    }
    schema
}

/// Coerce whole-number JSON floats to integers so models that emit `50.0`
/// for an integer field don't hit serde's "expected u64, got float" error.
/// Walks the entire value so nested params (e.g. arrays) are also fixed.
pub fn coerce_int_params(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, val)| (k, coerce_int_params(val)))
                .collect(),
        ),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(coerce_int_params).collect())
        }
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                    return serde_json::Value::Number((f as i64).into());
                }
            }
            serde_json::Value::Number(n)
        }
        other => other,
    }
}

/// Schema-guided scalar coercion. First applies the unconditional float->int
/// pass (`coerce_int_params` semantics), then walks `args` alongside `schema`
/// and coerces string-encoded scalars to the declared type — but only when the
/// schema says `integer`/`number`/`boolean`, so genuine string fields (paths,
/// names) are never mangled. Recurses object `properties` and array `items`.
pub fn coerce_scalar_params(
    args: serde_json::Value,
    schema: &serde_json::Value,
) -> serde_json::Value {
    /// True if the schema's `type` (a string OR an array like `["integer","null"]`)
    /// permits any of `want`. Optional fields emit array `type`, so a plain
    /// `as_str()` check would silently skip exactly the fields weak models
    /// stringify — handle both shapes.
    fn type_allows(schema: &serde_json::Value, want: &[&str]) -> bool {
        match schema.get("type") {
            Some(serde_json::Value::String(s)) => want.contains(&s.as_str()),
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .any(|v| v.as_str().is_some_and(|s| want.contains(&s))),
            _ => false,
        }
    }
    fn walk(v: serde_json::Value, schema: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let props = schema.get("properties").and_then(|p| p.as_object());
                serde_json::Value::Object(
                    map.into_iter()
                        .map(|(k, val)| {
                            let child = props.and_then(|p| p.get(&k));
                            let val = match child {
                                Some(cs) => walk(val, cs),
                                None => coerce_int_params(val),
                            };
                            (k, val)
                        })
                        .collect(),
                )
            }
            serde_json::Value::Array(arr) => {
                let items = schema.get("items");
                serde_json::Value::Array(
                    arr.into_iter()
                        .map(|x| match items {
                            Some(is) => walk(x, is),
                            None => coerce_int_params(x),
                        })
                        .collect(),
                )
            }
            serde_json::Value::String(s) => {
                if type_allows(schema, &["integer", "number"]) {
                    serde_json::from_str::<serde_json::Number>(&s)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::String(s))
                } else if type_allows(schema, &["boolean"]) {
                    match s.as_str() {
                        "true" => serde_json::Value::Bool(true),
                        "false" => serde_json::Value::Bool(false),
                        _ => serde_json::Value::String(s),
                    }
                } else {
                    serde_json::Value::String(s)
                }
            }
            other => coerce_int_params(other),
        }
    }
    walk(args, schema)
}

/// Names of all built-in tools. Used for allow-list validation without constructing tool instances.
pub fn builtin_tool_names() -> &'static [&'static str] {
    &[
        "read",
        "write",
        "apply_patch",
        "glob",
        "grep",
        "ls",
        "shell",
        "structured_done",
        "codesearch",
        "webfetch",
        "enable",
        "spawn_agent",
        "list_agents",
        "clarify",
        "inform",
        "annotate",
        "prune",
        "code",
    ]
}

/// Returns all built-in tools.
pub fn builtin_tools(
    ctx: ToolCtx,
    mcp_tools: Vec<McpToolDef>,
    skills: Vec<SkillDef>,
    plugins: Vec<crate::config::WasmPluginDef>,
    repos: crate::git::Repos,
) -> Vec<Arc<dyn Tool>> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        ReadTool::new(ctx.clone()).arced(),
        WriteTool::new(ctx.clone()).arced(),
        ApplyPatchTool::new(ctx.clone()).arced(),
        GlobTool::new(ctx.clone()).arced(),
        GrepTool::new(ctx.clone()).arced(),
        LsTool::new(ctx.clone()).arced(),
        ShellTool::new(ctx.clone()).arced(),
        StructuredDoneTool::new().arced(),
        CodesearchTool::new(ctx.clone()).arced(),
        {
            let wf = if let Some(ref hosts) = ctx.network_allowlist {
                WebfetchTool::new().with_network_allowlist(hosts.clone())
            } else {
                WebfetchTool::new()
            };
            wf.arced()
        },
        LoadTool::new(mcp_tools, skills, plugins).arced(),
        SpawnAgentTool::new(ctx.clone(), repos.clone()).arced(),
        ListAgentsTool::new(ctx.clone()).arced(),
        ClarifyTool::new().arced(),
        InformTool::new().arced(),
        PruneTool::new().arced(),
        CodeTool::new().arced(),
    ];
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use std::path::PathBuf;

    #[test]
    fn builtin_tools_returns_seventeen() {
        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );
        assert_eq!(tools.len(), 17);
    }

    #[test]
    fn all_tool_names_unique() {
        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate tool names found");
    }

    #[test]
    fn only_unbounded_builtin_outputs_opt_into_spooling() {
        use crate::types::OutputCaptureMode;

        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );

        for tool in tools {
            let expected = if matches!(tool.name(), "shell" | "grep") {
                OutputCaptureMode::Spool
            } else {
                OutputCaptureMode::Returned
            };
            assert_eq!(
                tool.output_capture_mode(),
                expected,
                "unexpected output capture declaration for {}",
                tool.name()
            );
        }
    }

    #[test]
    fn spawn_agent_and_list_agents_present() {
        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(
            names.contains(&"spawn_agent"),
            "missing spawn_agent: {:?}",
            names
        );
        assert!(
            names.contains(&"list_agents"),
            "missing list_agents: {:?}",
            names
        );
    }

    #[test]
    fn checkpoint_gated_tools_stay_serial() {
        // Per-tool-call checkpointing (see docs/superpowers/plans/2026-07-25-
        // per-tool-call-checkpointing-plan.md §3) relies on shell/write/
        // apply_patch never running concurrently with themselves — two
        // parallel checkpoint() calls against the same shadow repo's HEAD
        // would race. This test guards that invariant against silent
        // regression; it should already be true today.
        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );
        for name in ["shell", "write", "apply_patch"] {
            let tool = tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("tool {name} not found in builtin_tools"));
            assert_eq!(
                tool.execution_mode(),
                crate::types::ExecutionMode::Serial,
                "checkpoint enrichment assumes {name} never runs concurrently with itself"
            );
        }
    }

    #[test]
    fn enable_meta_tool_present() {
        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"enable"), "missing enable: {:?}", names);
    }

    #[test]
    fn inject_filter_parse_adds_array_field() {
        use crate::types::OutputFilter;
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "cmd": { "type": "string" } }
        });
        let result = inject_filter(schema, OutputFilter::Parse);
        let parse_field = &result["properties"]["parse"];
        assert!(parse_field.is_object(), "parse field missing: {result}");
        assert_eq!(parse_field["type"].as_str().unwrap(), "array");
    }

    #[test]
    fn inject_filter_curate_adds_curate_prompt_field() {
        use crate::types::OutputFilter;
        let schema = serde_json::json!({ "type": "object", "properties": {} });
        let result = inject_filter(schema, OutputFilter::Curate);
        assert!(result["properties"]["curate_prompt"].is_object());
        assert_eq!(
            result["properties"]["curate_prompt"]["type"]
                .as_str()
                .unwrap(),
            "string"
        );
    }

    #[test]
    fn inject_filter_none_is_noop() {
        use crate::types::OutputFilter;
        let schema = serde_json::json!({ "type": "object", "properties": {} });
        let result = inject_filter(schema, OutputFilter::None);
        let props = result["properties"].as_object().unwrap();
        assert!(!props.contains_key("parse"));
        assert!(!props.contains_key("curate_prompt"));
    }

    #[test]
    fn inject_filter_both_adds_parse_and_curate_prompt() {
        use crate::types::OutputFilter;
        let schema =
            serde_json::json!({ "type": "object", "properties": { "cmd": { "type": "string" } } });
        let result = inject_filter(schema, OutputFilter::Both);
        let props = result["properties"].as_object().unwrap();
        assert!(props.contains_key("parse"), "both should add parse");
        assert!(
            props.contains_key("curate_prompt"),
            "both should add curate_prompt"
        );
        assert_eq!(props["parse"]["type"].as_str().unwrap(), "array");
        assert_eq!(props["curate_prompt"]["type"].as_str().unwrap(), "string");
    }

    #[test]
    fn builtin_tool_names_has_no_run_workflow() {
        assert!(
            !builtin_tool_names().contains(&"run_workflow"),
            "run_workflow must be gone: {:?}",
            builtin_tool_names()
        );
    }

    #[tokio::test]
    async fn builtin_tools_wires_network_allowlist_to_webfetch() {
        let ctx = ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into())
            .with_network_allowlist(vec!["github.com".to_string()]);
        let tools = builtin_tools(ctx, vec![], vec![], vec![], crate::git::Repos::production());

        let webfetch = tools
            .iter()
            .find(|t| t.name() == "webfetch")
            .expect("webfetch tool not found");
        let result = webfetch
            .execute(
                serde_json::json!({"url": "https://example.com/"}),
                &crate::Recorder::detached(),
            )
            .await;

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("egress denied"),
            "allowlist not applied: {err}"
        );
    }

    #[test]
    fn world_touching_tools_declare_access() {
        use crate::types::ToolAccess;
        let tools = builtin_tools(
            ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "stest".into()),
            vec![],
            vec![],
            vec![],
            crate::git::Repos::production(),
        );
        let by = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().access();

        assert!(
            by("read").touches_world()
                && by("read")
                    == ToolAccess {
                        reads: true,
                        ..Default::default()
                    }
        );
        assert_eq!(
            by("write"),
            ToolAccess {
                reads: true,
                writes: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by("apply_patch"),
            ToolAccess {
                reads: true,
                writes: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by("shell"),
            ToolAccess {
                reads: true,
                writes: true,
                spawns: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by("webfetch"),
            ToolAccess {
                network: true,
                ..Default::default()
            }
        );
        for n in ["glob", "grep", "ls", "codesearch"] {
            assert_eq!(
                by(n),
                ToolAccess {
                    reads: true,
                    ..Default::default()
                },
                "{n}"
            );
        }
        // Control tools stay brain-side.
        for n in [
            "structured_done",
            "spawn_agent",
            "clarify",
            "inform",
            "prune",
        ] {
            assert!(!by(n).touches_world(), "{n} must be brain-side");
        }
    }

    #[test]
    fn coerce_scalar_parses_stringified_integer_when_schema_says_integer() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"limit": {"type": "integer"}}
        });
        let args = serde_json::json!({"limit": "50"});
        let out = coerce_scalar_params(args, &schema);
        assert_eq!(out["limit"], serde_json::json!(50));
    }

    #[test]
    fn coerce_scalar_leaves_string_field_alone() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}}
        });
        let args = serde_json::json!({"path": "123"});
        let out = coerce_scalar_params(args, &schema);
        assert_eq!(out["path"], serde_json::json!("123")); // NOT coerced to number
    }

    #[test]
    fn coerce_scalar_parses_stringified_bool() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"detached": {"type": "boolean"}}
        });
        let args = serde_json::json!({"detached": "true"});
        let out = coerce_scalar_params(args, &schema);
        assert_eq!(out["detached"], serde_json::json!(true));
    }

    #[test]
    fn coerce_scalar_recurses_array_items() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "rows": {"type": "array", "items": {
                    "type": "object", "properties": {"n": {"type": "integer"}}
                }}
            }
        });
        let args = serde_json::json!({"rows": [{"n": "7"}]});
        let out = coerce_scalar_params(args, &schema);
        assert_eq!(out["rows"][0]["n"], serde_json::json!(7));
    }

    #[test]
    fn coerce_scalar_handles_optional_field_with_array_type() {
        // optional integer -> schemars emits type: ["integer","null"]
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"limit": {"type": ["integer", "null"]}}
        });
        let args = serde_json::json!({"limit": "50"});
        let out = coerce_scalar_params(args, &schema);
        assert_eq!(out["limit"], serde_json::json!(50));
    }

    #[test]
    fn coerce_scalar_still_floors_float_to_int() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"limit": {"type": "integer"}}
        });
        let args = serde_json::json!({"limit": 50.0});
        let out = coerce_scalar_params(args, &schema);
        assert_eq!(out["limit"], serde_json::json!(50));
    }
}
