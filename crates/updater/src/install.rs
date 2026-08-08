//! Staging and applying an update on disk.
//!
//! All three platforms use one strategy: unpack beside the installation, then
//! swap directories by rename. This works everywhere because every supported OS
//! allows renaming a directory that contains a running executable, including
//! Windows, which forbids only overwriting or deleting the running image. The
//! previous installation is kept as a backup until the new one has started
//! successfully, so a failed swap can always be undone.

use std::fs;
use std::io::{Cursor, Read as _};
use std::path::{Component, Path, PathBuf};

use crate::manifest::ArtifactFormat;
use crate::version::executable_name;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("could not determine the installation directory: {0}")]
    UnknownInstallDir(String),
    #[error(
        "installation directory {path} is not writable, so this build cannot update itself; \
         install a new release manually"
    )]
    ReadOnlyInstall { path: PathBuf },
    #[error("update archive is malformed: {0}")]
    MalformedArchive(String),
    #[error("update archive entry {entry} escapes the extraction directory; refusing to unpack")]
    UnsafeEntry { entry: String },
    #[error("update archive does not contain {expected}")]
    MissingExecutable { expected: String },
    #[error("{operation} failed: {source}")]
    Io {
        operation: String,
        #[source]
        source: std::io::Error,
    },
    #[error("update failed and the previous installation was restored: {reason}")]
    RolledBack { reason: String },
    #[error(
        "update failed while swapping directories and the installation could not be \
         restored automatically. The previous installation is at {backup}; move it back to \
         {install} to recover. Cause: {reason}"
    )]
    RollbackFailed {
        install: PathBuf,
        backup: PathBuf,
        reason: String,
    },
}

fn io(operation: impl Into<String>) -> impl FnOnce(std::io::Error) -> InstallError {
    let operation = operation.into();
    |source| InstallError::Io { operation, source }
}

/// Where an installation lives and where update work happens.
///
/// Staging and backup are siblings of the install directory rather than
/// children of the user data directory, because a rename is only atomic within
/// one filesystem and user data is frequently on a different mount.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallLayout {
    pub install_dir: PathBuf,
    pub staging_root: PathBuf,
    pub backup_root: PathBuf,
}

impl InstallLayout {
    /// Layout derived from an install directory.
    #[must_use]
    pub fn for_install_dir(install_dir: impl Into<PathBuf>) -> Self {
        let install_dir = install_dir.into();
        let parent = install_dir
            .parent()
            .map_or_else(|| install_dir.clone(), Path::to_path_buf);
        let name = install_dir.file_name().map_or_else(
            || "gcabb".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self {
            staging_root: parent.join(format!(".{name}-update-staging")),
            backup_root: parent.join(format!(".{name}-update-backup")),
            install_dir,
        }
    }

    /// Layout of the currently running installation.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError`] when the running executable's location cannot
    /// be determined.
    pub fn for_running_executable() -> Result<Self, InstallError> {
        let exe = std::env::current_exe()
            .map_err(|error| InstallError::UnknownInstallDir(error.to_string()))?;
        let dir = exe.parent().ok_or_else(|| {
            InstallError::UnknownInstallDir(format!("{} has no parent directory", exe.display()))
        })?;
        Ok(Self::for_install_dir(dir))
    }

    /// Confirms the installation can be replaced before anything is downloaded.
    ///
    /// A system-managed or read-only install is a normal deployment, not a bug,
    /// so it is detected up front and reported as an actionable state instead
    /// of failing halfway through an update.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::ReadOnlyInstall`] when the install location
    /// cannot be written.
    pub fn ensure_writable(&self) -> Result<(), InstallError> {
        let parent = self.install_dir.parent().unwrap_or(&self.install_dir);
        let probe = parent.join(".gcabb-update-write-probe");
        match fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
                Ok(())
            }
            Err(_) => Err(InstallError::ReadOnlyInstall {
                path: parent.to_path_buf(),
            }),
        }
    }

    /// Removes backups left behind by a previous update.
    ///
    /// Called at startup: reaching startup proves the new installation runs, so
    /// the previous one is no longer needed as a rollback target.
    pub fn clean_completed_updates(&self) {
        for path in [&self.backup_root, &self.staging_root] {
            if path.exists()
                && let Err(error) = fs::remove_dir_all(path)
            {
                tracing::warn!(path = %path.display(), %error, "could not clean update directory");
            }
        }
    }
}

/// An unpacked, verified update waiting to be swapped into place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedUpdate {
    pub version: String,
    pub root: PathBuf,
}

/// Unpacks a verified artifact into staging.
///
/// The artifact bytes must already have passed hash and size verification;
/// this function assumes trusted input and is concerned with layout, not
/// authenticity.
///
/// # Errors
///
/// Returns [`InstallError`] when the archive is malformed, contains unsafe
/// paths, or lacks the application executable.
pub fn stage(
    layout: &InstallLayout,
    archive: &[u8],
    format: ArtifactFormat,
    version: &str,
) -> Result<StagedUpdate, InstallError> {
    let root = layout.staging_root.join(version);
    if root.exists() {
        fs::remove_dir_all(&root).map_err(io("clearing the previous staging directory"))?;
    }
    fs::create_dir_all(&root).map_err(io("creating the staging directory"))?;

    match format {
        ArtifactFormat::TarGz => extract_tar_gz(archive, &root)?,
        ArtifactFormat::Zip => extract_zip(archive, &root)?,
    }

    let executable = root.join(executable_name());
    if !executable.is_file() {
        return Err(InstallError::MissingExecutable {
            expected: executable_name().to_owned(),
        });
    }
    ensure_executable_bit(&executable)?;

    Ok(StagedUpdate {
        version: version.to_owned(),
        root,
    })
}

/// Swaps a staged update into the install directory.
///
/// On success the previous installation remains in the backup directory until
/// the next successful startup, which is what makes rollback possible.
///
/// # Errors
///
/// Returns [`InstallError::RolledBack`] when the swap failed but the previous
/// installation was restored, and [`InstallError::RollbackFailed`] when manual
/// recovery is required.
pub fn apply(layout: &InstallLayout, staged: &StagedUpdate) -> Result<(), InstallError> {
    layout.ensure_writable()?;

    if layout.backup_root.exists() {
        fs::remove_dir_all(&layout.backup_root)
            .map_err(io("clearing the previous update backup"))?;
    }
    if let Some(parent) = layout.backup_root.parent() {
        fs::create_dir_all(parent).map_err(io("creating the update backup directory"))?;
    }

    // Move the running installation aside. Renaming a directory that contains a
    // running executable is permitted on all supported platforms; deleting or
    // overwriting it is not, which is why nothing here does either.
    move_dir(&layout.install_dir, &layout.backup_root).map_err(|error| {
        InstallError::RolledBack {
            reason: format!("could not move the current installation aside: {error}"),
        }
    })?;

    if let Err(error) = move_dir(&staged.root, &layout.install_dir) {
        // Put the old installation back before reporting failure.
        return Err(match move_dir(&layout.backup_root, &layout.install_dir) {
            Ok(()) => InstallError::RolledBack {
                reason: format!("could not move the new version into place: {error}"),
            },
            Err(restore) => InstallError::RollbackFailed {
                install: layout.install_dir.clone(),
                backup: layout.backup_root.clone(),
                reason: format!("{error}; restore also failed: {restore}"),
            },
        });
    }

    Ok(())
}

/// Restores the backed-up installation, undoing an applied update.
///
/// # Errors
///
/// Returns [`InstallError`] when the backup is missing or cannot be restored.
pub fn rollback(layout: &InstallLayout) -> Result<(), InstallError> {
    if !layout.backup_root.exists() {
        return Err(InstallError::RolledBack {
            reason: "no backup of a previous installation is available".to_owned(),
        });
    }
    if layout.install_dir.exists() {
        fs::remove_dir_all(&layout.install_dir).map_err(io("removing the failed installation"))?;
    }
    move_dir(&layout.backup_root, &layout.install_dir)
        .map_err(io("restoring the previous installation"))
}

/// Moves a directory, falling back to copy-then-delete across filesystems.
fn move_dir(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    fs::remove_dir_all(from)
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = entry.metadata()?.permissions().mode();
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

/// Resolves an archive entry against the extraction root, rejecting any path
/// that would escape it.
///
/// Archive entries are attacker-controlled in the general case, so `..` and
/// absolute paths are refused outright rather than normalised away.
fn safe_join(root: &Path, entry: &Path) -> Result<PathBuf, InstallError> {
    let mut resolved = root.to_path_buf();
    for component in entry.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(InstallError::UnsafeEntry {
                    entry: entry.display().to_string(),
                });
            }
        }
    }
    Ok(resolved)
}

fn extract_tar_gz(archive: &[u8], root: &Path) -> Result<(), InstallError> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(archive));
    let mut tar = tar::Archive::new(decoder);
    let entries = tar
        .entries()
        .map_err(|error| InstallError::MalformedArchive(error.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| InstallError::MalformedArchive(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| InstallError::MalformedArchive(error.to_string()))?
            .into_owned();
        let target = safe_join(root, &path)?;

        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target).map_err(io("creating a directory from the archive"))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io("creating a directory from the archive"))?;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| InstallError::MalformedArchive(error.to_string()))?;
        fs::write(&target, &bytes).map_err(io("writing a file from the archive"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(mode) = entry.header().mode() {
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn extract_zip(archive: &[u8], root: &Path) -> Result<(), InstallError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive))
        .map_err(|error| InstallError::MalformedArchive(error.to_string()))?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| InstallError::MalformedArchive(error.to_string()))?;
        let raw = entry
            .enclosed_name()
            .ok_or_else(|| InstallError::UnsafeEntry {
                entry: entry.name().to_owned(),
            })?;
        let target = safe_join(root, &raw)?;

        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(io("creating a directory from the archive"))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io("creating a directory from the archive"))?;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| InstallError::MalformedArchive(error.to_string()))?;
        fs::write(&target, &bytes).map_err(io("writing a file from the archive"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

/// Ensures the staged executable is runnable.
///
/// A zip round-trip loses the executable bit on Unix, so it is restored rather
/// than trusted from the archive.
fn ensure_executable_bit(path: &Path) -> Result<(), InstallError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(path)
            .map_err(io("reading the staged executable"))?
            .permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        fs::set_permissions(path, permissions)
            .map_err(io("marking the staged executable runnable"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::path::Path;

    use super::{InstallError, InstallLayout, apply, rollback, safe_join, stage};
    use crate::manifest::ArtifactFormat;
    use crate::version::executable_name;

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *contents).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn zip_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().unix_permissions(0o755);
        for (name, contents) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn install_with(dir: &Path, marker: &[u8]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(executable_name()), marker).unwrap();
    }

    #[test]
    fn a_tar_gz_update_stages_with_the_executable_present() {
        let temp = tempfile::tempdir().unwrap();
        let layout = InstallLayout::for_install_dir(temp.path().join("gcabb"));
        let archive = tar_gz(&[(executable_name(), b"new build"), ("README.md", b"notes")]);

        let staged = stage(&layout, &archive, ArtifactFormat::TarGz, "0.2.0").unwrap();

        assert_eq!(
            fs::read(staged.root.join(executable_name())).unwrap(),
            b"new build"
        );
        assert!(staged.root.join("README.md").is_file());
    }

    #[test]
    fn a_zip_update_stages_with_the_executable_present() {
        let temp = tempfile::tempdir().unwrap();
        let layout = InstallLayout::for_install_dir(temp.path().join("gcabb"));
        let archive = zip_archive(&[(executable_name(), b"new build")]);

        let staged = stage(&layout, &archive, ArtifactFormat::Zip, "0.2.0").unwrap();

        assert_eq!(
            fs::read(staged.root.join(executable_name())).unwrap(),
            b"new build"
        );
    }

    #[test]
    fn an_archive_without_the_executable_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let layout = InstallLayout::for_install_dir(temp.path().join("gcabb"));
        let archive = tar_gz(&[("README.md", b"notes")]);

        assert!(matches!(
            stage(&layout, &archive, ArtifactFormat::TarGz, "0.2.0"),
            Err(InstallError::MissingExecutable { .. })
        ));
    }

    #[test]
    fn archive_entries_cannot_escape_the_staging_directory() {
        let root = Path::new("/tmp/staging");
        assert!(matches!(
            safe_join(root, Path::new("../../etc/passwd")),
            Err(InstallError::UnsafeEntry { .. })
        ));
        assert!(safe_join(root, Path::new("nested/file")).is_ok());
    }

    #[test]
    fn applying_an_update_replaces_the_installation_and_keeps_a_backup() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("gcabb");
        install_with(&install, b"old build");
        let layout = InstallLayout::for_install_dir(&install);
        let archive = tar_gz(&[(executable_name(), b"new build")]);
        let staged = stage(&layout, &archive, ArtifactFormat::TarGz, "0.2.0").unwrap();

        apply(&layout, &staged).unwrap();

        assert_eq!(
            fs::read(install.join(executable_name())).unwrap(),
            b"new build"
        );
        assert_eq!(
            fs::read(layout.backup_root.join(executable_name())).unwrap(),
            b"old build"
        );
    }

    #[test]
    fn rollback_restores_the_previous_installation() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("gcabb");
        install_with(&install, b"old build");
        let layout = InstallLayout::for_install_dir(&install);
        let archive = tar_gz(&[(executable_name(), b"new build")]);
        let staged = stage(&layout, &archive, ArtifactFormat::TarGz, "0.2.0").unwrap();
        apply(&layout, &staged).unwrap();

        rollback(&layout).unwrap();

        assert_eq!(
            fs::read(install.join(executable_name())).unwrap(),
            b"old build"
        );
    }

    #[test]
    fn startup_cleanup_removes_backup_and_staging() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("gcabb");
        install_with(&install, b"old build");
        let layout = InstallLayout::for_install_dir(&install);
        let archive = tar_gz(&[(executable_name(), b"new build")]);
        let staged = stage(&layout, &archive, ArtifactFormat::TarGz, "0.2.0").unwrap();
        apply(&layout, &staged).unwrap();
        assert!(layout.backup_root.exists());

        layout.clean_completed_updates();

        assert!(!layout.backup_root.exists());
        assert!(!layout.staging_root.exists());
        assert!(install.join(executable_name()).is_file());
    }

    #[test]
    fn staging_and_backup_are_siblings_of_the_installation() {
        let layout = InstallLayout::for_install_dir("/opt/gcabb");
        assert_eq!(layout.staging_root.parent().unwrap(), Path::new("/opt"));
        assert_eq!(layout.backup_root.parent().unwrap(), Path::new("/opt"));
    }
}
