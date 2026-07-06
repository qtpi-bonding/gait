//! Swappable filesystem backend for the read/write/edit tools.
//!
//! Today every tool talks to `std::fs` directly. That keeps the code simple,
//! but it means a future delegation target (e.g. an ACP client exposing the
//! editor's unsaved-buffer state via `fs/read_text_file`/`fs/write_text_file`)
//! would have to rewrite five tool bodies to plug in.
//!
//! `FsBackend` is the seam. The default [`LocalFs`] is a 1:1 pass-through to
//! `std::fs`, so every existing caller behaves byte-for-byte identically
//! until something swaps the `Arc` in [`crate::tools::ToolCtx`].
//!
//! Method set is intentionally minimal and driven by what the five tool files
//! (`read.rs`, `write.rs`, `edit/replace.rs`, `edit/append.rs`,
//! `edit/prepend.rs`) actually call today. No speculative methods (no
//! `read_dir`, no `metadata`).

use std::io::Read;
use std::path::Path;

/// The set of filesystem operations the read/write/edit tools need.
///
/// All methods take `&Path` (matching `std::fs` ergonomics) and return
/// [`std::io::Result`] so callers can use `?` / `.map_err(...)` the same way
/// they did against `std::fs`.
pub trait FsBackend: Send + Sync {
    /// Read the entire file as UTF-8 text. On a missing file or non-UTF-8
    /// bytes the underlying `std::io::Error` is returned — identical to
    /// [`std::fs::read_to_string`].
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;

    /// Read up to `len` bytes from the start of the file. Used by the
    /// binary-sniff heuristic in `read.rs`; we don't want to slurp a 4 GB
    /// file just to decide it's binary.
    ///
    /// Returns the number of bytes actually read (0..=len); a short read is
    /// not an error and is the expected case for files smaller than `len`.
    fn read_bytes_prefix(&self, path: &Path, len: usize) -> std::io::Result<Vec<u8>>;

    /// Write `contents` to `path`, creating or truncating as needed. Mirrors
    /// [`std::fs::write`].
    fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()>;

    /// Create `path` and any missing parents. Mirrors
    /// [`std::fs::create_dir_all`].
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;

    /// Whether `path` currently exists on the filesystem. Mirrors
    /// [`std::path::Path::exists`].
    fn exists(&self, path: &Path) -> bool;
}

/// Default `FsBackend` — a direct pass-through to `std::fs::*`. Byte-for-byte
/// identical behavior to the pre-seam tools.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFs;

impl FsBackend for LocalFs {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn read_bytes_prefix(&self, path: &Path, len: usize) -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        // Cap the read at `len` so we don't pull arbitrary file sizes into
        // memory for the binary-sniff heuristic.
        let mut buf = vec![0u8; len];
        let mut filled = 0usize;
        while filled < len {
            match file.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        std::fs::write(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn local_fs_round_trips_read_write_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        let fs = LocalFs;

        // exists() is false before write
        assert!(!fs.exists(&path));

        // write -> exists -> read back identical bytes
        let payload = b"hello\nworld\n";
        fs.write(&path, payload).unwrap();
        assert!(fs.exists(&path));
        let got = fs.read_to_string(&path).unwrap();
        assert_eq!(got, "hello\nworld\n");
        assert_eq!(got.as_bytes(), payload);
    }

    #[test]
    fn read_bytes_prefix_returns_short_read_for_small_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tiny.bin");
        let fs = LocalFs;
        fs.write(&path, b"abc").unwrap();

        // Asking for more than the file holds returns exactly what was there.
        let prefix = fs.read_bytes_prefix(&path, 4096).unwrap();
        assert_eq!(prefix, b"abc");
    }

    #[test]
    fn create_dir_all_makes_nested_path() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        let fs = LocalFs;
        assert!(!fs.exists(&nested));
        fs.create_dir_all(&nested).unwrap();
        assert!(fs.exists(&nested));
    }

    #[test]
    fn read_to_string_errors_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let fs = LocalFs;
        let err = fs.read_to_string(&dir.path().join("nope.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
