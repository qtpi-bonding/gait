use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::auth::AuthError;

pub const AUTH_FILE_MAX_BYTES: usize = 1024 * 1024;

pub struct AuthFileLock {
    file: File,
}

impl AuthFileLock {
    pub fn try_acquire(auth_path: &Path) -> Result<Option<Self>, AuthError> {
        let parent = auth_path
            .parent()
            .ok_or_else(|| AuthError::NoParent(auth_path.to_owned()))?;
        identity_keyfile::private_fs::create_private_dir(parent)
            .map_err(|e| AuthError::from_keyfile(&e))?;
        let path = auth_path.with_file_name("auth.lock");
        let file = identity_keyfile::private_fs::open_private_lock_file(&path)
            .map_err(|e| AuthError::from_keyfile(&e))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn acquire_blocking(auth_path: &Path, timeout: Duration) -> Result<Self, AuthError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(lock) = Self::try_acquire(auth_path)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(AuthError::LockTimeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for AuthFileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub fn load_auth_map(path: &Path) -> Result<Map<String, Value>, AuthError> {
    if let Some(parent) = path.parent() {
        if fs::symlink_metadata(parent).is_ok() {
            identity_keyfile::private_fs::check_private_dir(parent)
                .map_err(|e| AuthError::from_keyfile(&e))?;
        }
    }
    let bytes = match identity_keyfile::private_fs::read_private_file(path, AUTH_FILE_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(identity_keyfile::KeyfileError::Missing(_)) => return Ok(Map::new()),
        Err(error) => return Err(AuthError::from_keyfile(&error)),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    serde_json::from_slice(&bytes[..]).map_err(|error| AuthError::Corrupt {
        path: path.to_owned(),
        detail: error.to_string(),
    })
}

pub fn store_auth_map(path: &Path, map: &Map<String, Value>) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::NoParent(path.to_owned()))?;
    let bytes = serde_json::to_vec_pretty(map)
        .map_err(|error| AuthError::Io(std::io::Error::new(ErrorKind::InvalidData, error)))?;
    identity_keyfile::private_fs::create_private_dir(parent)
        .map_err(|e| AuthError::from_keyfile(&e))?;
    identity_keyfile::private_fs::write_private_atomic(path, &bytes, 0o600)
        .map_err(|e| AuthError::from_keyfile(&e))
}

pub fn update_auth_file<T>(
    path: &Path,
    f: impl FnOnce(&mut Map<String, Value>) -> Result<T, AuthError>,
) -> Result<T, AuthError> {
    let _lock = AuthFileLock::acquire_blocking(path, Duration::from_secs(30))?;
    let mut map = load_auth_map(path)?;
    let original = map.clone();
    let result = f(&mut map)?;
    if map != original {
        store_auth_map(path, &map)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_file_is_not_overwritten() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let original = b"{not json";
        crate::auth_test_support::write_private(&path, original);

        let result = update_auth_file(&path, |map| {
            map.insert("new".into(), serde_json::json!({"type": "api"}));
            Ok(())
        });

        assert!(matches!(result, Err(AuthError::Corrupt { .. })));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn unknown_entries_survive_roundtrip() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let unknown = serde_json::json!({"type": "future", "a": 1});
        crate::auth_test_support::write_private(
            &path,
            serde_json::to_vec(&serde_json::json!({"x": unknown})).unwrap(),
        );

        update_auth_file(&path, |map| {
            map.insert("y".into(), serde_json::json!({"type": "api", "key": "k"}));
            Ok(())
        })
        .unwrap();

        let map = load_auth_map(&path).unwrap();
        assert_eq!(map.get("x"), Some(&unknown));
        assert_eq!(
            map.get("y").and_then(|v| v.get("key")),
            Some(&serde_json::json!("k"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_is_0600_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let mut map = Map::new();
        map.insert("x".into(), serde_json::json!({"type": "api"}));

        store_auth_map(&path, &map).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("auth.json")]);
    }

    #[test]
    fn lock_excludes_second_holder() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let first = AuthFileLock::try_acquire(&path).unwrap().unwrap();
        assert!(AuthFileLock::try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(AuthFileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn acquire_blocking_times_out_with_lock_timeout() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let _first = AuthFileLock::try_acquire(&path).unwrap().unwrap();
        let result = AuthFileLock::acquire_blocking(&path, Duration::from_millis(100));
        assert!(matches!(result, Err(AuthError::LockTimeout)));
    }

    #[test]
    fn stale_lock_file_does_not_block() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        crate::auth_test_support::write_private(dir.path().join("auth.lock"), b"stale");
        assert!(AuthFileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn failed_store_leaves_no_temp_file() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        fs::create_dir(&path).unwrap();
        let result = store_auth_map(&path, &Map::new());
        assert!(result.is_err());
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("auth.json")]);
    }

    // ---- A1-fix I-2: private_fs, size cap, fail closed on loose modes ----

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn code_of(error: AuthError) -> &'static str {
        use coded_error::CodedError;
        error.code()
    }

    #[cfg(unix)]
    fn assert_refused(error: AuthError, path: &Path, mode: &str) {
        let text = error.to_string();
        assert!(matches!(error, AuthError::Insecure { .. }), "{text}");
        assert_eq!(code_of(error), "gait.auth_store.insecure", "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains(mode), "{text}");
        assert!(
            text.contains("run the Wave D permissions-and-replay migration script"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn loose_auth_dir_is_refused_not_tightened() {
        let dir = crate::auth_test_support::private_tempdir();
        chmod(dir.path(), 0o755);
        let path = dir.path().join("auth.json");
        let error = update_auth_file(&path, |_| Ok(())).unwrap_err();
        assert_refused(error, dir.path(), "0755");
        assert_eq!(mode_of(dir.path()), 0o755);
        assert!(!path.exists());
        assert!(!dir.path().join("auth.lock").exists());
        let error = load_auth_map(&path).unwrap_err();
        assert_refused(error, dir.path(), "0755");
    }

    #[cfg(unix)]
    #[test]
    fn loose_auth_file_is_refused_not_tightened() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        crate::auth_test_support::write_private(&path, b"{}");
        chmod(&path, 0o644);
        let error = load_auth_map(&path).unwrap_err();
        assert_refused(error, &path, "0644");
        let error = update_auth_file(&path, |_| Ok(())).unwrap_err();
        assert_refused(error, &path, "0644");
        assert_eq!(mode_of(&path), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn loose_lock_file_is_refused_not_tightened() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let lock = dir.path().join("auth.lock");
        crate::auth_test_support::write_private(&lock, b"");
        chmod(&lock, 0o664);
        let error = AuthFileLock::try_acquire(&path).err().unwrap();
        assert_refused(error, &lock, "0664");
        assert_eq!(mode_of(&lock), 0o664);
    }

    #[cfg(unix)]
    #[test]
    fn oversize_auth_file_is_refused() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        crate::auth_test_support::write_private(&path, vec![b' '; AUTH_FILE_MAX_BYTES + 1]);
        chmod(&path, 0o600);
        let error = load_auth_map(&path).unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, AuthError::TooLarge { .. }), "{text}");
        assert_eq!(code_of(error), "gait.auth_store.too_large", "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains(&AUTH_FILE_MAX_BYTES.to_string()), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn fresh_state_is_created_private() {
        let root = crate::auth_test_support::private_tempdir();
        let dir = root.path().join("fresh");
        let path = dir.join("auth.json");
        update_auth_file(&path, |map| {
            map.insert("k".into(), serde_json::json!({"type": "api", "key": "v"}));
            Ok(())
        })
        .unwrap();
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(&dir.join("auth.lock")), 0o600);
        assert_eq!(load_auth_map(&path).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_round_trips_and_replaces() {
        let dir = crate::auth_test_support::private_tempdir();
        let path = dir.path().join("auth.json");
        let mut map = Map::new();
        map.insert("a".into(), serde_json::json!({"type": "api", "key": "1"}));
        store_auth_map(&path, &map).unwrap();
        map.insert("b".into(), serde_json::json!({"type": "api", "key": "2"}));
        store_auth_map(&path, &map).unwrap();
        assert_eq!(load_auth_map(&path).unwrap(), map);
        assert_eq!(mode_of(&path), 0o600);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("auth.json")]);
    }
}
