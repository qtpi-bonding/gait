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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_in_container_when_env_unset() {
        if std::env::var("GAIT_CONTAINER").is_err() {
            assert!(!is_inside_container());
        }
    }
}
