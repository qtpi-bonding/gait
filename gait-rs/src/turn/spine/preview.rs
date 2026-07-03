//! Spine-side permission/stream enrichment (the brain can't read disk).

use std::path::Path;

use crate::config::SandboxConfig;

/// Spine-side preview: for a `Permissions` event, fill any empty `preview` on
/// edit/write items by running that tool's own `preview()` in-process. The spine
/// is unsealed and already opens the codebase, so this is where the diff read
/// belongs (the brain stays read-free). Scope-bounded: a target outside the
/// tool's read ceiling yields no preview rather than reading an arbitrary file.
pub(crate) fn enrich_permissions_preview(
    event: &mut crate::proto::gait::log::v1::Event,
    codebase: &Path,
    gait_dir: &Path,
    session_id: &str,
    sandbox_cfg: &SandboxConfig,
) {
    use crate::proto::gait::log::v1::{event, payload};

    let Some(event::Body::Payload(p)) = event.body.as_mut() else {
        return;
    };
    let Some(payload::Kind::Permissions(perm)) = p.kind.as_mut() else {
        return;
    };

    let ctx = crate::tools::ToolCtx::new(
        codebase.to_path_buf(),
        gait_dir.to_path_buf(),
        session_id.to_string(),
    );
    let tools =
        crate::tools::builtin_tools(ctx, vec![], vec![], vec![], crate::git::Repos::production());

    // compute_scope uses the jail only for path arithmetic (pushes it into
    // readable/writable vecs); it never calls canonicalize or metadata on it.
    // A stable sentinel is therefore sufficient — no temp directory needed.
    let jail = std::env::temp_dir().join("gait-preview-jail");

    for item in perm.items.iter_mut() {
        if item.preview.is_some() {
            continue; // already filled (e.g. non-relay path)
        }
        let Some(tool) = tools.iter().find(|t| t.name() == item.tool_name) else {
            continue;
        };
        // Convert tool_args (proto Struct) back to serde_json::Value for the tool API.
        let args: serde_json::Value = item
            .tool_args
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null);

        // Scope-bound the read: only preview targets within the tool's read ceiling.
        let ceiling =
            crate::sandbox::ceiling::compute_scope(tool.access(), sandbox_cfg, codebase, &jail);
        let targets = tool.scope_targets(&args);
        // Empty targets → no declared scope; treat as out-of-scope (skip preview).
        let in_scope = !targets.is_empty()
            && targets.iter().all(|(path, _dim)| {
                // Canonicalize the target path so /var/... and /private/var/... compare equal.
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                ceiling.read_all
                    || ceiling.readable.iter().any(|r| canonical.starts_with(r))
                    || ceiling.writable.iter().any(|w| canonical.starts_with(w))
            });
        if !in_scope {
            continue;
        }

        if let Some(diff) = tool.preview(&args) {
            item.preview = Some(diff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_permissions_event(
        items: Vec<crate::proto::gait::log::v1::PermissionItem>,
    ) -> crate::proto::gait::log::v1::Event {
        use crate::proto::gait::log::v1::{event, payload, Payload, Permissions};
        gait_proto::envelope::mint(
            event::Body::Payload(Payload {
                kind: Some(payload::Kind::Permissions(Permissions { items })),
            }),
            None,
            0,
            0,
        )
    }

    /// An in-scope write target: preview should be filled.
    #[test]
    fn enrich_fills_preview_for_in_scope_write() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();
        let target_file = codebase.join("hello.txt");
        std::fs::write(&target_file, "old content\n").unwrap();

        let item = PermissionItem {
            id: "req-1".into(),
            cmd: "write hello.txt".into(),
            reason: "write requires permission".into(),
            patterns: vec!["write:hello.txt".into()],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": target_file.to_string_lossy(),
                "content": "new content\n"
            })),
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions");
        };
        assert_eq!(perm.items.len(), 1);
        assert!(
            perm.items[0].preview.is_some(),
            "expected preview to be filled for in-scope write"
        );
    }

    /// A target outside the codebase: preview should stay None.
    #[test]
    fn enrich_skips_preview_for_out_of_scope_target() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();

        // Point the write outside the codebase (e.g. /etc/passwd)
        let item = PermissionItem {
            id: "req-2".into(),
            cmd: "write /etc/passwd".into(),
            reason: "write requires permission".into(),
            patterns: vec!["write:/etc/passwd".into()],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": "/etc/passwd",
                "content": "malicious"
            })),
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions");
        };
        assert!(
            perm.items[0].preview.is_none(),
            "expected preview to be None for out-of-scope target"
        );
    }

    /// A tool name that doesn't exist in builtin_tools → find returns None → continue →
    /// preview stays None. Exercises the empty-targets short-circuit guard.
    #[test]
    fn enrich_skips_preview_when_no_targets() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();

        // Use a tool name that doesn't exist in builtin_tools so the find() returns None
        // and the loop continues without touching preview.
        let item = PermissionItem {
            id: "req-no-targets".into(),
            cmd: "nonexistent_tool".into(),
            reason: "some reason".into(),
            patterns: vec![],
            tool_name: "nonexistent_tool_xyz".into(),
            tool_args: None,
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions kind");
        };
        assert!(
            perm.items[0].preview.is_none(),
            "expected preview to remain None when tool has no targets"
        );
    }

    /// Already-filled preview should not be overwritten.
    #[test]
    fn enrich_does_not_overwrite_existing_preview() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();
        let target_file = codebase.join("existing.txt");
        std::fs::write(&target_file, "content\n").unwrap();

        let item = PermissionItem {
            id: "req-3".into(),
            cmd: "write existing.txt".into(),
            reason: "write requires permission".into(),
            patterns: vec![],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": target_file.to_string_lossy(),
                "content": "new\n"
            })),
            preview: Some("--- brain computed this ---".into()),
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body in overwrite test");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions kind in overwrite test");
        };
        assert_eq!(
            perm.items[0].preview.as_deref(),
            Some("--- brain computed this ---"),
            "existing preview must not be overwritten"
        );
    }
}
