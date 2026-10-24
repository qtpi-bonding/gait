use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Error, ErrorKind};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::auth::AuthError;

pub struct AuthFileLock {
    file: File,
}

impl AuthFileLock {
    pub fn try_acquire(auth_path: &Path) -> Result<Option<Self>, AuthError> {
        let parent = auth_path
            .parent()
            .ok_or_else(|| AuthError::NoParent(auth_path.to_owned()))?;
        fs::create_dir_all(parent)?;
        let path = auth_path.with_file_name("auth.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
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
    let cleanup_path = temp_path.clone();
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    let bytes = serde_json::to_vec_pretty(map).map_err(|error| {
        let _ = fs::remove_file(&cleanup_path);
        AuthError::Io(Error::new(ErrorKind::InvalidData, error))
    })?;
    use std::io::Write;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&cleanup_path);
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&cleanup_path);
        return Err(error.into());
    }
    Ok(())
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

    #[test]
    fn stale_lock_file_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(dir.path().join("auth.lock"), b"stale").unwrap();
        assert!(AuthFileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn failed_store_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
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
}
