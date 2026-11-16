//! Test fixtures for the private-file auth state: `tempfile` directories are
//! created from the process umask (0775 here), which `private_fs` refuses.

use std::path::Path;

pub fn private_tempdir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

pub fn private_dirs(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

pub fn write_private(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path.as_ref(), bytes).unwrap();
    std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o600)).unwrap();
}
