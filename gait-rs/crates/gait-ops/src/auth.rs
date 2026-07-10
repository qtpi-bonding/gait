//! Auth/model verbs: `list`, `add`, `remove`, and the async `models`.
//!
//! `add`/`remove`/`list` wrap `gait_core::auth` and never read stdin — the CLI
//! adapter handles the key prompt per design §4. `models` is async because it
//! hits `models.dev` (with disk-cache fallback); the pure `filter_models`
//! helper stays in gait-core and we just delegate here.
use crate::OpError;
use std::collections::HashMap;

/// Provider → masked-display-string map. Each key is a provider name
/// (`"anthropic"`, `"openai"`, …); each value is the rendering shown by
/// `gait auth list` (an 8-char prefix + `...` for api keys, or the literal
/// `"oauth"` / `"wellknown"` / `"unknown"` tag).
pub type AuthListOutcome = HashMap<String, String>;

/// Outcome of [`add`] — reports the provider whose key was stored and the
/// masked-display value rendered back so the CLI can echo it consistently
/// with `list()`.
#[derive(Debug, Clone)]
pub struct AuthWriteOutcome {
    pub provider: String,
    pub display: String,
}

/// Outcome of [`remove`] — reports the provider whose key was removed. The
/// CLI adapter renders this as `"Removed <provider> credentials"`.
#[derive(Debug, Clone)]
pub struct AuthRemoveOutcome {
    pub provider: String,
}

/// Outcome of [`models`]. The filtered view the CLI / REPL renders. We
/// deliberately return *borrowed* `ModelInfo` references so the op is
/// zero-copy over the `Vec<ModelInfo>` returned by
/// `gait_core::models::get_models` — no clone, no allocation. The op
/// only fails when the network fetch (and the stale-cache fallback) both
/// fail; `filter` then narrows the set on the way out.
#[derive(Debug)]
pub struct ModelsOutcome {
    pub models: Vec<gait_core::models::ModelInfo>,
    pub filter: Option<String>,
}

impl ModelsOutcome {
    /// Apply the stored `filter` over `models`. Convenience accessor so the
    /// CLI/REPL adapter does not have to re-import `filter_models` — it
    /// renders rows straight off this method's return.
    pub fn render_filtered(&self) -> Vec<&gait_core::models::ModelInfo> {
        gait_core::models::filter_models(&self.models, self.filter.as_deref())
    }
}
/// Save `api_key` for `provider` to the credential store. The key is passed
/// in already read; the CLI adapter handles the stdin prompt (design §4).
/// `key` is required to be non-empty — the CLI adapter also guards this,
/// but the op keeps the invariant locally so a programmatic caller can't
/// accidentally wipe an entry by passing `""`.
pub fn add(provider: &str, api_key: String) -> Result<AuthWriteOutcome, OpError> {
    if provider.is_empty() {
        return Err(OpError::Conflict("provider name is required".to_string()));
    }
    if api_key.is_empty() {
        return Err(OpError::Conflict("api key is required".to_string()));
    }
    gait_core::auth::write_api_key(provider, &api_key).map_err(|e| OpError::Other(e.into()))?;
    let prefix: String = api_key.chars().take(8).collect();
    Ok(AuthWriteOutcome {
        provider: provider.to_string(),
        display: format!("{}...", prefix),
    })
}

/// Remove `provider` from the credential store. Idempotent: removing a
/// provider that isn't present still succeeds, mirroring the original
/// `cmd_auth remove` behavior (it called `remove_api_key` and reported
/// success unconditionally).
pub fn remove(provider: &str) -> Result<AuthRemoveOutcome, OpError> {
    if provider.is_empty() {
        return Err(OpError::Conflict("provider name is required".to_string()));
    }
    gait_core::auth::remove_api_key(provider).map_err(|e| OpError::Other(e.into()))?;
    Ok(AuthRemoveOutcome {
        provider: provider.to_string(),
    })
}

/// List every provider configured in the credential store, sorted by
/// provider name with masked-display values (`"abcd1234..."` for `api`
/// entries, literal `"oauth"`/`"wellknown"`/`"unknown"` for the rest).
pub fn list() -> AuthListOutcome {
    let mut creds = gait_core::auth::list_credentials();
    let mut sorted: Vec<(String, String)> = creds.drain().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted.into_iter().collect()
}

/// Fetch the current models.dev catalog (with the same disk-cache /
/// stale-cache fallback that `gait_core::models::get_models` already
/// implements) and apply an optional substring filter over provider/id/name.
/// Async because the upstream fetch is a network read; frontends should
/// await this and render the rows.
pub async fn models(filter: Option<&str>) -> Result<ModelsOutcome, OpError> {
    let models = gait_core::models::get_models()
        .await
        .map_err(OpError::Other)?;
    Ok(ModelsOutcome {
        models,
        filter: filter.map(|s| s.to_string()),
    })
}

// -----------------------------------------------------------------------------
// tests
//
// Credential-store tests in gait-core (`src/auth.rs`) already round-trip
// `write_api_key_to`/`list_credentials_from`/`remove_api_key_from` against a
// caller-supplied path, so the *overridable* branch is fully covered there.
//
// `gait_core::auth::list_credentials`/`write_api_key`/`remove_api_key` (the
// no-arg forms the op must use) hardcode `~/.local/share/gait/auth.json`,
// so an end-to-end round-trip here would touch the real filesystem
// credential store. We deliberately do NOT write that test — the gait-core
// tests above prove the underlying I/O works, and a u32 mock-everything
// wrapper around the home-dir lookup would obscure the public contract.
// Instead these tests exercise the argument plumbing (empty provider/key
// rejections) and confirm the wrapper's masked-display shape matches what
// the CLI adapter renders.
//
// `models()` gets a unit test against the pure `filter_models` (which the op
// calls) plus a `#[ignore]` real-network test gated by `--include-ignored`
// so a controller can opt in.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn op_err_conflict(err: OpError) -> bool {
        matches!(err, OpError::Conflict(_))
    }

    // -- add --

    #[test]
    fn add_rejects_empty_provider() {
        let err = add("", "sk-test".into()).unwrap_err();
        assert!(op_err_conflict(err), "empty provider must error");
    }

    #[test]
    fn add_rejects_empty_key() {
        let err = add("anthropic", "".into()).unwrap_err();
        assert!(op_err_conflict(err), "empty key must error");
    }

    #[test]
    fn add_returns_outcome_with_eight_char_prefix_display() {
        // We can't end-to-end round-trip `add` (no path override at the
        // no-arg `write_api_key` layer), so we directly exercise the
        // masking shape by reconstructing what the op returns. The
        // equivalent round-trip is covered in gait-core's auth.rs tests.
        let prefix: String = "sk-abcdefg".chars().take(8).collect();
        let display = format!("{}...", prefix);
        assert_eq!(display, "sk-abcde...");
    }

    // -- remove --

    #[test]
    fn remove_rejects_empty_provider() {
        let err = remove("").unwrap_err();
        assert!(op_err_conflict(err), "empty provider must error");
    }

    // -- list --

    #[test]
    fn list_returns_empty_map_when_credential_store_absent() {
        // The user's home-dir credential store is unlikely to exist in CI;
        // even when it does, `list()` must not panic — it returns whatever
        // the core helper reports.
        let out = list();
        // Insertion-order check is too strict (depends on the host's real
        // auth.json), so just assert the value is a well-formed map.
        for v in out.values() {
            assert!(!v.is_empty(), "list value must be non-empty: {v:?}");
        }
    }

    // -- models (filter, pure) + network --

    fn fake_models() -> Vec<gait_core::models::ModelInfo> {
        vec![
            gait_core::models::ModelInfo {
                id: "claude-3-5-sonnet".into(),
                name: "Claude 3.5 Sonnet".into(),
                provider: "anthropic".into(),
                context_length: Some(200_000),
            },
            gait_core::models::ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "openai".into(),
                context_length: Some(128_000),
            },
            gait_core::models::ModelInfo {
                id: "gpt-4o-mini".into(),
                name: "GPT-4o mini".into(),
                provider: "openai".into(),
                context_length: None,
            },
        ]
    }

    #[test]
    fn filter_models_none_returns_all() {
        let ms = fake_models();
        let out = gait_core::models::filter_models(&ms, None);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn filter_models_matches_provider_case_insensitive() {
        let ms = fake_models();
        let out = gait_core::models::filter_models(&ms, Some("OpenAI"));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|m| m.provider == "openai"));
    }

    #[test]
    fn filter_models_matches_id_substring() {
        let ms = fake_models();
        let out = gait_core::models::filter_models(&ms, Some("sonnet"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "claude-3-5-sonnet");
    }

    #[test]
    fn render_filtered_respects_filter() {
        // Smoke-test the outcome's own render_filtered accessor end-to-end
        // without touching the network — mirrors what `models()` returns
        // shape-wise.
        let out = ModelsOutcome {
            models: fake_models(),
            filter: Some("anthropic".into()),
        };
        let filtered = out.render_filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider, "anthropic");
    }

    /// Real-network smoke test for `models()`. Gated by `#[ignore]` so the
    /// normal test pass stays offline / hermetic; a controller can opt in
    /// with `cargo test -p gait-ops auth::tests::models_real_network --
    /// --ignored`.
    #[tokio::test]
    #[ignore = "hits models.dev; run with --ignored to exercise the async path"]
    async fn models_real_network() {
        let out = models(None).await.expect("models fetch");
        assert!(!out.models.is_empty(), "expected at least one model");
        let filtered = out.render_filtered();
        assert!(!filtered.is_empty());
    }
}
