//! User-controlled desktop settings.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SETTINGS_FILE: &str = "app-settings.json";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppSettings {
    /// Override for newly created worktrees. `None` keeps the platform default.
    worktrees_root: Option<PathBuf>,
    /// Roots GCABB has used, retained so existing worktrees remain managed.
    managed_worktrees_roots: Vec<PathBuf>,
}

impl AppSettings {
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "app settings unreadable; using defaults");
            Self::default()
        })
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(data_dir)?;
        let encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let path = Self::path(data_dir);
        let temporary = path.with_extension("json.tmp");
        let write_result = (|| {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &path)
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        write_result
    }

    #[must_use]
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    #[must_use]
    pub fn worktrees_root(&self, default_root: &Path) -> PathBuf {
        self.worktrees_root
            .clone()
            .unwrap_or_else(|| default_root.to_owned())
    }

    #[must_use]
    pub fn uses_default_worktrees_root(&self) -> bool {
        self.worktrees_root.is_none()
    }

    /// Change where future worktrees are created while retaining ownership of
    /// worktrees under the previous location.
    pub fn set_worktrees_root(&mut self, root: PathBuf, default_root: &Path) {
        self.remember_root(self.worktrees_root(default_root));
        if root == default_root {
            self.worktrees_root = None;
        } else {
            self.worktrees_root = Some(root.clone());
        }
        self.remember_root(root);
    }

    pub fn use_default_worktrees_root(&mut self, default_root: &Path) {
        self.remember_root(self.worktrees_root(default_root));
        self.worktrees_root = None;
        self.remember_root(default_root.to_owned());
    }

    #[must_use]
    pub fn managed_worktrees_roots(&self, default_root: &Path) -> Vec<PathBuf> {
        let mut roots = self.managed_worktrees_roots.clone();
        roots.push(default_root.to_owned());
        roots.push(self.worktrees_root(default_root));
        roots.sort();
        roots.dedup();
        roots
    }

    #[must_use]
    pub fn managed_root_for(&self, path: &Path, default_root: &Path) -> Option<PathBuf> {
        self.managed_worktrees_roots(default_root)
            .into_iter()
            .filter(|root| {
                path.strip_prefix(root)
                    .is_ok_and(|relative| is_gcabb_worktree_path(relative, false))
            })
            .max_by_key(|root| root.components().count())
    }

    /// Root that owns a generated worktree path.
    ///
    /// Checking GCABB's exact two-component layout prevents a broad configured
    /// root, such as a home directory, from claiming unrelated worktrees.
    #[must_use]
    pub fn owning_root_for_worktree(&self, path: &Path, default_root: &Path) -> Option<PathBuf> {
        self.managed_worktrees_roots(default_root)
            .into_iter()
            .filter(|root| {
                path.strip_prefix(root)
                    .is_ok_and(|relative| is_gcabb_worktree_path(relative, true))
            })
            .max_by_key(|root| root.components().count())
    }

    #[must_use]
    pub fn display_worktree_path(&self, path: &Path, default_root: &Path) -> String {
        self.managed_root_for(path, default_root)
            .and_then(|root| path.strip_prefix(root).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn remember_root(&mut self, root: PathBuf) {
        if !self.managed_worktrees_roots.contains(&root) {
            self.managed_worktrees_roots.push(root);
        }
    }
}

fn is_gcabb_worktree_path(relative: &Path, exact: bool) -> bool {
    let components = relative.components().collect::<Vec<_>>();
    components.len() >= 2
        && (!exact || components.len() == 2)
        && components[1]
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.starts_with("gcabb-"))
}

#[cfg(test)]
mod tests {
    use super::AppSettings;

    #[test]
    fn missing_settings_use_the_default_worktree_location() {
        let data = tempfile::tempdir().unwrap();
        let default = data.path().join("worktrees");
        let settings = AppSettings::load(data.path());

        assert_eq!(settings.worktrees_root(&default), default);
        assert!(settings.uses_default_worktrees_root());
    }

    #[test]
    fn changing_the_location_retains_the_previous_managed_root() {
        let default = std::path::Path::new("/data/worktrees");
        let custom = std::path::PathBuf::from("/fast/worktrees");
        let mut settings = AppSettings::default();

        settings.set_worktrees_root(custom.clone(), default);

        assert_eq!(settings.worktrees_root(default), custom);
        assert_eq!(
            settings.managed_root_for(
                std::path::Path::new("/data/worktrees/repo/gcabb-session"),
                default
            ),
            Some(default.to_owned())
        );
    }

    #[test]
    fn settings_round_trip() {
        let data = tempfile::tempdir().unwrap();
        let default = data.path().join("worktrees");
        let custom = data.path().join("custom");
        let mut settings = AppSettings::default();
        settings.set_worktrees_root(custom, &default);
        settings.save(data.path()).unwrap();

        assert_eq!(AppSettings::load(data.path()), settings);
    }

    #[test]
    fn paths_under_current_and_previous_roots_are_compact() {
        let default = std::path::Path::new("/data/worktrees");
        let custom = std::path::PathBuf::from("/fast/worktrees");
        let mut settings = AppSettings::default();
        settings.set_worktrees_root(custom, default);

        assert_eq!(
            settings.display_worktree_path(
                std::path::Path::new("/fast/worktrees/gcabb/gcabb-session/plan.md"),
                default
            ),
            "gcabb/gcabb-session/plan.md"
        );
        assert_eq!(
            settings.display_worktree_path(
                std::path::Path::new("/data/worktrees/gcabb/gcabb-old-session/plan.md"),
                default
            ),
            "gcabb/gcabb-old-session/plan.md"
        );
    }

    #[test]
    fn broad_roots_do_not_claim_unrelated_worktrees() {
        let default = std::path::Path::new("/data/worktrees");
        let home = std::path::PathBuf::from("/home/developer");
        let mut settings = AppSettings::default();
        settings.set_worktrees_root(home, default);

        assert_eq!(
            settings.owning_root_for_worktree(
                std::path::Path::new("/home/developer/project/manual-worktree"),
                default
            ),
            None
        );
        assert_eq!(
            settings.owning_root_for_worktree(
                std::path::Path::new("/home/developer/project/gcabb-fix-login"),
                default
            ),
            Some(std::path::PathBuf::from("/home/developer"))
        );
        assert_eq!(
            settings.display_worktree_path(
                std::path::Path::new("/home/developer/secrets.txt"),
                default
            ),
            "/home/developer/secrets.txt"
        );
    }
}
