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
        if metadata.uid() != current_uid() {
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
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
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

    #[cfg(unix)]
    struct TempDir(PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "android-driver-lock-unit-{}-{index}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn process_uid_is_independent_of_home() {
        const CHILD: &str = "ANDROID_DRIVER_UID_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let directory = TempDir::new();
            let expected = std::fs::metadata(&directory.0).unwrap().uid();
            assert_eq!(current_uid(), expected);
            return;
        }
        let directory = TempDir::new();
        for home in [
            None,
            Some(directory.0.join("missing")),
            Some(PathBuf::from("/")),
        ] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "lock::tests::process_uid_is_independent_of_home"])
                .env(CHILD, "1")
                .env_remove("HOME");
            if let Some(home) = &home {
                command.env("HOME", home);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "HOME={home:?}:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn lock_permissions_are_private() {
        let directory = TempDir::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        let endpoint = AdbEndpoint::parse("127.0.0.1:5037").unwrap();
        let lock = DeviceLock::acquire(&directory.0, &endpoint, "PRIVATE").unwrap();
        assert_eq!(
            std::fs::metadata(&directory.0).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(lock.path()).unwrap().mode() & 0o777,
            0o600
        );
        let public_file = directory.0.join(lock_file_name(&endpoint, "PUBLIC"));
        std::fs::write(&public_file, b"").unwrap();
        std::fs::set_permissions(&public_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = DeviceLock::acquire(&directory.0, &endpoint, "PUBLIC")
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("private regular file"));
    }

    #[test]
    fn default_lock_dir_is_home_based() {
        const CHILD: &str = "ANDROID_DRIVER_LOCK_DIR_TEST_CHILD";
        const EXPECTED: &str = "ANDROID_DRIVER_EXPECTED_LOCK_DIR";
        if std::env::var_os(CHILD).is_some() {
            match std::env::var_os(EXPECTED) {
                Some(expected) => assert_eq!(default_lock_dir().unwrap(), PathBuf::from(expected)),
                None => {
                    let error = default_lock_dir().unwrap_err();
                    assert_eq!(error.code, ErrorCode::InvalidArgument);
                    assert!(error.message.contains("--lock-dir"));
                    assert!(error.message.contains("HOME"));
                }
            }
            return;
        }

        let home = std::env::temp_dir().join("android-driver-test-home");
        let runtime = std::env::temp_dir().join("android-driver-test-runtime");
        let expected = home.join(".android-driver").join("locks");
        let cases = [
            (Some(home.as_path()), None, true),
            (Some(home.as_path()), Some(runtime.as_path()), true),
            (
                Some(home.as_path()),
                Some(Path::new("relative-runtime")),
                true,
            ),
            (None, Some(runtime.as_path()), false),
            (Some(Path::new("")), Some(runtime.as_path()), false),
            (
                Some(Path::new("relative-home")),
                Some(runtime.as_path()),
                false,
            ),
        ];
        for (home, runtime, valid) in cases {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "lock::tests::default_lock_dir_is_home_based"])
                .env(CHILD, "1")
                .env_remove("HOME")
                .env_remove("XDG_RUNTIME_DIR")
                .env_remove(EXPECTED);
            if let Some(home) = home {
                command.env("HOME", home);
            }
            if let Some(runtime) = runtime {
                command.env("XDG_RUNTIME_DIR", runtime);
            }
            if valid {
                command.env(EXPECTED, &expected);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "HOME={home:?}, XDG_RUNTIME_DIR={runtime:?}:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

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
