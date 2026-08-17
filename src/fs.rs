//! Filesystem helpers packaging relies on.

use std::path::Path;

/// Write bytes to `path` atomically: temp file in the same directory, then
/// rename. A reader either sees the old file or the fully written new one,
/// never a half-written one. Parent directories are created as needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| format!("cannot create temp file in {}: {e}", dir.display()))?;
    use std::io::Write;
    tmp.write_all(bytes)
        .map_err(|e| format!("cannot write temp file: {e}"))?;
    tmp.flush()
        .map_err(|e| format!("cannot flush temp file: {e}"))?;
    tmp.persist(path)
        .map_err(|e| format!("cannot rename into {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_parents_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("db.json");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        write_atomic(&path, b"world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "only the file remains, no temp files");
    }
}
