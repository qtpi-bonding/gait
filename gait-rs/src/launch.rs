//! Typed self-invocation argv builders (spec-00-footguns.md item 4).
//!
//! The `gait` binary launches itself in six places, each of which used to
//! hand-assemble a `Vec<String>` of `gait …` flags. Any one of those sites
//! could drift from what the corresponding parser actually reads and the
//! mismatch would only surface at *runtime*, inside a spawned (often sealed)
//! child process. This module centralizes argv construction into one typed
//! builder per role, with a `to_argv()` that a round-trip test (`build →
//! to_argv → parse → assert-equal`) can check against the real parser — so a
//! renamed/added flag becomes a compile or test failure instead.
//!
//! Two parsers live outside this crate (`RunArgs` in `gait-cli`, since
//! gait-core cannot depend on gait-cli — see [`crate::is_role_argv`]'s doc
//! comment for the same DAG rule). [`RunLaunch`] is therefore verified by a
//! round-trip test in `gait-cli` (`crates/gait-cli/src/cli/run.rs`), not
//! here; [`BrainLaunch`] and [`DaemonExitLaunch`] round-trip against parsers
//! that *do* live in gait-core ([`BrainArgs`] below, and
//! [`crate::daemon_exit::DaemonExitArgs`]), so those tests live in this file.
//!
//! Note: [`BrainLaunch`] here is distinct from [`crate::embed::BrainLaunch`],
//! which is a higher-level "what the driver needs to spawn a turn" struct
//! that eventually calls [`BrainLaunch::to_argv`] (via `brain_argv()` in
//! `turn/spine.rs`) to get the wire argv. Neither module glob-imports the
//! other, so the shared name does not collide in practice.

use std::path::PathBuf;

// ── RunLaunch (sites 1, 3, 4, 5 — `RunArgs` in gait-cli) ────────────────────

/// Flags for `gait run` / the bare-positional `gait "task"` entry, parsed by
/// `RunArgs` (`crates/gait-cli/src/cli/run.rs`).
///
/// `to_argv()` produces exactly the flags `RunArgs::parse_from` expects
/// (`#[command(no_binary_name = true)]` — no leading `"run"` token). Site 1
/// (`gait-bin/src/main.rs`, the bare-positional path) calls `cmd_run`
/// in-process with `to_argv()` directly. Sites 3/4/5 spawn a brand-new `gait`
/// process, so they need the `"run"` role token that `main.rs` strips before
/// forwarding to `cmd_run` — use [`RunLaunch::to_process_argv`] for that.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunLaunch {
    pub session: Option<String>,
    pub task: Option<String>,
    pub auto: bool,
    pub model_override: Option<String>,
    pub agent_name: Option<String>,
    pub result_schema_path: Option<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub unlock: Vec<String>,
    pub max_iter: Option<usize>,
    pub sandbox: bool,
    pub no_sandbox: bool,
    pub sandbox_locked: bool,
    pub cascade_group: Option<String>,
    pub gait_dir: Option<PathBuf>,
    pub codebase: Option<PathBuf>,
    /// Override for the in-tree safety guard (design §7 / Task 5), sourced from
    /// `--allow-intree`. Never sourced from config — see `embed::BrainLaunch::allow_intree`.
    pub allow_intree: bool,
}

impl RunLaunch {
    /// Flags only — what `RunArgs::parse_from` expects. Order is irrelevant
    /// to clap (named flags, not positionals) so this does not attempt to
    /// match any one call site's historical ordering.
    pub fn to_argv(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(t) = &self.task {
            a.extend(["--task".to_string(), t.clone()]);
        }
        if let Some(s) = &self.session {
            a.extend(["--session".to_string(), s.clone()]);
        }
        if self.auto {
            a.push("--auto".to_string());
        }
        if let Some(m) = &self.model_override {
            a.extend(["--model".to_string(), m.clone()]);
        }
        if let Some(ag) = &self.agent_name {
            a.extend(["--agent".to_string(), ag.clone()]);
        }
        for p in &self.allow {
            a.extend(["--allow".to_string(), p.clone()]);
        }
        for p in &self.deny {
            a.extend(["--deny".to_string(), p.clone()]);
        }
        for p in &self.unlock {
            a.extend(["--unlock".to_string(), p.clone()]);
        }
        if let Some(n) = self.max_iter {
            a.extend(["--max-iter".to_string(), n.to_string()]);
        }
        if let Some(rs) = &self.result_schema_path {
            a.extend(["--result-schema".to_string(), rs.clone()]);
        }
        if self.sandbox {
            a.push("--sandbox".to_string());
        }
        if self.no_sandbox {
            a.push("--no-sandbox".to_string());
        }
        if self.sandbox_locked {
            a.push("--sandbox-locked".to_string());
        }
        if let Some(cg) = &self.cascade_group {
            a.extend(["--cascade-group".to_string(), cg.clone()]);
        }
        if let Some(g) = &self.gait_dir {
            a.extend(["--gait-dir".to_string(), g.to_string_lossy().into_owned()]);
        }
        if let Some(c) = &self.codebase {
            a.extend(["--codebase".to_string(), c.to_string_lossy().into_owned()]);
        }
        if self.allow_intree {
            a.push("--allow-intree".to_string());
        }
        a
    }

    /// argv for spawning a brand-new `gait` process: `["run", ...to_argv()]`.
    /// `main.rs` strips the `"run"` role token and hands the rest to
    /// `cmd_run`/`RunArgs::parse_from`.
    pub fn to_process_argv(&self) -> Vec<String> {
        let mut a = vec!["run".to_string()];
        a.extend(self.to_argv());
        a
    }
}

// ── WakeLaunch (site 4 — `session/completion.rs::spawn_wake`) ──────────────

/// Thin convenience over [`RunLaunch`] for `spawn_wake`'s
/// `gait run --auto --session <parent> --gait-dir <dir>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeLaunch {
    pub gait_dir: PathBuf,
    pub parent_session: String,
}

impl WakeLaunch {
    fn as_run_launch(&self) -> RunLaunch {
        RunLaunch {
            session: Some(self.parent_session.clone()),
            auto: true,
            gait_dir: Some(self.gait_dir.clone()),
            ..Default::default()
        }
    }

    pub fn to_argv(&self) -> Vec<String> {
        self.as_run_launch().to_argv()
    }

    pub fn to_process_argv(&self) -> Vec<String> {
        self.as_run_launch().to_process_argv()
    }
}

// ── BrainLaunch (sites 2, 3 — the local `BrainArgs` clap struct below) ──────

/// Build the argv passed to `gait __brain` — the single source of truth for
/// the brain's command line. The `"__brain"` role token itself is prepended
/// by the caller's spawn (`relay_brain_turn` in `turn/spine.rs`); no consumer
/// constructs any of this by hand.
///
/// Only flags the brain actually reads from argv are emitted: `--gait-dir`,
/// `--codebase`, `--session`, `--auto`, `--max-iter`, `--allow`, `--deny`,
/// and the sandbox flag. The profile (model/agent/result-schema) reaches the
/// brain via the `BrainInit` frame, not argv — see `brain_argv`'s original
/// doc comment in `turn/spine.rs`, preserved there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrainLaunch {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub session_id: String,
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub sandbox_on: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

impl BrainLaunch {
    /// Order matches the pre-refactor `brain_argv()` exactly: `--gait-dir`,
    /// `--codebase`, `--session` come first (a caller-side test asserts on
    /// `argv[0]`/`argv[1]` positionally), then `--auto`, `--max-iter`,
    /// `--allow`*, `--deny`*, and finally the sandbox flag.
    pub fn to_argv(&self) -> Vec<String> {
        let mut a = vec![
            "--gait-dir".to_string(),
            self.gait_dir.to_string_lossy().into_owned(),
            "--codebase".to_string(),
            self.codebase.to_string_lossy().into_owned(),
            "--session".to_string(),
            self.session_id.clone(),
        ];
        if self.auto {
            a.push("--auto".to_string());
        }
        if let Some(n) = self.max_iter {
            a.push("--max-iter".to_string());
            a.push(n.to_string());
        }
        for p in &self.allow {
            a.push("--allow".to_string());
            a.push(p.clone());
        }
        for p in &self.deny {
            a.push("--deny".to_string());
            a.push(p.clone());
        }
        // Sealed spine ⇒ sealed brain (and vice versa); the brain never
        // escapes a sandboxed parent and is never sandboxed under an
        // unsandboxed parent.
        a.push(
            if self.sandbox_on {
                "--sandbox-locked"
            } else {
                "--no-sandbox"
            }
            .to_string(),
        );
        a
    }
}

/// Parse side for `__brain` argv (`argv[1..]` when `argv[0] == "__brain"`).
/// Hoisted out of `__dispatch_role`'s match arm (was a function-local struct)
/// so [`BrainLaunch`]'s round-trip test can call it directly; behavior is
/// unchanged — same fields, same flags, still `no_binary_name = true`.
#[derive(clap::Parser, Debug, PartialEq, Eq)]
#[command(no_binary_name = true)]
pub struct BrainArgs {
    #[arg(long = "session")]
    pub session_id: Option<String>,
    #[arg(long)]
    pub auto: bool,
    #[arg(long = "model")]
    pub model_override: Option<String>,
    #[arg(long = "agent")]
    pub agent_name: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub allow: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub deny: Vec<String>,
    #[arg(long)]
    pub max_iter: Option<usize>,
    #[arg(long)]
    pub sandbox: bool,
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,
    #[arg(long = "sandbox-locked", hide = true)]
    pub sandbox_locked: bool,
    #[arg(long, hide = true)]
    pub gait_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub codebase: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub task: Option<String>,
    #[arg(long = "result-schema", hide = true)]
    pub result_schema_path: Option<String>,
    #[arg(long = "cascade-group", hide = true)]
    pub cascade_group: Option<String>,
}

// ── DaemonExitLaunch (site 6 — `tools/shell/mod.rs` daemon wrapper) ─────────

/// Args for the hidden `__daemon-exit` subcommand
/// (`gait __daemon-exit --gait-dir <dir> --session <parent> --name <name>
/// --exit <code>`), hand-parsed by [`crate::daemon_exit::DaemonExitArgs`].
///
/// Site 6 (the shell tool's detached-daemon wrapper) must stay a `sh -c`
/// fragment: it runs the user's command, captures `$?`, *then* fires this
/// hook — that composition requires a shell, not a plain argv array. `--exit`
/// is `$?` at shell runtime, so it is not a field here: [`Self::to_argv_prefix`]
/// stops before it, and the caller (`to_shell_fragment`) appends it as the
/// literal shell variable reference `$__ec`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonExitLaunch {
    pub gait_dir: PathBuf,
    pub session: String,
    pub name: String,
}

impl DaemonExitLaunch {
    /// argv for the hidden subcommand, stopping before `--exit` (that value
    /// is `$?` captured at shell runtime by the caller).
    pub fn to_argv_prefix(&self) -> Vec<String> {
        vec![
            "--gait-dir".to_string(),
            self.gait_dir.to_string_lossy().into_owned(),
            "--session".to_string(),
            self.session.clone(),
            "--name".to_string(),
            self.name.clone(),
        ]
    }

    /// The `sh -c` fragment for the detached-daemon wrapper: runs `user_cmd`,
    /// captures its exit code, then fires `__daemon-exit` with the quoted
    /// argv from [`Self::to_argv_prefix`] plus `--exit $__ec`. Every
    /// interpolated value (not the flag names) is routed through
    /// `tools::shell::shell_quote` so a value containing spaces/quotes
    /// survives the shell round-trip — same quoting discipline the wrapper
    /// used before this was centralized.
    pub fn to_shell_fragment(&self, exe: &str, user_cmd: &str) -> String {
        use crate::tools::shell::shell_quote;
        let mut argv_str = String::new();
        for tok in self.to_argv_prefix() {
            if tok.starts_with("--") {
                argv_str.push_str(&tok);
            } else {
                argv_str.push_str(&shell_quote(&tok));
            }
            argv_str.push(' ');
        }
        format!(
            "{user}\n__ec=$?\n{exe} __daemon-exit {argv}--exit $__ec\n",
            user = user_cmd,
            exe = shell_quote(exe),
            argv = argv_str,
        )
    }
}

// ── Round-trip tests ─────────────────────────────────────────────────────
//
// The safety-critical part: for each role, `build → to_argv → parse via the
// real parser → assert fields equal`. This is what converts a future flag
// rename from a runtime failure in a spawned (often sealed) role into a
// compile or test failure. `RunLaunch`'s round-trip lives in
// `crates/gait-cli/src/cli/run.rs` — `RunArgs` is defined there and
// gait-core cannot depend on gait-cli.

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn brain_launch_round_trips_through_brain_args() {
        let launch = BrainLaunch {
            gait_dir: PathBuf::from("/g"),
            codebase: PathBuf::from("/c"),
            session_id: "s123".to_string(),
            auto: true,
            max_iter: Some(7),
            sandbox_on: true,
            allow: vec!["Bash".to_string()],
            deny: vec!["Net".to_string()],
        };
        let argv = launch.to_argv();
        // The role token is added by the spawn, never by the builder.
        assert!(!argv.iter().any(|a| a == "__brain"));

        let parsed = BrainArgs::parse_from(&argv);
        assert_eq!(parsed.session_id.as_deref(), Some("s123"));
        assert!(parsed.auto);
        assert_eq!(parsed.max_iter, Some(7));
        assert_eq!(parsed.allow, vec!["Bash".to_string()]);
        assert_eq!(parsed.deny, vec!["Net".to_string()]);
        assert!(parsed.sandbox_locked);
        assert!(!parsed.no_sandbox);
        // model/agent/result-schema flow via BrainInit, NOT argv.
        assert_eq!(parsed.model_override, None);
        assert_eq!(parsed.agent_name, None);
    }

    #[test]
    fn brain_launch_unsandboxed_emits_no_sandbox() {
        let launch = BrainLaunch {
            gait_dir: PathBuf::from("/g"),
            codebase: PathBuf::from("/c"),
            session_id: "s".to_string(),
            auto: false,
            max_iter: None,
            sandbox_on: false,
            allow: vec![],
            deny: vec![],
        };
        let argv = launch.to_argv();
        assert!(argv.iter().any(|a| a == "--no-sandbox"));
        assert!(!argv.iter().any(|a| a == "--sandbox-locked"));
        assert!(!argv.iter().any(|a| a == "--auto"));
        assert!(!argv.iter().any(|a| a == "--max-iter"));

        let parsed = BrainArgs::parse_from(&argv);
        assert!(parsed.no_sandbox);
        assert!(!parsed.sandbox_locked);
        assert!(!parsed.auto);
        assert_eq!(parsed.max_iter, None);
    }

    #[test]
    fn brain_launch_argv_is_canonically_ordered() {
        // A caller-side test in turn/spine.rs asserts on argv[0]/argv[1]
        // positionally; pin that ordering here too so a reorder is caught
        // in both places.
        let launch = BrainLaunch {
            gait_dir: PathBuf::from("/g"),
            codebase: PathBuf::from("/c"),
            session_id: "s".to_string(),
            auto: false,
            max_iter: None,
            sandbox_on: false,
            allow: vec![],
            deny: vec![],
        };
        let argv = launch.to_argv();
        assert_eq!(argv[0], "--gait-dir");
        assert_eq!(argv[1], "/g");
    }

    #[test]
    fn daemon_exit_launch_round_trips_through_daemon_exit_args() {
        let launch = DaemonExitLaunch {
            gait_dir: PathBuf::from("/gd"),
            session: "parent1".to_string(),
            name: "daemon-abcd1234".to_string(),
        };
        let mut argv = launch.to_argv_prefix();
        argv.extend(["--exit".to_string(), "0".to_string()]);

        let parsed = crate::daemon_exit::DaemonExitArgs::parse_from(&argv);
        assert_eq!(parsed.gait_dir, PathBuf::from("/gd"));
        assert_eq!(parsed.session, "parent1");
        assert_eq!(parsed.name, "daemon-abcd1234");
        assert_eq!(parsed.exit, 0);
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes_in_the_fragment() {
        let launch = DaemonExitLaunch {
            gait_dir: PathBuf::from("/tmp/my gait dir"),
            session: "session with spaces".to_string(),
            name: "it's-a-daemon".to_string(),
        };
        let fragment = launch.to_shell_fragment("/usr/local/bin/gait", "echo hi");

        // Every interpolated value is single-quoted; embedded single quotes
        // are escaped via the POSIX '\'' sequence (see shell_quote).
        assert!(fragment.contains("'/tmp/my gait dir'"));
        assert!(fragment.contains("'session with spaces'"));
        assert!(fragment.contains(r"'it'\''s-a-daemon'"));
        assert!(fragment.contains("__daemon-exit --gait-dir"));
        assert!(fragment.contains("--exit $__ec"));
        assert!(fragment.starts_with("echo hi\n__ec=$?\n"));

        // Round-trip through an actual shell: the quoted values must survive
        // word-splitting when `sh -c` re-tokenizes the fragment. We can't
        // easily intercept the real __daemon-exit dispatch here, so instead
        // assert the shell's own re-tokenization (via `printf '%s\n' "$@"`
        // driven from the same quoted argv string) recovers the original
        // values byte-for-byte.
        let quoted_only = fragment
            .lines()
            .nth(2)
            .expect("third line is the __daemon-exit invocation");
        // Strip the leading `<exe> __daemon-exit ` and trailing `--exit $__ec`.
        let inner = quoted_only
            .trim_start_matches("'/usr/local/bin/gait' __daemon-exit ")
            .trim_end_matches("--exit $__ec")
            .trim();
        let script = format!("set -- {inner}; printf '%s\\n' \"$@\"");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sh available");
        let recovered = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = recovered.lines().collect();
        assert_eq!(
            lines,
            vec![
                "--gait-dir",
                "/tmp/my gait dir",
                "--session",
                "session with spaces",
                "--name",
                "it's-a-daemon",
            ]
        );
    }
}
