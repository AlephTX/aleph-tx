//! Shared-memory segment paths, mirroring the Go feeder's `shm.Path`.
//!
//! Supervised sessions export `ALEPH_SHM_DIR` to a private directory; every
//! reader must resolve segments there instead of the global `/dev/shm`, or it
//! silently attaches to another session's (or a stale) feeder.

use std::io;
use std::path::PathBuf;

/// Environment variable naming the directory that holds the SHM segments.
pub const SHM_DIR_ENV: &str = "ALEPH_SHM_DIR";
/// Directory used when `ALEPH_SHM_DIR` is unset or empty.
pub const DEFAULT_SHM_DIR: &str = "/dev/shm";

/// Directory that holds the SHM segments for this process.
pub fn shm_dir() -> PathBuf {
    resolve_dir(std::env::var_os(SHM_DIR_ENV))
}

/// UTF-8 path of the named segment, e.g. `shm_path("aleph-matrix")`.
pub fn shm_path(name: &str) -> io::Result<String> {
    shm_dir()
        .join(name)
        .into_os_string()
        .into_string()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{SHM_DIR_ENV} must be valid UTF-8"),
            )
        })
}

fn resolve_dir(value: Option<std::ffi::OsString>) -> PathBuf {
    match value {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(DEFAULT_SHM_DIR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_empty_dir_falls_back_to_dev_shm() {
        assert_eq!(resolve_dir(None), PathBuf::from("/dev/shm"));
        assert_eq!(resolve_dir(Some("".into())), PathBuf::from("/dev/shm"));
    }

    #[test]
    fn session_dir_is_honored() {
        let dir = resolve_dir(Some("/dev/shm/aleph-tx-abc".into()));
        assert_eq!(
            dir.join("aleph-matrix"),
            PathBuf::from("/dev/shm/aleph-tx-abc/aleph-matrix")
        );
    }
}
