#![allow(clippy::missing_errors_doc)]

//! Git inspection for the session changes view.
//!
//! The plan pins Git access to "Git CLI behind typed Rust argument APIs", so
//! every invocation here builds an explicit argument vector. No user-supplied
//! text is ever concatenated into a shell string.
//!
//! Phase 3 compares the worktree against the session's recorded base and
//! reports committed, staged, unstaged, and untracked changes in one view.
//! Selectable bases and merge-base discovery arrive in Phase 6.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use app_model::changes::{ChangeStage, ChangeStatus, ChangedFile, ChangesView, DiffStats};
use thiserror::Error;

/// Files larger than this have their diff omitted to keep the view responsive.
pub const MAX_DIFF_BYTES: usize = 512 * 1024;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git executable could not be run: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git {command} failed: {stderr}")]
    Command { command: String, stderr: String },
    #[error("path is not inside a git worktree: {0}")]
    NotAWorktree(PathBuf),
    #[error("worktree path already exists: {0}")]
    WorktreePathExists(PathBuf),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// Runs git commands against a single worktree.
#[derive(Clone, Debug)]
pub struct GitService {
    worktree: PathBuf,
}

impl GitService {
    #[must_use]
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
        }
    }

    #[must_use]
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = self.run_raw(args)?;
        if !output.status.success() {
            return Err(GitError::Command {
                command: args.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn run_raw(&self, args: &[&str]) -> Result<Output> {
        Command::new("git")
            .arg("-C")
            .arg(&self.worktree)
            // Keep output stable regardless of the developer's git config.
            .arg("--no-pager")
            .arg("-c")
            .arg("core.quotepath=false")
            .arg("-c")
            .arg("diff.noprefix=false")
            .args(args)
            .output()
            .map_err(GitError::from)
    }

    /// Whether the configured path is inside a git worktree.
    #[must_use]
    pub fn is_worktree(&self) -> bool {
        self.run(&["rev-parse", "--is-inside-work-tree"])
            .is_ok_and(|value| value.trim() == "true")
    }

    pub fn head_commit(&self) -> Result<String> {
        Ok(self.run(&["rev-parse", "HEAD"])?.trim().to_owned())
    }

    pub fn current_branch(&self) -> Result<String> {
        Ok(self
            .run(&["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_owned())
    }

    /// Merge base between `HEAD` and `base_ref`.
    ///
    /// Falls back to the ref itself when no merge base exists, so a detached
    /// or unrelated base still produces a usable comparison.
    pub fn merge_base(&self, base_ref: &str) -> Result<String> {
        match self.run(&["merge-base", "HEAD", base_ref]) {
            Ok(value) => Ok(value.trim().to_owned()),
            Err(_) => Ok(self.run(&["rev-parse", base_ref])?.trim().to_owned()),
        }
    }

    /// Create a linked worktree at `path` on a new branch.
    ///
    /// A session worktree gives the agent its own checkout, so parallel
    /// sessions in one repository cannot fight over the working tree or the
    /// checked-out branch. The branch is created from `base_ref`.
    ///
    /// Returns the branch that was created.
    pub fn create_worktree(&self, path: &Path, branch: &str, base_ref: &str) -> Result<String> {
        let path_string = path.to_string_lossy().into_owned();
        // Resolve the base first so a missing ref fails here with a clear
        // error rather than leaving a half-created worktree behind.
        let base = self
            .merge_base(base_ref)
            .unwrap_or_else(|_| base_ref.to_owned());
        self.run(&["worktree", "add", "-b", branch, &path_string, &base])?;
        Ok(branch.to_owned())
    }

    /// Recreate a missing linked worktree from an existing local branch.
    ///
    /// This never creates a branch or overwrites a path. It is intended for
    /// recovering an app-managed worktree whose directory was removed while its
    /// branch and session history remained.
    pub fn recreate_worktree(&self, path: &Path, branch: &str) -> Result<()> {
        if path.exists() {
            return Err(GitError::WorktreePathExists(path.to_owned()));
        }
        self.run(&["rev-parse", "--verify", &format!("refs/heads/{branch}")])?;
        self.run(&["worktree", "prune"])?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path_string = path.to_string_lossy().into_owned();
        self.run(&["worktree", "add", &path_string, branch])?;
        Ok(())
    }

    /// Whether `branch` already exists in this repository.
    #[must_use]
    pub fn branch_exists(&self, branch: &str) -> bool {
        self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .is_ok_and(|value| !value.trim().is_empty())
    }

    /// Whether this worktree has no staged, unstaged, or untracked changes.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.run(&["status", "--porcelain=v1", "--untracked-files=all"])
            .is_ok_and(|output| output.trim().is_empty())
    }

    /// Remove a linked worktree and prune its registration.
    ///
    /// Refuses when the worktree still contains work, so deleting a session
    /// cannot silently destroy uncommitted changes. Callers should surface the
    /// refusal rather than forcing it.
    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        let path_string = path.to_string_lossy().into_owned();
        self.run(&["worktree", "remove", &path_string])?;
        // Prune leaves the administrative files consistent even when the
        // directory was already gone.
        let _ = self.run(&["worktree", "prune"]);
        Ok(())
    }

    /// Delete a branch only when it has been merged into `base_ref`.
    ///
    /// Unmerged work keeps its branch, so a deleted session never takes
    /// commits with it.
    pub fn delete_branch_if_merged(&self, branch: &str) -> Result<bool> {
        if !self.branch_exists(branch) {
            return Ok(false);
        }
        // `-d` refuses to delete a branch with unmerged commits, which is the
        // safety property we want, so a refusal is a result and not an error.
        Ok(self.run(&["branch", "-d", branch]).is_ok())
    }

    /// Build the complete changes view against `base_ref`.
    ///
    /// `base_ref` may be a branch, tag, or commit. The comparison is made
    /// against the merge base so unrelated commits landing on the base branch
    /// do not appear as session changes.
    #[must_use]
    pub fn changes(&self, base_ref: &str, generated_at: String) -> ChangesView {
        if !self.is_worktree() {
            return ChangesView {
                error: Some(GitError::NotAWorktree(self.worktree.clone()).to_string()),
                generated_at: Some(generated_at),
                ..ChangesView::default()
            };
        }

        match self.collect_changes(base_ref) {
            Ok((base, head, branch, files)) => ChangesView {
                base: Some(base),
                base_label: Some(base_ref.to_owned()),
                head: Some(head),
                branch: Some(branch),
                files,
                generated_at: Some(generated_at),
                error: None,
            },
            Err(error) => ChangesView {
                base_label: Some(base_ref.to_owned()),
                generated_at: Some(generated_at),
                error: Some(error.to_string()),
                ..ChangesView::default()
            },
        }
    }

    fn collect_changes(
        &self,
        base_ref: &str,
    ) -> Result<(String, String, String, Vec<ChangedFile>)> {
        let base = self.merge_base(base_ref)?;
        let head = self.head_commit().unwrap_or_default();
        let branch = self.current_branch().unwrap_or_default();

        let stages = self.stage_map()?;
        let mut files = Vec::new();

        // Tracked changes: base -> working tree. This single diff covers
        // committed, staged, and unstaged changes together, which is what the
        // exit criteria require the view to show accurately.
        for entry in self.numstat(&["diff", "--numstat", "-M", "-z", &base])? {
            let stage = stages
                .iter()
                .find(|(path, _)| *path == entry.path)
                .map_or(ChangeStage::Committed, |(_, stage)| *stage);
            let status = if entry.original_path.is_some() {
                ChangeStatus::Renamed
            } else {
                self.status_for(&base, &entry.path)
                    .unwrap_or(ChangeStatus::Modified)
            };

            let diff = if entry.binary {
                None
            } else {
                self.file_diff(&base, &entry.path)?
            };
            files.push(ChangedFile {
                path: entry.path,
                original_path: entry.original_path,
                status,
                stage,
                stats: DiffStats {
                    insertions: entry.insertions,
                    deletions: entry.deletions,
                },
                diff: diff.clone(),
                binary: entry.binary,
                diff_omitted_reason: if entry.binary {
                    Some("Binary file".to_owned())
                } else if diff.is_none() {
                    Some("Diff exceeds display limit".to_owned())
                } else {
                    None
                },
            });
        }

        // Untracked files are invisible to `git diff`, so add them explicitly.
        for path in self.untracked()? {
            if files.iter().any(|file| file.path == path) {
                continue;
            }
            let (diff, insertions, binary) = self.untracked_diff(&path);
            files.push(ChangedFile {
                path,
                original_path: None,
                status: ChangeStatus::Untracked,
                stage: ChangeStage::Untracked,
                stats: DiffStats {
                    insertions,
                    deletions: 0,
                },
                diff: diff.clone(),
                binary,
                diff_omitted_reason: if binary {
                    Some("Binary file".to_owned())
                } else if diff.is_none() {
                    Some("Diff exceeds display limit".to_owned())
                } else {
                    None
                },
            });
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok((base, head, branch, files))
    }

    /// Map each path reported by `git status` to the stage it lives in.
    fn stage_map(&self) -> Result<Vec<(String, ChangeStage)>> {
        let output = self.run(&["status", "--porcelain=v1", "--untracked-files=all"])?;
        let mut entries = Vec::new();
        for line in output.lines() {
            if line.len() < 4 {
                continue;
            }
            let code = &line[..2];
            let path = unquote(line[3..].trim());
            // Renames report "old -> new"; the new path is what we track.
            let path = path.rsplit(" -> ").next().unwrap_or(&path).to_owned();
            let bytes = code.as_bytes();
            let index = bytes.first().copied().unwrap_or(b' ');
            let worktree = bytes.get(1).copied().unwrap_or(b' ');
            let stage = if code == "??" {
                ChangeStage::Untracked
            } else if worktree != b' ' {
                ChangeStage::Unstaged
            } else if index != b' ' {
                ChangeStage::Staged
            } else {
                ChangeStage::Committed
            };
            entries.push((path, stage));
        }
        Ok(entries)
    }

    fn status_for(&self, base: &str, path: &str) -> Option<ChangeStatus> {
        let output = self
            .run(&["diff", "--name-status", "-M", base, "--", path])
            .ok()?;
        let line = output.lines().next()?;
        let code = line.split('\t').next()?;
        Some(ChangeStatus::from_porcelain(&normalize_name_status(code)))
    }

    fn untracked(&self) -> Result<Vec<String>> {
        let output = self.run(&["ls-files", "--others", "--exclude-standard"])?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(unquote)
            .collect())
    }

    fn numstat(&self, args: &[&str]) -> Result<Vec<NumstatEntry>> {
        let output = self.run(args)?;
        let mut entries = Vec::new();
        // With `-z`, records are NUL-terminated. A normal record is
        // `added\tremoved\tpath\0`; a rename is `added\tremoved\t\0old\0new\0`,
        // which avoids the ambiguous `old => new` form of the text output.
        let mut fields = output.split('\0');
        while let Some(record) = fields.next() {
            if record.is_empty() {
                continue;
            }
            let mut parts = record.split('\t');
            let (Some(added), Some(removed)) = (parts.next(), parts.next()) else {
                continue;
            };
            // git reports binary files as "-\t-\tpath".
            let binary = added == "-" || removed == "-";
            let inline_path = parts.next().unwrap_or_default();
            let (original_path, path) = if inline_path.is_empty() {
                // Rename: the next two NUL-separated fields are old and new.
                let original = fields.next().unwrap_or_default().to_owned();
                let new_path = fields.next().unwrap_or_default().to_owned();
                (Some(original), new_path)
            } else {
                (None, inline_path.to_owned())
            };
            if path.is_empty() {
                continue;
            }
            entries.push(NumstatEntry {
                path,
                original_path,
                insertions: added.parse().unwrap_or(0),
                deletions: removed.parse().unwrap_or(0),
                binary,
            });
        }
        Ok(entries)
    }

    fn file_diff(&self, base: &str, path: &str) -> Result<Option<String>> {
        let diff = self.run(&["diff", "-M", base, "--", path])?;
        Ok(if diff.len() > MAX_DIFF_BYTES {
            None
        } else {
            Some(diff)
        })
    }

    /// Synthesize an add-diff for an untracked file.
    ///
    /// `--no-index` exits non-zero when files differ, which is the expected
    /// case here, so the exit status is deliberately ignored.
    fn untracked_diff(&self, path: &str) -> (Option<String>, u32, bool) {
        let Ok(output) =
            self.run_raw(&["diff", "--no-index", "--numstat", "--", null_device(), path])
        else {
            return (None, 0, false);
        };
        let numstat = String::from_utf8_lossy(&output.stdout);
        let mut parts = numstat.split('\t');
        let added = parts.next().unwrap_or("0");
        let binary = added == "-";
        let insertions = added.parse().unwrap_or(0);
        if binary {
            return (None, 0, true);
        }

        let Ok(output) = self.run_raw(&["diff", "--no-index", "--", null_device(), path]) else {
            return (None, insertions, false);
        };
        let diff = String::from_utf8_lossy(&output.stdout).into_owned();
        if diff.len() > MAX_DIFF_BYTES {
            (None, insertions, false)
        } else {
            (Some(diff), insertions, false)
        }
    }
}

struct NumstatEntry {
    path: String,
    original_path: Option<String>,
    insertions: u32,
    deletions: u32,
    binary: bool,
}

/// Platform null device, used as the "before" side of untracked diffs.
const fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// `git status` reports a single letter; reuse the porcelain mapping by
/// widening it into the two-column form.
fn normalize_name_status(code: &str) -> String {
    let letter = code.chars().next().unwrap_or('M');
    format!(" {letter}")
}

/// Remove the surrounding quotes git adds for paths with unusual characters.
fn unquote(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].replace("\\\"", "\"")
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git command runs");
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        git(path, &["init", "--initial-branch=main"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        fs::write(path.join("base.txt"), "base\n").expect("write");
        git(path, &["add", "."]);
        git(path, &["commit", "-m", "base"]);
        dir
    }

    #[test]
    fn reports_committed_staged_unstaged_and_untracked_changes() {
        let dir = repo();
        let path = dir.path();
        git(path, &["checkout", "-b", "work"]);

        // Committed change on the session branch.
        fs::write(path.join("committed.txt"), "committed\n").expect("write");
        git(path, &["add", "committed.txt"]);
        git(path, &["commit", "-m", "committed"]);

        // Staged change.
        fs::write(path.join("staged.txt"), "staged\n").expect("write");
        git(path, &["add", "staged.txt"]);

        // Unstaged change to a tracked file.
        fs::write(path.join("base.txt"), "base modified\n").expect("write");

        // Untracked file.
        fs::write(path.join("untracked.txt"), "untracked\n").expect("write");

        let service = GitService::new(path);
        let view = service.changes("main", "now".to_owned());

        assert!(view.error.is_none(), "unexpected error: {:?}", view.error);
        assert_eq!(view.files.len(), 4, "files: {:?}", view.files);

        let committed = view.file("committed.txt").expect("committed file");
        assert_eq!(committed.stage, ChangeStage::Committed);
        assert!(
            committed
                .diff
                .as_ref()
                .is_some_and(|d| d.contains("+committed"))
        );

        let staged = view.file("staged.txt").expect("staged file");
        assert_eq!(staged.stage, ChangeStage::Staged);

        let unstaged = view.file("base.txt").expect("unstaged file");
        assert_eq!(unstaged.stage, ChangeStage::Unstaged);
        assert_eq!(unstaged.status, ChangeStatus::Modified);

        let untracked = view.file("untracked.txt").expect("untracked file");
        assert_eq!(untracked.stage, ChangeStage::Untracked);
        assert_eq!(untracked.status, ChangeStatus::Untracked);
        assert!(
            untracked
                .diff
                .as_ref()
                .is_some_and(|d| d.contains("+untracked"))
        );
    }

    #[test]
    fn totals_match_per_file_stats() {
        let dir = repo();
        let path = dir.path();
        fs::write(path.join("base.txt"), "one\ntwo\nthree\n").expect("write");
        let view = GitService::new(path).changes("HEAD", "now".to_owned());
        let totals = view.totals();
        let summed: u32 = view.files.iter().map(|file| file.stats.insertions).sum();
        assert_eq!(totals.insertions, summed);
    }

    #[test]
    fn creates_a_session_worktree_on_a_new_branch() {
        let dir = repo();
        let path = dir.path();
        // The worktree must live outside the repository, but still inside a
        // temporary directory so the test leaves nothing behind.
        let outside = tempfile::tempdir().expect("tempdir");
        let worktree = outside.path().join("session-worktree");
        let service = GitService::new(path);

        assert!(!service.branch_exists("session/one"));
        service
            .create_worktree(&worktree, "session/one", "main")
            .expect("worktree created");

        assert!(worktree.join("base.txt").exists(), "checkout is populated");
        assert!(service.branch_exists("session/one"));

        // The worktree is its own checkout on its own branch, so the two do
        // not share a working tree.
        let session = GitService::new(&worktree);
        assert_eq!(session.current_branch().unwrap(), "session/one");
        assert_eq!(GitService::new(path).current_branch().unwrap(), "main");
    }

    #[test]
    fn recreates_a_missing_worktree_from_its_existing_branch() {
        let dir = repo();
        let outside = tempfile::tempdir().expect("tempdir");
        let worktree = outside.path().join("session-worktree");
        let service = GitService::new(dir.path());
        service
            .create_worktree(&worktree, "session/recover", "main")
            .expect("worktree created");
        service
            .remove_worktree(&worktree)
            .expect("worktree removed");

        assert!(!worktree.exists());
        assert!(service.branch_exists("session/recover"));
        service
            .recreate_worktree(&worktree, "session/recover")
            .expect("worktree recreated");

        assert_eq!(
            GitService::new(&worktree).current_branch().unwrap(),
            "session/recover"
        );
    }

    #[test]
    fn worktree_changes_compare_against_the_base_branch() {
        let dir = repo();
        let path = dir.path();
        let outside = tempfile::tempdir().expect("tempdir");
        let worktree = outside.path().join("session-worktree");
        let service = GitService::new(path);
        service
            .create_worktree(&worktree, "session/two", "main")
            .expect("worktree created");

        fs::write(worktree.join("new.txt"), "from session\n").expect("write");
        let view = GitService::new(&worktree).changes("main", "now".to_owned());
        assert!(view.error.is_none(), "unexpected error: {:?}", view.error);
        assert!(view.file("new.txt").is_some());
    }

    #[test]
    fn non_worktree_path_reports_error_instead_of_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let view = GitService::new(dir.path()).changes("main", "now".to_owned());
        assert!(view.error.is_some());
        assert!(view.files.is_empty());
    }

    #[test]
    fn detects_renames_against_base() {
        let dir = repo();
        let path = dir.path();
        git(path, &["mv", "base.txt", "renamed.txt"]);
        git(path, &["commit", "-m", "rename"]);
        let view = GitService::new(path).changes("HEAD~1", "now".to_owned());
        let renamed = view.file("renamed.txt").expect("renamed file present");
        assert_eq!(renamed.status, ChangeStatus::Renamed);
        assert_eq!(renamed.original_path.as_deref(), Some("base.txt"));
    }
}
