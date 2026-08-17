//! Changes view state: the session worktree compared against its base.
//!
//! The session stores a logical base branch while each refresh resolves its
//! current upstream and merge-base commit.

use serde::{Deserialize, Serialize};

/// How a file changed relative to the base.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

impl ChangeStatus {
    /// Map a git porcelain status code pair to a change status.
    #[must_use]
    pub fn from_porcelain(code: &str) -> Self {
        let code = code.trim();
        if code == "??" {
            return Self::Untracked;
        }
        // Prefer the worktree column, falling back to the index column.
        let bytes = code.as_bytes();
        let index = bytes.first().copied().unwrap_or(b' ');
        let worktree = bytes.get(1).copied().unwrap_or(b' ');
        let effective = if worktree == b' ' { index } else { worktree };
        match effective {
            b'A' => Self::Added,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            _ => Self::Modified,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Untracked => "untracked",
        }
    }
}

/// Where a change currently lives.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStage {
    Committed,
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiffStats {
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedFile {
    pub path: String,
    /// Previous path, for renames.
    pub original_path: Option<String>,
    pub status: ChangeStatus,
    pub stage: ChangeStage,
    pub stats: DiffStats,
    /// Unified diff text. `None` for binary files or when generation failed.
    pub diff: Option<String>,
    pub binary: bool,
    /// Set when the diff was omitted because the file exceeds the size cap.
    pub diff_omitted_reason: Option<String>,
}

/// The session's changes against its recorded base.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangesView {
    /// Commit the comparison is against.
    pub base: Option<String>,
    /// Human-readable base description, e.g. a branch name.
    pub base_label: Option<String>,
    /// Ref actually resolved, e.g. `origin/main` for logical base `main`.
    #[serde(default)]
    pub tracking_ref: Option<String>,
    pub head: Option<String>,
    pub branch: Option<String>,
    #[serde(default)]
    pub files: Vec<ChangedFile>,
    pub generated_at: Option<String>,
    pub error: Option<String>,
}

impl ChangesView {
    #[must_use]
    pub fn totals(&self) -> DiffStats {
        self.files
            .iter()
            .fold(DiffStats::default(), |mut totals, file| {
                totals.insertions += file.stats.insertions;
                totals.deletions += file.stats.deletions;
                totals
            })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    #[must_use]
    pub fn file(&self, path: &str) -> Option<&ChangedFile> {
        self.files.iter().find(|file| file.path == path)
    }
}
