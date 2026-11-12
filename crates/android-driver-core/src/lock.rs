use std::fs::{File, OpenOptions, symlink_metadata};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

use crate::endpoint::AdbEndpoint;
use crate::error::{DriverError, ErrorCode, Result, invalid_argument};

const LOCK_VERSION: &str = "v1";

/// An advisory, kernel-held exclusive lock shared by every driver process on
/// this host. The file is never unlinked, so a process that dies releases the
/// kernel lock without leaving a stale file that needs recovery. Cooperation
/// is host-local: NFS, container namespaces, and another user's lock directory
/// are outside this guarantee.
pub struct DeviceLock {
    file: File,
    path: PathBuf,
}

impl DeviceLock {
    pub fn acquire(lock_dir: &Path, endpoint: &AdbEndpoint, serial: &str) -> Result<Self> {
        let directory = prepare_lock_dir(lock_dir)?;
        let path = directory.join(lock_file_name(endpoint, serial));
        if let Ok(metadata) = symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(invalid_argument(format!(
                "lock path is not a regular file: {}",
                path.display()
            )));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).map_err(|error| {
            invalid_argument(format!(
                "cannot open device lock {}: {error}",
                path.display()
            ))
        })?;

        #[cfg(unix)]
        {
            let metadata = file.metadata().map_err(|error| {
                invalid_argument(format!(
                    "cannot inspect device lock {}: {error}",
                    path.display()
                ))
            })?;
            if !metadata.is_file() || metadata.mode() & 0o077 != 0 {
                return Err(invalid_argument(format!(
                    "device lock is not a private regular file: {}",
                    path.display()
                )));
            }
        }

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(DriverError::new(
                    ErrorCode::Busy,
                    format!(
                        "device session is already owned by another process ({} serial {serial})",
                        endpoint.canonical()
                    ),
                )
                .detail("endpoint", endpoint.canonical())
                .detail("serial", serial));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(invalid_argument(format!(
                    "cannot lock device file {}: {error}",
                    path.display()
                )));
            }
        }

        let identity = format!(
            "{LOCK_VERSION} endpoint={} serial={} pid={}\n",
            endpoint.canonical(),
            serial,
            std::process::id()
        );
        let _ = file.set_len(0);
        let _ = file.write_all(identity.as_bytes());
        let _ = file.flush();
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DeviceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn default_lock_dir() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            return Ok(runtime.join("android-driver"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            return Ok(home.join(".android-driver").join("locks"));
        }
    }
    Err(invalid_argument(
        "cannot choose a lock directory; pass --lock-dir or set HOME",
    ))
}

fn prepare_lock_dir(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid_argument("lock directory must be an absolute path"));
    }
    create_private_dir(path)?;
    let canonical = path.canonicalize().map_err(|error| {
        invalid_argument(format!(
            "cannot resolve lock directory {}: {error}",
            path.display()
        ))
    })?;
    let metadata = symlink_metadata(&canonical).map_err(|error| {
        invalid_argument(format!(
            "cannot inspect lock directory {}: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_argument(format!(
            "lock directory is not a real directory: {}",
            canonical.display()
        )));
    }
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| {
                    invalid_argument(format!(
                        "lock directory is accessible by other users and cannot be tightened: {error}"
                    ))
                })?;
        }
        if let Some(expected_uid) = current_uid()
            && metadata.uid() != expected_uid
        {
            return Err(invalid_argument(format!(
                "lock directory is owned by another user: {}",
                canonical.display()
            )));
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| {
            invalid_argument(format!(
                "cannot create lock directory {}: {error}",
                path.display()
            ))
        })
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        invalid_argument(format!(
            "cannot create lock directory {}: {error}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    std::fs::metadata(home).ok().map(|metadata| metadata.uid())
}

fn lock_file_name(endpoint: &AdbEndpoint, serial: &str) -> String {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    let key = endpoint.canonical();
    for byte in key.bytes().chain([0u8]).chain(serial.bytes()) {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    format!("{LOCK_VERSION}-device-{hash:032x}.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_names_are_stable_and_endpoint_specific() {
        let loopback = AdbEndpoint::parse("127.0.0.1:5037").unwrap();
        let alias = AdbEndpoint::parse("localhost:5037").unwrap();
        let other_port = AdbEndpoint::parse("127.0.0.1:5038").unwrap();
        let first = lock_file_name(&loopback, "SERIAL");
        assert_eq!(first, lock_file_name(&alias, "SERIAL"));
        assert_ne!(first, lock_file_name(&other_port, "SERIAL"));
        assert_ne!(first, lock_file_name(&loopback, "OTHER"));
        assert!(first.ends_with(".lock"));
        assert!(!first.contains('/'));
    }
}
