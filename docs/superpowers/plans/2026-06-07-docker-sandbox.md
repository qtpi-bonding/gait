---
status: draft
---

# Docker Sandbox Wrapper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `gait` binary auto-route through a Docker container (with rg, fd, git pre-installed) when Docker is available, falling back to native with a warning when it isn't, with `--native` to bypass.

**Architecture:** A new `docker` module handles detection, mount mapping, and Docker arg construction. The existing `gait.rs` binary entry point checks at startup: if already inside the container (via `GAIT_CONTAINER=1`) or `--native` flag, run native; if Docker is available, rewrite `--workspace`/`--codebase` to container paths and `exec` Docker; otherwise warn and run native. A multi-stage Dockerfile builds the image with rg/fd/git bundled. Same binary serves as both wrapper and inner gait — no separate crate needed.

**Tech Stack:** Rust, `serde`/`serde_json` (already in Cargo.toml), `dirs-next` (already in Cargo.toml), `std::io::IsTerminal` (stable since Rust 1.70), Docker CLI.

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `gait-rs/src/docker.rs` | Create | Detection, `MountMap`, arg rewriting, Docker command building |
| `gait-rs/src/lib.rs` | Modify | expose `pub mod docker` |
| `gait-rs/src/bin/gait.rs` | Modify | `try_docker_route` + `--native` check at top of `main` |
| `Dockerfile` | Create | Multi-stage build: Rust → debian-slim with rg/fd/git |

---

## Task 1: `docker` module — detection

**Files:**
- Create: `gait-rs/src/docker.rs`
- Test: `gait-rs/src/docker.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write failing tests**

```rust
// bottom of gait-rs/src/docker.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_in_container_when_env_unset() {
        // Only assert when the env var is definitely absent (not running inside the container)
        if std::env::var("GAIT_CONTAINER").is_err() {
            assert!(!is_inside_container());
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test docker 2>&1 | tail -10
```

Expected: compile error — module not found.

- [ ] **Step 3: Implement detection functions**

Create `gait-rs/src/docker.rs`:

```rust
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// True when running inside the gait Docker container (GAIT_CONTAINER=1 is set by the image).
pub fn is_inside_container() -> bool {
    std::env::var("GAIT_CONTAINER").as_deref() == Ok("1")
}

/// True if `docker` is on PATH and the daemon is responsive.
pub fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True if the named image exists locally (no pull needed).
pub fn image_present(image: &str) -> bool {
    std::process::Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pull the image from the registry. Streams progress to stderr.
pub fn pull_image(image: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("docker")
        .args(["pull", image])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("docker pull {} failed", image)
    }
}
```

- [ ] **Step 4: Add module to `lib.rs`**

In `gait-rs/src/lib.rs`, add after `pub mod shell_history;`:

```rust
pub mod docker;
```

- [ ] **Step 5: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test docker::tests 2>&1 | tail -10
```

Expected: `1 passed`.

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/docker.rs src/lib.rs && git commit -m "feat: add docker detection module"
```

---

## Task 2: Mount map + arg rewriting + Docker command builder

**Files:**
- Modify: `gait-rs/src/docker.rs`

- [ ] **Step 1: Write failing tests**

Add to the `#[cfg(test)]` block in `docker.rs`:

```rust
    use std::path::PathBuf;

    #[test]
    fn build_mount_map_uses_fixed_container_paths() {
        let ws = PathBuf::from("/host/myproject");
        let cb = PathBuf::from("/host/myproject/src");
        let m = build_mount_map(&ws, &cb);
        assert_eq!(m.workspace_host, PathBuf::from("/host/myproject"));
        assert_eq!(m.workspace_container, PathBuf::from("/gait-workspace"));
        assert_eq!(m.codebase_host, PathBuf::from("/host/myproject/src"));
        assert_eq!(m.codebase_container, PathBuf::from("/gait-codebase"));
    }

    #[test]
    fn build_docker_args_rewrites_workspace_and_codebase() {
        let mounts = build_mount_map(
            &PathBuf::from("/host/ws"),
            &PathBuf::from("/host/cb"),
        );
        let orig = vec![
            "run".to_string(),
            "--workspace".to_string(), "/host/ws".to_string(),
            "--codebase".to_string(), "/host/cb".to_string(),
            "--task".to_string(), "do stuff".to_string(),
        ];
        let args = build_docker_args("gaitai/gait:latest", &mounts, None, &orig, false);
        // --workspace in inner args should be container path
        let ws_idx = args.iter().rposition(|a| a == "--workspace").unwrap();
        assert_eq!(args[ws_idx + 1], "/gait-workspace");
        let cb_idx = args.iter().rposition(|a| a == "--codebase").unwrap();
        assert_eq!(args[cb_idx + 1], "/gait-codebase");
    }

    #[test]
    fn build_docker_args_injects_paths_when_absent() {
        let mounts = build_mount_map(
            &PathBuf::from("/host/ws"),
            &PathBuf::from("/host/ws"),
        );
        let orig = vec!["run".to_string(), "--task".to_string(), "hello".to_string()];
        let args = build_docker_args("gaitai/gait:latest", &mounts, None, &orig, false);
        assert!(args.iter().any(|a| a == "--workspace"));
        assert!(args.iter().any(|a| a == "--codebase"));
    }

    #[test]
    fn build_docker_args_includes_image_and_volumes() {
        let mounts = build_mount_map(
            &PathBuf::from("/host/ws"),
            &PathBuf::from("/host/cb"),
        );
        let args = build_docker_args("gaitai/gait:latest", &mounts, None, &[], false);
        assert!(args.contains(&"gaitai/gait:latest".to_string()));
        // Two -v flags (workspace + codebase are different)
        assert_eq!(args.iter().filter(|a| *a == "-v").count(), 2);
    }

    #[test]
    fn build_docker_args_single_volume_when_same_path() {
        let mounts = build_mount_map(
            &PathBuf::from("/host/ws"),
            &PathBuf::from("/host/ws"),
        );
        let args = build_docker_args("gaitai/gait:latest", &mounts, None, &[], false);
        // Only one -v flag when workspace == codebase
        assert_eq!(args.iter().filter(|a| *a == "-v").count(), 1);
    }

    #[test]
    fn build_docker_args_strips_native_flag() {
        let mounts = build_mount_map(
            &PathBuf::from("/host/ws"),
            &PathBuf::from("/host/ws"),
        );
        let orig = vec!["run".to_string(), "--native".to_string(), "--task".to_string(), "hi".to_string()];
        let args = build_docker_args("gaitai/gait:latest", &mounts, None, &orig, false);
        assert!(!args.contains(&"--native".to_string()));
    }

    #[test]
    fn build_docker_args_adds_it_when_interactive() {
        let mounts = build_mount_map(&PathBuf::from("/h/w"), &PathBuf::from("/h/w"));
        let args_tty = build_docker_args("img", &mounts, None, &[], true);
        let args_no = build_docker_args("img", &mounts, None, &[], false);
        assert!(args_tty.contains(&"-it".to_string()));
        assert!(!args_no.contains(&"-it".to_string()));
    }
```

- [ ] **Step 2: Run to verify failure**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test docker::tests 2>&1 | tail -10
```

Expected: compile error — `build_mount_map`, `build_docker_args` not defined.

- [ ] **Step 3: Implement `MountMap`, `build_mount_map`, `write_mount_map`, `build_docker_args`**

Add to `gait-rs/src/docker.rs` (after the detection functions):

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct MountMap {
    pub workspace_host: PathBuf,
    pub workspace_container: PathBuf,
    pub codebase_host: PathBuf,
    pub codebase_container: PathBuf,
}

pub fn build_mount_map(workspace: &Path, codebase: &Path) -> MountMap {
    MountMap {
        workspace_host: workspace.to_path_buf(),
        workspace_container: PathBuf::from("/gait-workspace"),
        codebase_host: codebase.to_path_buf(),
        codebase_container: PathBuf::from("/gait-codebase"),
    }
}

/// Write host↔container path mapping to <workspace>/tmp/docker-mounts.json.
pub fn write_mount_map(workspace: &Path, map: &MountMap) -> anyhow::Result<()> {
    let tmp = workspace.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let json = serde_json::to_string_pretty(map)?;
    std::fs::write(tmp.join("docker-mounts.json"), json)?;
    Ok(())
}

/// Build the full argument list for `docker run`.
/// `interactive`: whether to pass `-it` (true when stdin is a TTY).
pub fn build_docker_args(
    image: &str,
    mounts: &MountMap,
    auth_dir: Option<&Path>,
    original_args: &[String],
    interactive: bool,
) -> Vec<String> {
    let mut args = vec!["run".to_string(), "--rm".to_string()];

    if interactive {
        args.push("-it".to_string());
    }

    // Workspace mount
    args.extend([
        "-v".to_string(),
        format!("{}:{}", mounts.workspace_host.display(), mounts.workspace_container.display()),
    ]);

    // Codebase mount only if it differs from workspace
    if mounts.codebase_host != mounts.workspace_host {
        args.extend([
            "-v".to_string(),
            format!("{}:{}", mounts.codebase_host.display(), mounts.codebase_container.display()),
        ]);
    }

    // Auth dir — mount read-only at the Linux data_dir path inside the container
    if let Some(auth) = auth_dir {
        if auth.exists() {
            args.extend([
                "-v".to_string(),
                format!("{}:/root/.local/share/gait:ro", auth.display()),
            ]);
        }
    }

    args.push(image.to_string());

    // Rewrite inner args: strip --workspace, --codebase, --native; inject container paths
    args.extend(rewrite_inner_args(original_args, mounts));

    args
}

fn rewrite_inner_args(args: &[String], mounts: &MountMap) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "--codebase" => {
                i += 2; // strip flag + value
            }
            "--native" => {
                i += 1; // strip flag (no value)
            }
            _ => {
                out.push(args[i].clone());
                i += 1;
            }
        }
    }
    // Always inject container paths
    out.push("--workspace".to_string());
    out.push(mounts.workspace_container.to_string_lossy().to_string());
    out.push("--codebase".to_string());
    out.push(mounts.codebase_container.to_string_lossy().to_string());
    out
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test docker::tests 2>&1 | tail -10
```

Expected: `9 passed`.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/docker.rs && git commit -m "feat: add docker mount map and arg rewriting"
```

---

## Task 3: Wire `try_docker_route` into `gait.rs`

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

- [ ] **Step 1: Write failing tests for `extract_path_arg`**

Add a `#[cfg(test)]` block at the bottom of `gait-rs/src/bin/gait.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_arg_finds_value() {
        let args = vec![
            "run".to_string(),
            "--workspace".to_string(),
            "/foo/bar".to_string(),
        ];
        assert_eq!(
            extract_path_arg(&args, "--workspace"),
            Some(std::path::PathBuf::from("/foo/bar"))
        );
    }

    #[test]
    fn extract_path_arg_returns_none_when_missing() {
        let args = vec!["run".to_string(), "--task".to_string(), "hi".to_string()];
        assert_eq!(extract_path_arg(&args, "--workspace"), None);
    }

    #[test]
    fn extract_path_arg_returns_none_for_empty() {
        assert_eq!(extract_path_arg(&[], "--workspace"), None);
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 'bin' 2>&1 | tail -10
```

Expected: compile error — `extract_path_arg` not defined.

- [ ] **Step 3: Add `extract_path_arg` and `try_docker_route` to `gait.rs`**

Add these two functions before `fn usage()` in `gait-rs/src/bin/gait.rs`:

```rust
fn extract_path_arg(args: &[String], flag: &str) -> Option<std::path::PathBuf> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| std::path::PathBuf::from(&w[1]))
}

fn try_docker_route(orig_args: &[String]) {
    use std::io::IsTerminal;
    const IMAGE: &str = "gaitai/gait:latest";

    if !gait_ai::docker::docker_available() {
        eprintln!("[gait] Docker not available — running without sandbox. Install Docker for isolated execution.");
        return;
    }

    if !gait_ai::docker::image_present(IMAGE) {
        eprintln!("[gait] pulling {}...", IMAGE);
        if let Err(e) = gait_ai::docker::pull_image(IMAGE) {
            eprintln!("[gait] pull failed: {e} — running without sandbox.");
            return;
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace = extract_path_arg(orig_args, "--workspace")
        .unwrap_or_else(|| cwd.clone())
        .canonicalize()
        .unwrap_or_else(|_| cwd.clone());
    let codebase = extract_path_arg(orig_args, "--codebase")
        .unwrap_or_else(|| workspace.clone())
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    let mounts = gait_ai::docker::build_mount_map(&workspace, &codebase);
    let _ = gait_ai::docker::write_mount_map(&workspace, &mounts);

    let auth_dir = dirs_next::data_dir().map(|d| d.join("gait"));
    let interactive = std::io::stdin().is_terminal();
    let docker_args = gait_ai::docker::build_docker_args(
        IMAGE,
        &mounts,
        auth_dir.as_deref(),
        orig_args,
        interactive,
    );

    let status = std::process::Command::new("docker")
        .args(&docker_args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("[gait] docker exec failed: {e}");
            std::process::exit(1);
        });

    std::process::exit(status.code().unwrap_or(1));
}
```

- [ ] **Step 4: Add the Docker routing check at the top of `main`**

In `gait-rs/src/bin/gait.rs`, find the start of `async fn main()`:

```rust
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }

    match args[1].as_str() {
```

Replace with:

```rust
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }

    // Auto-route through Docker unless: already inside container, or --native flag present.
    // --native must appear after the subcommand (e.g. `gait run --native`), not at position 1.
    let has_native = args[2..].iter().any(|a| a == "--native");
    if !has_native && !gait_ai::docker::is_inside_container() {
        try_docker_route(&args[1..]); // calls process::exit if Docker succeeds
    }

    match args[1].as_str() {
```

- [ ] **Step 5: Build and run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | tail -10
```

Expected: 0 errors.

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -10
```

Expected: all pass.

- [ ] **Step 6: Smoke test native fallback**

Since Docker is available on most dev machines, use `--native` to verify native path still works:

```bash
/Users/aicoder/develop/cargo-target/release/gait --native models 2>&1 | head -5
```

Expected: model list output (same as before this change).

- [ ] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/bin/gait.rs && git commit -m "feat: auto-route through Docker sandbox with --native bypass"
```

---

## Task 4: Dockerfile

**Files:**
- Create: `Dockerfile` (at repo root: `/Users/aicoder/Documents/ideation/gait/Dockerfile`)

No unit tests for the Dockerfile — verified by building and running.

- [ ] **Step 1: Create `Dockerfile`**

Create `/Users/aicoder/Documents/ideation/gait/Dockerfile`:

```dockerfile
# Stage 1: build the gait binary inside a Rust container
FROM rust:1.80-slim AS builder

# cmake: required by git2's vendored libgit2 build
# libssl-dev + pkg-config: required by reqwest (OpenSSL)
RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev cmake gcc \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY gait-rs/ .
RUN cargo build --release

# Stage 2: minimal runtime image with tooling
FROM debian:bookworm-slim

# libssl3: required if reqwest is dynamically linked against OpenSSL
RUN apt-get update && apt-get install -y \
    git \
    ripgrep \
    fd-find \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd

COPY --from=builder /build/target/release/gait /usr/local/bin/gait
RUN chmod +x /usr/local/bin/gait

# Signals to the wrapper binary that we are inside the container — skip Docker re-routing
ENV GAIT_CONTAINER=1

ENTRYPOINT ["/usr/local/bin/gait"]
```

- [ ] **Step 2: Build the image**

```bash
cd /Users/aicoder/Documents/ideation/gait && docker build -t gaitai/gait:latest . 2>&1 | tail -20
```

Expected: `Successfully tagged gaitai/gait:latest` (build may take a few minutes on first run).

- [ ] **Step 3: Verify tools are present**

```bash
docker run --rm gaitai/gait:latest --native models 2>&1 | head -3
docker run --rm gaitai/gait:latest sh -c "rg --version && fd --version && git --version" 2>&1
```

Expected: model list output, then rg/fd/git version strings.

- [ ] **Step 4: Verify sandbox routing end-to-end**

Run `gait models` from the host. It should route through Docker automatically:

```bash
/Users/aicoder/develop/cargo-target/release/gait models 2>&1 | head -5
```

Expected: model list (via container), no errors.

- [ ] **Step 5: Verify `--native` bypasses Docker**

```bash
/Users/aicoder/develop/cargo-target/release/gait --native models 2>&1 | head -3
```

Expected: same model list but running natively (no `docker run` invoked).

- [ ] **Step 6: Verify workspace/codebase mount rewriting**

```bash
/Users/aicoder/develop/cargo-target/release/gait --native models 2>&1 | head -3
# Then check docker-mounts.json was written
cat /Users/aicoder/Documents/ideation/gait/tmp/docker-mounts.json 2>/dev/null || echo "no mounts file (expected if --native used)"
```

Run without `--native` and a `--workspace` to verify:

```bash
/Users/aicoder/develop/cargo-target/release/gait --workspace /Users/aicoder/Documents/ideation/gait models 2>&1 | head -3
cat /Users/aicoder/Documents/ideation/gait/tmp/docker-mounts.json
```

Expected: `docker-mounts.json` shows `workspace_host` → `/gait-workspace` mapping.

- [ ] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait && git add Dockerfile && git commit -m "feat: add multi-stage Dockerfile with rg/fd/git for sandbox execution"
```
