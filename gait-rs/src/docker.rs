use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

pub fn is_inside_container() -> bool {
    std::env::var("GAIT_CONTAINER").as_deref() == Ok("1")
}

pub fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn image_present(image: &str) -> bool {
    std::process::Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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

pub fn write_mount_map(workspace: &Path, map: &MountMap) -> anyhow::Result<()> {
    let tmp = workspace.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let json = serde_json::to_string_pretty(map)?;
    std::fs::write(tmp.join("docker-mounts.json"), json)?;
    Ok(())
}

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

    // Pass through host git identity so commits in the container use the user's name/email.
    // git and libgit2 respect GIT_AUTHOR_* / GIT_COMMITTER_* natively.
    let git_name = std::process::Command::new("git")
        .args(["config", "user.name"])
        .output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let git_email = std::process::Command::new("git")
        .args(["config", "user.email"])
        .output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    if let (Some(name), Some(email)) = (git_name, git_email) {
        args.extend(["-e".to_string(), format!("GIT_AUTHOR_NAME={}", name)]);
        args.extend(["-e".to_string(), format!("GIT_AUTHOR_EMAIL={}", email)]);
        args.extend(["-e".to_string(), format!("GIT_COMMITTER_NAME={}", name)]);
        args.extend(["-e".to_string(), format!("GIT_COMMITTER_EMAIL={}", email)]);
    }

    args.extend([
        "-v".to_string(),
        format!("{}:{}", mounts.workspace_host.display(), mounts.workspace_container.display()),
    ]);

    if mounts.codebase_host != mounts.workspace_host {
        args.extend([
            "-v".to_string(),
            format!("{}:{}", mounts.codebase_host.display(), mounts.codebase_container.display()),
        ]);
    }

    if let Some(auth) = auth_dir {
        if auth.exists() {
            args.extend([
                "-v".to_string(),
                format!("{}:/root/.local/share/gait:ro", auth.display()),
            ]);
        }
    }

    args.push(image.to_string());
    args.extend(rewrite_inner_args(original_args, mounts));
    args
}

fn rewrite_inner_args(args: &[String], mounts: &MountMap) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "--codebase" => {
                i += 2;
            }
            "--native" => {
                i += 1;
            }
            _ => {
                out.push(args[i].clone());
                i += 1;
            }
        }
    }
    out.push("--workspace".to_string());
    out.push(mounts.workspace_container.to_string_lossy().to_string());
    out.push("--codebase".to_string());
    out.push(mounts.codebase_container.to_string_lossy().to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn not_in_container_when_env_unset() {
        if std::env::var("GAIT_CONTAINER").is_err() {
            assert!(!is_inside_container());
        }
    }

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
        assert_eq!(args.iter().filter(|a| *a == "-v").count(), 2);
    }

    #[test]
    fn build_docker_args_single_volume_when_same_path() {
        let mounts = build_mount_map(
            &PathBuf::from("/host/ws"),
            &PathBuf::from("/host/ws"),
        );
        let args = build_docker_args("gaitai/gait:latest", &mounts, None, &[], false);
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
}
