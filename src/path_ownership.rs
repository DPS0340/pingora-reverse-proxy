//! Replacement-safe ownership and cleanup for public filesystem entries.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::AsRawFd;

const QUARANTINE_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, Eq, PartialEq)]
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

    fn matches(self, other: Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
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

/// An opened parent directory and entry name captured before any pathname use.
#[cfg(unix)]
pub(crate) struct AnchoredDirectory {
    directory: File,
    directory_path: PathBuf,
}

#[cfg(unix)]
impl AnchoredDirectory {
    pub(crate) fn capture(parent: &Path) -> io::Result<Self> {
        let directory_path = fs::canonicalize(parent)?;
        let directory = open_directory(&directory_path)?;
        Ok(Self {
            directory,
            directory_path,
        })
    }

    pub(crate) fn stable_path(&self) -> io::Result<PathBuf> {
        stable_directory_path(&self.directory)
    }

    pub(crate) fn own_entry(
        &self,
        public_path: PathBuf,
        public_name: OsString,
    ) -> io::Result<OwnedPath> {
        let stat = rustix::fs::statat(
            &self.directory,
            &public_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        Ok(OwnedPath {
            public_path,
            identity: FileIdentity {
                device: stat.st_dev as u64,
                inode: stat.st_ino,
            },
            active: true,
            directory: self.directory.try_clone()?,
            directory_path: self.directory_path.clone(),
            public_name,
        })
    }

    pub(crate) fn remove_entry(&self, name: &OsStr) {
        let _ = rustix::fs::unlinkat(&self.directory, name, rustix::fs::AtFlags::empty());
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn stable_directory_path(directory: &File) -> io::Result<PathBuf> {
    Ok(PathBuf::from(format!(
        "/proc/self/fd/{}",
        directory.as_raw_fd()
    )))
}

#[cfg(target_vendor = "apple")]
fn stable_directory_path(directory: &File) -> io::Result<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;

    let mut path = [0 as libc::c_char; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let bytes = unsafe { CStr::from_ptr(path.as_ptr()) }.to_bytes().to_vec();
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn stable_directory_path(_directory: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stable path resolution from a directory descriptor is unavailable on this Unix target",
    ))
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
            let directory = open_directory(&directory_path)?;
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
    #[cfg(test)]
    pub(crate) fn from_path(path: PathBuf) -> io::Result<Self> {
        let public_name = path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "owned path has no file name")
            })?
            .to_owned();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let directory_path = fs::canonicalize(parent)?;
        let directory = open_directory(&directory_path)?;
        let stat = rustix::fs::statat(
            &directory,
            &public_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        Ok(Self {
            public_path: path,
            identity: FileIdentity {
                device: stat.st_dev as u64,
                inode: stat.st_ino,
            },
            active: true,
            directory,
            directory_path,
            public_name,
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn from_file(path: PathBuf, file: &File) -> io::Result<Self> {
        Self::new(path, FileIdentity::from_file(file)?)
    }

    #[cfg(not(unix))]
    fn new(path: PathBuf, identity: FileIdentity) -> io::Result<Self> {
        Ok(Self {
            public_path: path,
            identity,
            active: true,
        })
    }

    #[cfg(unix)]
    pub(crate) fn stable_entry_path(&self) -> io::Result<PathBuf> {
        Ok(stable_directory_path(&self.directory)?.join(&self.public_name))
    }

    #[cfg(unix)]
    pub(crate) fn configured_entry_matches(&self) -> io::Result<bool> {
        let parent = self.public_path.parent().unwrap_or_else(|| Path::new("."));
        let reopened_path = fs::canonicalize(parent)?;
        let reopened = open_directory(&reopened_path)?;
        if FileIdentity::from_file(&reopened)? != FileIdentity::from_file(&self.directory)? {
            return Ok(false);
        }
        let stat = rustix::fs::statat(
            &reopened,
            &self.public_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        Ok(self.identity.device == stat.st_dev as u64 && self.identity.inode == stat.st_ino)
    }

    #[cfg(unix)]
    pub(crate) fn publish_as(
        &mut self,
        path: PathBuf,
        destination_name: OsString,
    ) -> io::Result<()> {
        rename_relative_noreplace(
            &self.directory,
            &self.public_name,
            &self.directory,
            &destination_name,
        )?;
        self.public_name = destination_name;
        self.public_path = path;
        Ok(())
    }

    pub(crate) fn cleanup(&mut self) -> Option<PathBuf> {
        self.cleanup_with_hooks(|| {}, || {}, || {})
    }

    pub(crate) fn cleanup_with_hooks<B, V, R>(
        &mut self,
        before_quarantine: B,
        after_verify: V,
        before_restore: R,
    ) -> Option<PathBuf>
    where
        B: FnOnce(),
        V: FnOnce(),
        R: FnOnce(),
    {
        if !self.active {
            return None;
        }
        self.active = false;
        before_quarantine();

        let quarantine = match self.quarantine_entry() {
            Ok(Some(name)) => name,
            Ok(None) => return None,
            Err(error) => {
                tracing::error!(path = %self.public_path.display(), %error, "owned path quarantine failed");
                return None;
            }
        };
        #[cfg(unix)]
        let quarantine_path = self
            .directory_path
            .join(&quarantine.name)
            .join(PRIVATE_CANDIDATE_NAME);
        #[cfg(not(unix))]
        let quarantine_path = self
            .public_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&quarantine);

        if self.quarantine_matches(&quarantine, &quarantine_path) {
            after_verify();
            if let Err(error) = self.remove_quarantine(&quarantine, &quarantine_path) {
                tracing::error!(path = %quarantine_path.display(), %error, "verified quarantine removal failed");
                return Some(quarantine_path);
            }
            return None;
        }

        before_restore();
        match self.restore_quarantine(&quarantine, &quarantine_path) {
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

    #[cfg(unix)]
    fn quarantine_entry(&self) -> io::Result<Option<UnixQuarantine>> {
        self.quarantine_entry_with_fault(|_| Ok(()))
    }

    #[cfg(unix)]
    fn quarantine_entry_with_fault<F>(&self, mut fault: F) -> io::Result<Option<UnixQuarantine>>
    where
        F: FnMut(NamespaceStage) -> io::Result<()>,
    {
        for _ in 0..QUARANTINE_ATTEMPTS {
            let private_name = random_private_name()?;
            match rustix::fs::mkdirat(
                &self.directory,
                &private_name,
                rustix::fs::Mode::from_raw_mode(0o700),
            ) {
                Ok(()) => {}
                Err(error) if error == rustix::io::Errno::EXIST => continue,
                Err(error) => return Err(io::Error::from_raw_os_error(error.raw_os_error())),
            }
            fault(NamespaceStage::Open)?;
            let private_directory = match rustix::fs::openat(
                &self.directory,
                &private_name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            ) {
                Ok(descriptor) => File::from(descriptor),
                Err(error) => {
                    return Err(io::Error::from_raw_os_error(error.raw_os_error()));
                }
            };
            let directory_identity = FileIdentity::from_file(&private_directory)?;
            let mut namespace_guard = PrivateNamespaceGuard {
                parent: &self.directory,
                name: private_name.clone(),
                identity: directory_identity,
                armed: true,
            };
            fault(NamespaceStage::InitialStat)?;
            let created = rustix::fs::statat(
                &self.directory,
                &private_name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
            let published_identity = FileIdentity {
                device: created.st_dev as u64,
                inode: created.st_ino,
            };
            if published_identity != directory_identity {
                return Err(io::Error::other(
                    "private cleanup namespace changed before verification",
                ));
            }
            fault(NamespaceStage::Chmod)?;
            rustix::fs::fchmod(&private_directory, rustix::fs::Mode::from_raw_mode(0o700))
                .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
            fault(NamespaceStage::Identity)?;
            let quarantine = UnixQuarantine {
                name: private_name,
                directory: private_directory,
                identity: directory_identity,
            };
            fault(NamespaceStage::Rename)?;
            let result = rename_relative_noreplace(
                &self.directory,
                &self.public_name,
                &quarantine.directory,
                OsStr::new(PRIVATE_CANDIDATE_NAME),
            );
            match result {
                Ok(()) => {
                    namespace_guard.armed = false;
                    return Ok(Some(quarantine));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a private cleanup quarantine",
        ))
    }

    #[cfg(all(test, unix))]
    fn cleanup_with_namespace_fault_for_test(&mut self, stage: NamespaceStage) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = self.quarantine_entry_with_fault(|current| {
            if current == stage {
                Err(io::Error::other("injected private namespace fault"))
            } else {
                Ok(())
            }
        });
    }

    #[cfg(not(unix))]
    fn quarantine_entry(&self) -> io::Result<Option<OsString>> {
        for _ in 0..QUARANTINE_ATTEMPTS {
            let private_name = random_private_name()?;
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

    #[cfg(unix)]
    fn quarantine_matches(&self, quarantine: &UnixQuarantine, _path: &Path) -> bool {
        rustix::fs::statat(
            &quarantine.directory,
            PRIVATE_CANDIDATE_NAME,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .ok()
        .is_some_and(|stat| {
            self.identity.device == stat.st_dev as u64 && self.identity.inode == stat.st_ino
        })
    }

    #[cfg(not(unix))]
    fn quarantine_matches(&self, _name: &OsStr, path: &Path) -> bool {
        self.identity.matches_path(path)
    }

    #[cfg(unix)]
    fn remove_quarantine(&self, quarantine: &UnixQuarantine, _path: &Path) -> io::Result<()> {
        rustix::fs::unlinkat(
            &quarantine.directory,
            PRIVATE_CANDIDATE_NAME,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        self.remove_private_directory(quarantine)
    }

    #[cfg(not(unix))]
    fn remove_quarantine(&self, _name: &OsStr, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    #[cfg(unix)]
    fn restore_quarantine(&self, quarantine: &UnixQuarantine, _path: &Path) -> io::Result<()> {
        rename_relative_noreplace(
            &quarantine.directory,
            OsStr::new(PRIVATE_CANDIDATE_NAME),
            &self.directory,
            &self.public_name,
        )?;
        self.remove_private_directory(quarantine)
    }

    #[cfg(not(unix))]
    fn restore_quarantine(&self, name: &OsStr, path: &Path) -> io::Result<()> {
        let _ = name;
        rename_noreplace(path, &self.public_path)
    }

    #[cfg(unix)]
    fn remove_private_directory(&self, quarantine: &UnixQuarantine) -> io::Result<()> {
        let parent_identity = rustix::fs::statat(
            &self.directory,
            &quarantine.name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map(|stat| FileIdentity {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        })
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        if !quarantine.identity.matches(parent_identity) {
            return Err(io::Error::other(
                "private cleanup namespace was replaced; preserving replacement",
            ));
        }
        // POSIX has no identity-conditional rmdir-by-handle. The candidate is
        // protected from other users by the unpredictable 0700 directory and
        // every operation above is descriptor-relative. A same-UID actor with
        // write access to the parent remains inside the process trust boundary
        // and can race this final checked unlinkat after discovering the name.
        rustix::fs::unlinkat(
            &self.directory,
            &quarantine.name,
            rustix::fs::AtFlags::REMOVEDIR,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
    }
}

#[cfg(unix)]
const PRIVATE_CANDIDATE_NAME: &str = "candidate";

#[cfg(unix)]
struct UnixQuarantine {
    name: OsString,
    directory: File,
    identity: FileIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum NamespaceStage {
    InitialStat,
    Open,
    Chmod,
    Identity,
    Rename,
}

#[cfg(unix)]
struct PrivateNamespaceGuard<'a> {
    parent: &'a File,
    name: OsString,
    identity: FileIdentity,
    armed: bool,
}

#[cfg(unix)]
impl Drop for PrivateNamespaceGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let matches = rustix::fs::statat(
            self.parent,
            &self.name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .ok()
        .is_some_and(|stat| {
            self.identity.device == stat.st_dev as u64 && self.identity.inode == stat.st_ino
        });
        if matches {
            let _ = rustix::fs::unlinkat(self.parent, &self.name, rustix::fs::AtFlags::REMOVEDIR);
        }
    }
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
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
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
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
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
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
    use super::{NamespaceStage, OwnedPath};
    use std::{fs, io};

    #[cfg(unix)]
    fn assert_namespace_fault_leaves_no_private_debris(stage: NamespaceStage) {
        let root = tempfile::tempdir().expect("temporary namespace fault root");
        let public = root.path().join("owned.sock");
        fs::write(&public, b"owned").expect("create owned entry");
        let mut owner = OwnedPath::from_path(public.clone()).expect("capture owned entry");

        owner.cleanup_with_namespace_fault_for_test(stage);

        assert_eq!(fs::read(&public).expect("owned entry remains"), b"owned");
        assert_eq!(
            fs::read_dir(root.path())
                .expect("list namespace fault root")
                .count(),
            1,
            "fault left a private cleanup namespace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn openat_fault_preserves_unverified_private_namespace() {
        let root = tempfile::tempdir().expect("temporary open fault root");
        let public = root.path().join("owned.sock");
        fs::write(&public, b"owned").expect("create owned entry");
        let mut owner = OwnedPath::from_path(public.clone()).expect("capture owned entry");

        owner.cleanup_with_namespace_fault_for_test(NamespaceStage::Open);

        assert_eq!(fs::read(&public).expect("owned entry remains"), b"owned");
        let private_namespace = fs::read_dir(root.path())
            .expect("list open fault root")
            .find_map(|entry| {
                let entry = entry.expect("read open fault entry");
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".chp-cleanup-")
                    .then_some(entry.path())
            })
            .expect("unverified private namespace was removed");
        assert!(private_namespace.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn initial_statat_fault_removes_empty_private_namespace() {
        assert_namespace_fault_leaves_no_private_debris(NamespaceStage::InitialStat);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_at_initial_statat_boundary_is_preserved() {
        let root = tempfile::tempdir().expect("temporary initial-stat replacement root");
        let public = root.path().join("owned.sock");
        let displaced = root.path().join("displaced-private");
        fs::write(&public, b"owned").expect("create owned entry");
        let owner = OwnedPath::from_path(public.clone()).expect("capture owned entry");

        let result = owner.quarantine_entry_with_fault(|stage| {
            if stage != NamespaceStage::InitialStat {
                return Ok(());
            }
            let private_namespace = fs::read_dir(root.path())
                .expect("list cleanup root")
                .find_map(|entry| {
                    let entry = entry.expect("read cleanup entry");
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".chp-cleanup-")
                        .then_some(entry.path())
                })
                .expect("private namespace exists");
            fs::rename(&private_namespace, &displaced).expect("displace private namespace");
            fs::create_dir(&private_namespace).expect("install empty foreign replacement");
            Err(io::Error::other("injected initial statat fault"))
        });

        assert!(result.is_err(), "injected initial statat fault succeeded");
        let replacement = fs::read_dir(root.path())
            .expect("list cleanup root after replacement")
            .find_map(|entry| {
                let entry = entry.expect("read replacement entry");
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".chp-cleanup-")
                    .then_some(entry.path())
            })
            .expect("empty foreign replacement was removed");
        assert!(replacement.is_dir());
        assert!(displaced.is_dir(), "owned namespace was not displaced");
        assert_eq!(fs::read(&public).expect("owned entry remains"), b"owned");
    }

    #[cfg(unix)]
    #[test]
    fn fchmod_fault_removes_empty_private_namespace() {
        assert_namespace_fault_leaves_no_private_debris(NamespaceStage::Chmod);
    }

    #[cfg(unix)]
    #[test]
    fn identity_fault_removes_empty_private_namespace() {
        assert_namespace_fault_leaves_no_private_debris(NamespaceStage::Identity);
    }

    #[cfg(unix)]
    #[test]
    fn rename_fault_removes_empty_private_namespace() {
        assert_namespace_fault_leaves_no_private_debris(NamespaceStage::Rename);
    }

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

    #[cfg(unix)]
    #[test]
    fn replacement_after_private_verification_cannot_redirect_candidate_unlink() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary private cleanup root");
        let public_parent = root.path().join("live");
        let moved_parent = root.path().join("moved");
        fs::create_dir(&public_parent).expect("create public parent");
        let public = public_parent.join("proxy.sock");
        fs::write(&public, b"owned").expect("write owned entry");
        let mut owner = OwnedPath::from_path(public.clone()).expect("capture owned path");

        owner.cleanup_with_hooks(
            || {},
            || {
                let private_namespace = fs::read_dir(&public_parent)
                    .expect("list private cleanup namespace")
                    .next()
                    .expect("private cleanup namespace exists")
                    .expect("read private cleanup namespace");
                let metadata = private_namespace
                    .metadata()
                    .expect("private cleanup namespace metadata");
                assert!(metadata.is_dir());
                assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
                fs::rename(&public_parent, &moved_parent).expect("move captured parent");
                fs::create_dir(&public_parent).expect("replace public parent");
                fs::write(&public, b"foreign public entry").expect("write foreign entry");
            },
            || {},
        );

        assert_eq!(
            fs::read(&public).expect("foreign public entry remains"),
            b"foreign public entry"
        );
        assert_eq!(
            fs::read_dir(&moved_parent)
                .expect("list original captured parent")
                .count(),
            0,
            "ordinary private cleanup namespace leaked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_namespace_replacement_is_preserved_instead_of_removed() {
        let root = tempfile::tempdir().expect("temporary private replacement root");
        let public = root.path().join("proxy.sock");
        let displaced = root.path().join("displaced-private");
        fs::write(&public, b"owned").expect("write owned entry");
        let mut owner = OwnedPath::from_path(public.clone()).expect("capture owned path");

        let debris = owner.cleanup_with_hooks(
            || {},
            || {
                let private_namespace = fs::read_dir(root.path())
                    .expect("list cleanup root")
                    .find_map(|entry| {
                        let entry = entry.expect("read cleanup entry");
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".chp-cleanup-")
                            .then_some(entry.path())
                    })
                    .expect("private namespace exists");
                fs::rename(&private_namespace, &displaced).expect("displace private namespace");
                fs::create_dir(&private_namespace).expect("install replacement namespace");
                fs::write(private_namespace.join("foreign"), b"replacement")
                    .expect("populate replacement namespace");
            },
            || {},
        );

        assert!(
            debris.is_some(),
            "replacement must be reported as preserved"
        );
        let replacement = fs::read_dir(root.path())
            .expect("list cleanup root after replacement")
            .find_map(|entry| {
                let entry = entry.expect("read replacement entry");
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".chp-cleanup-")
                    .then_some(entry.path())
            })
            .expect("replacement namespace remains");
        assert_eq!(
            fs::read(replacement.join("foreign")).expect("foreign replacement remains"),
            b"replacement"
        );
        assert_eq!(
            fs::read_dir(&displaced)
                .expect("list displaced owned namespace")
                .count(),
            0,
            "verified candidate was not unlinked through its private dirfd"
        );
    }
}
