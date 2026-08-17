//! destination free-space check (DoM bug 3150: writer should check DCP <= destination).
//! fails a write early when the required bytes exceed the free space on the
//! destination filesystem, instead of running out of room mid-copy/encode.

use std::path::Path;

/// bytes available to an unprivileged user on the filesystem containing `path`.
/// `path` must exist (a directory is fine).
pub fn available_bytes(path: &Path) -> std::io::Result<u64> {
    volume_bytes(path).map(|(available, _)| available)
}

/// bytes available to an unprivileged user, and total bytes, on the filesystem
/// containing `path`. `path` must exist (a directory is fine).
#[cfg(unix)]
pub fn volume_bytes(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has null byte"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // f_bavail: blocks free for unprivileged users; f_frsize: bytes per block
    let block = stat.f_frsize as u64;
    Ok((stat.f_bavail as u64 * block, stat.f_blocks as u64 * block))
}

/// bytes available and total on the volume containing `path`, via
/// GetDiskFreeSpaceExW.
#[cfg(windows)]
pub fn volume_bytes(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;

    // kernel32; always linked, so no extra dependency
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((free_to_caller, total))
}

/// total size in bytes of `path`: a plain file's length, or the sum of all files
/// under a directory (recursively). unreadable entries count as zero.
pub fn path_size(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| path_size(&e.path()))
            .sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Fail with a clear message when `required` bytes won't fit in the free space at
/// `dest`. If the free space can't be determined, warns and allows the write
/// rather than blocking on a stat failure.
pub fn check_destination_space(dest: &Path, required: u64) -> Result<(), String> {
    match available_bytes(dest) {
        Ok(available) if required > available => Err(format!(
            "not enough space on destination {}: need {} but only {} free",
            dest.display(),
            human_bytes(required),
            human_bytes(available),
        )),
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("could not check free space at {}: {e}", dest.display());
            Ok(())
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_reports_available_within_total() {
        let dir = tempfile::tempdir().unwrap();
        // one reading, so the two numbers describe the same instant: a mounted
        // filesystem has a size, and what a user may still write is part of it.
        // Two readings can never be compared for equality, since anything else
        // writing to the volume moves the available figure between the calls.
        let (available, total) = volume_bytes(dir.path()).unwrap();
        assert!(total > 0, "a mounted filesystem has a size");
        assert!(available <= total, "{available} > {total}");
        assert!(available_bytes(dir.path()).unwrap() <= total);
    }

    #[test]
    fn a_path_that_does_not_exist_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no_such_directory");
        assert!(volume_bytes(&missing).is_err());
        assert!(available_bytes(&missing).is_err());
        // the caller keeps going when the volume cannot be read: refusing a write
        // over a failed stat would block more than it protects
        assert!(check_destination_space(&missing, u64::MAX).is_ok());
    }

    #[test]
    fn passes_when_space_is_ample() {
        let dir = tempfile::tempdir().unwrap();
        // zero required always fits on an existing filesystem
        assert!(check_destination_space(dir.path(), 0).is_ok());
    }

    #[test]
    fn fails_when_required_exceeds_free() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_destination_space(dir.path(), u64::MAX).unwrap_err();
        assert!(err.contains("not enough space"), "{err}");
    }

    #[test]
    fn path_size_sums_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![0u8; 50]).unwrap();
        assert_eq!(path_size(dir.path()), 150);
    }
}
