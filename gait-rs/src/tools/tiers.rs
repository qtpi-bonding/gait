use crate::types::Tool;

/// All tools loaded into the registry for this session (tier 1: register/unregister).
///
/// Built-in tools are always registered. MCP tools are registered when their
/// server connects successfully during profile resolution.
pub struct RegisteredTools(pub Vec<Box<dyn Tool>>);

/// Registered tools that are currently visible to the LLM (tier 2: enable/disable).
///
/// Note: enable/disable state is partially dynamic — the in-session `load` tool
/// lets the LLM toggle MCP servers and skills mid-turn, tracked via `AgentState`.
/// `RegisteredTools::into_enabled` is currently a pass-through; the `config_enabled`
/// seed on `ResolvedProfile` handles the static portion, and `AgentState` handles
/// the dynamic portion during a turn.
pub struct EnabledTools(pub Vec<Box<dyn Tool>>);

/// Enabled tools with their permission rules attached (tier 3: allow/deny).
///
/// This is what `run_turn` receives. The `allow`/`deny` lists are checked per
/// call at execution time: allow → runs without prompting, deny → blocked,
/// neither → gates on user approval.
pub struct PermissionedTools {
    pub tools: EnabledTools,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl RegisteredTools {
    /// Pass all registered tools through as enabled.
    /// Dynamic enable/disable is handled by `AgentState` during turn execution.
    pub fn into_enabled(self) -> EnabledTools {
        EnabledTools(self.0)
    }
}

impl EnabledTools {
    pub fn with_permissions(self, allow: Vec<String>, deny: Vec<String>) -> PermissionedTools {
        PermissionedTools {
            tools: self,
            allow,
            deny,
        }
    }

    pub fn as_slice(&self) -> &[Box<dyn Tool>] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<Box<dyn Tool>> {
        self.0
    }

    /// Mutable access to the underlying tool vec. Needed by
    /// `PermissionedTools::push_tool`; `pub(crate)` because callers outside the
    /// crate should go through `PermissionedTools::push_tool` instead of
    /// reaching past the permission tier.
    pub(crate) fn as_slice_mut(&mut self) -> &mut Vec<Box<dyn Tool>> {
        &mut self.0
    }
}

impl PermissionedTools {
    pub fn tools(&self) -> &[Box<dyn Tool>] {
        self.tools.as_slice()
    }

    pub fn into_parts(self) -> (Vec<Box<dyn Tool>>, Vec<String>, Vec<String>) {
        (self.tools.into_vec(), self.allow, self.deny)
    }

    /// Append an extra tool to the registered set. Intended for embedders
    /// (e.g. `gait-repl`) that want to inject a host-specific tool that is
    /// always available, without rebuilding the whole `Profile`. The new tool
    /// is still subject to the existing `allow` / `deny` rules — callers who
    /// need it auto-allowed should also extend `self.allow`.
    pub fn push_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.as_slice_mut().push(tool);
    }
}
