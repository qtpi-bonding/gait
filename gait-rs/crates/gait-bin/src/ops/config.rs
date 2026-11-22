//! Config ops: read and write one key of the gait directory's `gait.jsonc`,
//! or show the effective config. Thin over `gait_ops::config`.
use super::{infra, GaitContext};
use op_registry::{CallOptions, OpParams, Ref};

output! {
    /// A config key and its value.
    pub struct ConfigEntry {
        pub key: String,
        /// The value as stored; null when the key is known but not set.
        pub value: Option<serde_json::Value>,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct ConfigKey {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "key", id_only)]
    pub id: Ref,
}

#[derive(Debug, Clone, OpParams)]
pub struct ConfigSetParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "key", id_only, absent_ok)]
    pub id: Ref,
    /// The value: JSON (a number, boolean, array or object) or plain text.
    #[op(allow_hyphen)]
    pub value: String,
}

#[derive(Debug, Clone, OpParams)]
pub struct ConfigShowParams {}

/// Parse a `config set` value: JSON first, else the text itself, so
/// `anthropic/claude-opus-4` needs no quoting.
fn parse_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

/// Read one config key; a known key that is not set is null, an unknown key is not found.
pub async fn get(
    ctx: &GaitContext,
    params: ConfigKey,
    _opts: CallOptions,
) -> anyhow::Result<ConfigEntry> {
    let key = params.id.as_raw();
    let value = gait_ops::config::get(&ctx.gait_dir()?, key).map_err(infra)?;
    Ok(ConfigEntry {
        key: key.to_owned(),
        value,
    })
}

/// Set one config key, keeping the file's comments and every other key.
pub async fn set(
    ctx: &GaitContext,
    params: ConfigSetParams,
    _opts: CallOptions,
) -> anyhow::Result<ConfigEntry> {
    let key = params.id.as_raw();
    let value = parse_value(&params.value);
    gait_ops::config::set(&ctx.gait_dir()?, key, value.clone()).map_err(infra)?;
    Ok(ConfigEntry {
        key: key.to_owned(),
        value: Some(value),
    })
}

/// Show the effective config: defaults and `gait.jsonc`, with the skills and MCP servers found on disk.
pub async fn show(
    ctx: &GaitContext,
    _params: ConfigShowParams,
    _opts: CallOptions,
) -> anyhow::Result<serde_json::Value> {
    let dir = ctx.gait_dir()?;
    gait_ops::config::resolve(&dir, &dir).map_err(infra)
}
