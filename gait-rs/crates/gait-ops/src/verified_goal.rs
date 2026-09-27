/// Resolve the per-invocation override to a verified_goal command. An empty override disables it.
pub fn resolve(override_value: Option<&str>) -> Option<String> {
    match override_value {
        Some("") => None,
        Some(value) => Some(value.to_owned()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn verified_goal_empty_override_disables_and_none_returns_none() {
        assert_eq!(
            resolve(Some("cargo test -p example")).as_deref(),
            Some("cargo test -p example")
        );
        assert_eq!(resolve(Some("")), None);
        assert_eq!(resolve(None), None);
    }
}
