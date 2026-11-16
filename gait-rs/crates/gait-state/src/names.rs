//! Shared validation for authored names (modes, inference profiles) that
//! become one directory or file component under the gait dir.

/// Validate that `name` is one safe, normal path component; `kind` ("mode",
/// "inference") names the thing in the error.
pub fn validate_name(kind: &str, name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        anyhow::bail!("{kind} name '{name}' is invalid: must be a single normal path component");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn rejects_the_unsafe_set_with_kind_in_message() {
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b", "/abs"] {
            for kind in ["mode", "inference"] {
                let err = validate_name(kind, bad).unwrap_err().to_string();
                assert!(
                    err.contains(&format!("{kind} name")),
                    "{kind} {bad:?}: {err}"
                );
                assert!(err.contains("invalid"), "{err}");
            }
        }
    }

    #[test]
    fn accepts_normal_components() {
        for ok in ["review", "a-b_c", "v1.2", "..x", "x.."] {
            validate_name("mode", ok).unwrap();
        }
    }
}
