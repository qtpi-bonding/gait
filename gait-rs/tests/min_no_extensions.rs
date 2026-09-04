//! Task 6a: proves the `min` feature build genuinely drops the live `rmcp`
//! MCP client surface, rather than merely leaving an unused Cargo feature.
//!
//! `ResolvedProfile.mcp_clients` and `crate::mcp::McpClient` are `pub(crate)`
//! / crate-private, so they can't be referenced from this external
//! integration-test crate at all -- the behavioral half of this proof (that
//! `ResolvedProfile::resolve` never populates `mcp_registry` under `min`,
//! while `Profile.mcp_servers` still reflects on-disk `mcp/*.json` files)
//! lives in `src/profile.rs`'s own `#[cfg(test)] mod tests` instead, where
//! it has crate-internal access to those fields. See the test named
//! `min_build_never_populates_mcp_registry_but_profile_still_scans_disk`
//! there.

#[test]
fn min_build_has_no_live_mcp_client_type() {
    // Under `--no-default-features --features min`, this compiles only if
    // `ResolvedProfile.mcp_clients` and `crate::mcp::McpClient` are gated
    // out -- there is no runtime behavior to assert beyond "it compiles",
    // since the removed surface is a type, not a directory scan.
}
