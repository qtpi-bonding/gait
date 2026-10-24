use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::auth::AuthError;

pub struct AuthFileLock {
    file: File,
    path: PathBuf,
}

impl AuthFileLock {
    pub fn try_acquire(auth_path: &Path) -> Result<Option<Self>, AuthError> {
        let parent = auth_path
            .parent()
            .ok_or_else(|| AuthError::NoParent(auth_path.to_owned()))?;
        fs::create_dir_all(parent)?;
        let path = auth_path.with_file_name("auth.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                restrict_permissions(&file)?;
                Ok(Some(Self { file, path }))
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
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
        let _ = self.file.sync_all();
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn restrict_permissions(file: &File) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &File) -> Result<(), AuthError> {
    Ok(())
}

pub fn load_auth_map(path: &Path) -> Result<Map<String, Value>, AuthError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error.into()),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    serde_json::from_slice(&bytes).map_err(|error| AuthError::Corrupt {
        path: path.to_owned(),
        detail: error.to_string(),
    })
}

pub fn store_auth_map(path: &Path, map: &Map<String, Value>) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::NoParent(path.to_owned()))?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".auth.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp_path)?;
    restrict_permissions(&file)?;
    let bytes = serde_json::to_vec_pretty(map).map_err(|error| AuthError::Corrupt {
        path: path.to_owned(),
        detail: error.to_string(),
    })?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp_path, path)?;
    Ok(())
}

pub fn update_auth_file<T>(
    path: &Path,
    f: impl FnOnce(&mut Map<String, Value>) -> Result<T, AuthError>,
) -> Result<T, AuthError> {
    let _lock = AuthFileLock::acquire_blocking(path, Duration::from_secs(30))?;
    let mut map = load_auth_map(path)?;
    let result = f(&mut map)?;
    store_auth_map(path, &map)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let original = b"{not json";
        fs::write(&path, original).unwrap();

        let result = update_auth_file(&path, |map| {
            map.insert("new".into(), serde_json::json!({"type": "api"}));
            Ok(())
        });

        assert!(matches!(result, Err(AuthError::Corrupt { .. })));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn unknown_entries_survive_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let unknown = serde_json::json!({"type": "future", "a": 1});
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"x": unknown})).unwrap(),
        )
        .unwrap();

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
        let dir = tempfile::tempdir().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let first = AuthFileLock::try_acquire(&path).unwrap().unwrap();
        assert!(AuthFileLock::try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(AuthFileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn acquire_blocking_times_out_with_lock_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let _first = AuthFileLock::try_acquire(&path).unwrap().unwrap();
        let result = AuthFileLock::acquire_blocking(&path, Duration::from_millis(100));
        assert!(matches!(result, Err(AuthError::LockTimeout)));
    }
}
