//! Replacement-safe ownership and cleanup for public filesystem entries.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

const QUARANTINE_ATTEMPTS: usize = 8;

#[derive(Clone, Copy)]
pub(crate) struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(not(any(unix, windows)))]
    length: u64,
}

impl FileIdentity {
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        Self::from_metadata(&file.metadata()?)
    }

    #[cfg(unix)]
    pub(crate) fn from_path(path: &Path) -> io::Result<Self> {
        Self::from_metadata(&fs::symlink_metadata(path)?)
    }

    fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            Ok(Self {
                volume_serial_number: metadata.volume_serial_number(),
                file_index: metadata.file_index(),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {
                length: metadata.len(),
            })
        }
    }

    #[cfg(not(unix))]
    fn matches_path(self, path: &Path) -> bool {
        fs::symlink_metadata(path)
            .ok()
            .and_then(|metadata| Self::from_metadata(&metadata).ok())
            .is_some_and(|other| self.matches(other))
    }

    #[cfg(not(unix))]
    fn matches(self, other: Self) -> bool {
        #[cfg(windows)]
        {
            self.volume_serial_number.is_some()
                && self.file_index.is_some()
                && self.volume_serial_number == other.volume_serial_number
                && self.file_index == other.file_index
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (self.length, other.length);
            false
        }
    }
}

/// Owns one exact filesystem identity and removes it through atomic quarantine.
pub(crate) struct OwnedPath {
    public_path: PathBuf,
    identity: FileIdentity,
    active: bool,
    #[cfg(unix)]
    directory: File,
    #[cfg(unix)]
    directory_path: PathBuf,
    #[cfg(unix)]
    public_name: OsString,
}

impl OwnedPath {
    pub(crate) fn create_new_file(path: PathBuf) -> io::Result<(File, Self)> {
        #[cfg(unix)]
        {
            let public_name = path
                .file_name()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "owned path has no file name")
                })?
                .to_owned();
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let directory_path = fs::canonicalize(parent)?;
            let directory = File::open(&directory_path)?;
            let descriptor = rustix::fs::openat(
                &directory,
                &public_name,
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::from_raw_mode(0o666),
            )
            .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
            let file = File::from(descriptor);
            let owner = Self {
                public_path: path,
                identity: FileIdentity::from_file(&file)?,
                active: true,
                directory,
                directory_path,
                public_name,
            };
            Ok((file, owner))
        }
        #[cfg(not(unix))]
        {
            use std::fs::OpenOptions;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let owner = Self::from_file(path, &file)?;
            Ok((file, owner))
        }
    }

    #[cfg(unix)]
    pub(crate) fn from_path(path: PathBuf) -> io::Result<Self> {
        let identity = FileIdentity::from_path(&path)?;
        Self::new(path, identity)
    }

    #[cfg(not(unix))]
    pub(crate) fn from_file(path: PathBuf, file: &File) -> io::Result<Self> {
        Self::new(path, FileIdentity::from_file(file)?)
    }

    fn new(path: PathBuf, identity: FileIdentity) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let public_name = path
                .file_name()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "owned path has no file name")
                })?
                .to_owned();
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let canonical_parent = fs::canonicalize(parent)?;
            let directory = File::open(&canonical_parent)?;
            Ok(Self {
                public_path: path,
                identity,
                active: true,
                directory,
                directory_path: canonical_parent,
                public_name,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                public_path: path,
                identity,
                active: true,
            })
        }
    }

    #[cfg(unix)]
    pub(crate) fn path(&self) -> &Path {
        &self.public_path
    }

    #[cfg(unix)]
    pub(crate) fn set_path(&mut self, path: PathBuf) {
        if let Some(name) = path.file_name() {
            self.public_name = name.to_owned();
            self.public_path = path;
        }
    }

    pub(crate) fn cleanup(&mut self) -> Option<PathBuf> {
        self.cleanup_with_hooks(|| {}, || {})
    }

    pub(crate) fn cleanup_with_hooks<B, R>(
        &mut self,
        before_quarantine: B,
        before_restore: R,
    ) -> Option<PathBuf>
    where
        B: FnOnce(),
        R: FnOnce(),
    {
        if !self.active {
            return None;
        }
        self.active = false;
        before_quarantine();

        let quarantine_name = match self.quarantine_entry() {
            Ok(Some(name)) => name,
            Ok(None) => return None,
            Err(error) => {
                tracing::error!(path = %self.public_path.display(), %error, "owned path quarantine failed");
                return None;
            }
        };
        #[cfg(unix)]
        let quarantine_path = self.directory_path.join(&quarantine_name);
        #[cfg(not(unix))]
        let quarantine_path = self
            .public_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&quarantine_name);

        if self.quarantine_matches(&quarantine_name, &quarantine_path) {
            if let Err(error) = self.remove_quarantine(&quarantine_name, &quarantine_path) {
                tracing::error!(path = %quarantine_path.display(), %error, "verified quarantine removal failed");
                return Some(quarantine_path);
            }
            return None;
        }

        before_restore();
        match self.restore_quarantine(&quarantine_name, &quarantine_path) {
            Ok(()) => None,
            Err(error) => {
                tracing::error!(
                    public_path = %self.public_path.display(),
                    quarantine_path = %quarantine_path.display(),
                    %error,
                    "foreign replacement could not be restored; preserving its private quarantine"
                );
                Some(quarantine_path)
            }
        }
    }

    fn quarantine_entry(&self) -> io::Result<Option<OsString>> {
        for _ in 0..QUARANTINE_ATTEMPTS {
            let private_name = random_private_name()?;
            #[cfg(unix)]
            let result = rename_relative_noreplace(
                &self.directory,
                &self.public_name,
                &self.directory,
                &private_name,
            );
            #[cfg(not(unix))]
            let result = rename_noreplace(
                &self.public_path,
                &self
                    .public_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(&private_name),
            );
            match result {
                Ok(()) => return Ok(Some(private_name)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a private cleanup quarantine",
        ))
    }

    fn quarantine_matches(&self, name: &OsStr, _path: &Path) -> bool {
        #[cfg(unix)]
        {
            rustix::fs::statat(&self.directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .ok()
                .is_some_and(|stat| {
                    self.identity.device == stat.st_dev as u64 && self.identity.inode == stat.st_ino
                })
        }
        #[cfg(not(unix))]
        {
            self.identity.matches_path(_path)
        }
    }

    fn remove_quarantine(&self, name: &OsStr, _path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::unlinkat(&self.directory, name, rustix::fs::AtFlags::empty())
                .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
        }
        #[cfg(not(unix))]
        {
            fs::remove_file(_path)
        }
    }

    fn restore_quarantine(&self, name: &OsStr, _path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            rename_relative_noreplace(&self.directory, name, &self.directory, &self.public_name)
        }
        #[cfg(not(unix))]
        {
            rename_noreplace(_path, &self.public_path)
        }
    }
}

impl Drop for OwnedPath {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn random_private_name() -> io::Result<OsString> {
    let mut random = [0_u8; 16];
    openssl::rand::rand_bytes(&mut random)
        .map_err(|_| io::Error::other("could not generate a private cleanup name"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(OsString::from(format!(".chp-cleanup-{suffix}")))
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "redox"
))]
fn rename_relative_noreplace(
    source_directory: &File,
    source: &OsStr,
    destination_directory: &File,
    destination: &OsStr,
) -> io::Result<()> {
    rustix::fs::renameat_with(
        source_directory,
        source,
        destination_directory,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "redox"
    ))
))]
fn rename_relative_noreplace(
    _source_directory: &File,
    _source: &OsStr,
    _destination_directory: &File,
    _destination: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic descriptor-relative no-replace rename is unavailable on this platform",
    ))
}

#[cfg(windows)]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    // MoveFileEx without MOVEFILE_REPLACE_EXISTING is atomic and fails if the
    // destination exists; std::fs::rename uses that behavior on Windows.
    fs::rename(source, destination)
}

#[cfg(not(any(unix, windows)))]
fn rename_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::OwnedPath;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn cleanup_stays_with_the_owned_parent_when_public_alias_is_replaced() {
        let root = tempfile::tempdir().expect("temporary parent ownership root");
        let public_parent = root.path().join("live");
        let moved_parent = root.path().join("moved");
        fs::create_dir(&public_parent).expect("create public parent");
        let public = public_parent.join("proxy.sock");
        fs::write(&public, b"owned").expect("write owned entry");
        let mut owner = OwnedPath::from_path(public.clone()).expect("capture owned path");

        owner.cleanup_with_hooks(
            || {
                fs::rename(&public_parent, &moved_parent).expect("move owned parent");
                fs::create_dir(&public_parent).expect("replace public parent");
                fs::write(&public, b"replacement").expect("write foreign public entry");
            },
            || {},
        );

        assert_eq!(
            fs::read(&public).expect("foreign public entry remains"),
            b"replacement"
        );
        assert!(!moved_parent.join("proxy.sock").exists());
        assert_eq!(
            fs::read_dir(&moved_parent)
                .expect("list original parent")
                .count(),
            0
        );
    }
}
