//! Container environment detection. Container spawning is always handled
//! externally (Serverpod, podman, `docker run`); gait itself never spawns
//! a container.

/// Returns `true` when gait is running inside a container.
/// Checks the `GAIT_CONTAINER=1` env var set unconditionally by the Dockerfile.
pub fn is_inside_container() -> bool {
    std::env::var("GAIT_CONTAINER").as_deref() == Ok("1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn not_in_container_when_env_unset() {
        std::env::remove_var("GAIT_CONTAINER");
        assert!(!is_inside_container());
    }

    #[test]
    #[serial]
    fn in_container_when_env_set_to_one() {
        std::env::set_var("GAIT_CONTAINER", "1");
        let result = is_inside_container();
        std::env::remove_var("GAIT_CONTAINER");
        assert!(result);
    }

    #[test]
    #[serial]
    fn not_in_container_when_env_is_zero() {
        std::env::set_var("GAIT_CONTAINER", "0");
        let result = is_inside_container();
        std::env::remove_var("GAIT_CONTAINER");
        assert!(!result);
    }
}
