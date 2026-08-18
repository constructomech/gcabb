//! Filesystem behaviour the runtime depends on.
//!
//! The runtime stops touching the session directory once this provider is
//! registered, so these cover the operations it performs rather than only the
//! happy path of each method.

use github_copilot_sdk::session_fs::{DirEntryKind, FsErrorKind, SessionFsProvider};
use session_fs::HostSessionFs;
use tempfile::{TempDir, tempdir};

/// A provider whose logical and real roots are the same, so these tests
/// exercise the operations rather than the path mapping.
fn provider() -> (HostSessionFs, TempDir) {
    let directory = tempdir().expect("tempdir");
    let provider = HostSessionFs::new(directory.path(), directory.path());
    (provider, directory)
}

fn path(directory: &TempDir, name: &str) -> String {
    directory.path().join(name).to_string_lossy().into_owned()
}

#[tokio::test]
async fn files_round_trip() {
    let (provider, directory) = provider();
    let file = path(&directory, "notes.txt");

    provider
        .write_file(&file, "first", None)
        .await
        .expect("write");
    assert_eq!(provider.read_file(&file).await.expect("read"), "first");

    provider
        .append_file(&file, " second", None)
        .await
        .expect("append");
    assert_eq!(
        provider.read_file(&file).await.expect("read"),
        "first second"
    );
}

#[tokio::test]
async fn writing_creates_missing_parent_directories() {
    let (provider, directory) = provider();
    // The runtime writes into subdirectories it has not created, so a write
    // that requires a parent must not fail.
    let file = path(&directory, "deep/nested/notes.txt");

    provider
        .write_file(&file, "body", None)
        .await
        .expect("write");

    assert_eq!(provider.read_file(&file).await.expect("read"), "body");
}

#[tokio::test]
async fn appending_to_a_missing_file_creates_it() {
    let (provider, directory) = provider();
    let file = path(&directory, "appended/events.jsonl");

    provider
        .append_file(&file, "line\n", None)
        .await
        .expect("append");

    assert_eq!(provider.read_file(&file).await.expect("read"), "line\n");
}

#[tokio::test]
async fn a_missing_path_reports_not_found_rather_than_a_generic_failure() {
    let (provider, directory) = provider();

    let error = provider
        .read_file(&path(&directory, "absent.txt"))
        .await
        .expect_err("read fails");

    // The SDK maps this kind to ENOENT; anything else would tell the runtime
    // the file exists but could not be read.
    assert!(matches!(error.kind(), FsErrorKind::NotFound(_)));
}

#[tokio::test]
async fn existence_checks_answer_rather_than_fail() {
    let (provider, directory) = provider();
    let file = path(&directory, "present.txt");
    assert!(!provider.exists(&file).await.expect("exists"));

    provider.write_file(&file, "x", None).await.expect("write");

    assert!(provider.exists(&file).await.expect("exists"));
}

#[tokio::test]
async fn stat_distinguishes_files_from_directories() {
    let (provider, directory) = provider();
    let file = path(&directory, "sized.txt");
    provider
        .write_file(&file, "12345", None)
        .await
        .expect("write");

    let file_info = provider.stat(&file).await.expect("stat file");
    assert!(file_info.is_file);
    assert!(!file_info.is_directory);
    assert_eq!(file_info.size, 5);
    assert!(!file_info.mtime.is_empty());

    let directory_info = provider
        .stat(&directory.path().to_string_lossy())
        .await
        .expect("stat directory");
    assert!(directory_info.is_directory);
    assert!(!directory_info.is_file);
}

#[tokio::test]
async fn directories_are_created_and_listed_in_a_stable_order() {
    let (provider, directory) = provider();
    let nested = path(&directory, "outer/inner");
    provider.mkdir(&nested, true, None).await.expect("mkdir");
    provider
        .write_file(&path(&directory, "outer/b.txt"), "b", None)
        .await
        .expect("write b");
    provider
        .write_file(&path(&directory, "outer/a.txt"), "a", None)
        .await
        .expect("write a");

    let entries = provider
        .readdir_with_types(&path(&directory, "outer"))
        .await
        .expect("readdir");

    let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "b.txt", "inner"]);
    assert_eq!(entries[0].kind, DirEntryKind::File);
    assert_eq!(entries[2].kind, DirEntryKind::Directory);

    let plain = provider
        .readdir(&path(&directory, "outer"))
        .await
        .expect("readdir");
    assert_eq!(plain, vec!["a.txt", "b.txt", "inner"]);
}

#[tokio::test]
async fn a_non_recursive_mkdir_will_not_invent_parents() {
    let (provider, directory) = provider();

    let error = provider
        .mkdir(&path(&directory, "missing/child"), false, None)
        .await
        .expect_err("mkdir fails");

    assert!(!error.to_string().is_empty());
}

#[tokio::test]
async fn removal_honours_recursive_and_force() {
    let (provider, directory) = provider();
    let tree = path(&directory, "tree");
    provider
        .write_file(&path(&directory, "tree/child.txt"), "x", None)
        .await
        .expect("write");

    // A populated directory cannot be removed without recursion.
    assert!(provider.rm(&tree, false, false).await.is_err());
    provider.rm(&tree, true, false).await.expect("recursive rm");
    assert!(!provider.exists(&tree).await.expect("exists"));

    // Removing something already gone is only an error without force.
    assert!(provider.rm(&tree, false, false).await.is_err());
    provider
        .rm(&tree, false, true)
        .await
        .expect("forced rm of a missing path");
}

#[tokio::test]
async fn renaming_moves_a_file_and_creates_the_destination_parent() {
    let (provider, directory) = provider();
    let source = path(&directory, "source.txt");
    let destination = path(&directory, "moved/destination.txt");
    provider
        .write_file(&source, "body", None)
        .await
        .expect("write");

    provider
        .rename(&source, &destination)
        .await
        .expect("rename");

    assert!(!provider.exists(&source).await.expect("exists"));
    assert_eq!(
        provider.read_file(&destination).await.expect("read"),
        "body"
    );
}

#[tokio::test]
async fn the_sqlite_capability_is_offered() {
    let (provider, _directory) = provider();
    // The runtime only routes SQL through the provider when it advertises the
    // capability, which is the whole reason for hosting the filesystem.
    assert!(provider.sqlite().is_some());
}

#[tokio::test]
async fn the_database_lives_inside_the_session_root() {
    let directory = tempdir().expect("tempdir");
    let real = directory.path().join("session-a");
    let provider = HostSessionFs::new("/session-state", &real);

    assert_eq!(provider.database().path(), real.join("session.db"));
}

#[tokio::test]
async fn the_shared_state_path_is_served_from_a_per_session_directory() {
    let directory = tempdir().expect("tempdir");
    // The runtime is configured with one session-state path for the whole
    // client, so two sessions ask for the very same path.
    let first = HostSessionFs::new("/session-state", directory.path().join("first"));
    let second = HostSessionFs::new("/session-state", directory.path().join("second"));

    first
        .write_file("/session-state/events.jsonl", "first", None)
        .await
        .expect("write first");
    second
        .write_file("/session-state/events.jsonl", "second", None)
        .await
        .expect("write second");

    // Neither session may see the other's state through that shared path.
    assert_eq!(
        first
            .read_file("/session-state/events.jsonl")
            .await
            .expect("read first"),
        "first"
    );
    assert_eq!(
        second
            .read_file("/session-state/events.jsonl")
            .await
            .expect("read second"),
        "second"
    );
}

#[tokio::test]
async fn project_paths_are_passed_through_untouched() {
    let directory = tempdir().expect("tempdir");
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");
    let source = project.join("main.rs");
    std::fs::write(&source, "fn main() {}").expect("seed");

    let provider = HostSessionFs::new("/session-state", directory.path().join("state"));

    // The agent reads and edits the repository through this same provider, so
    // rewriting these would send its edits somewhere else entirely.
    let read = provider
        .read_file(&source.to_string_lossy())
        .await
        .expect("read project file");
    assert_eq!(read, "fn main() {}");

    provider
        .write_file(&source.to_string_lossy(), "edited", None)
        .await
        .expect("write project file");
    assert_eq!(
        std::fs::read_to_string(&source).expect("read back"),
        "edited"
    );
}

#[tokio::test]
async fn renaming_out_of_session_state_resolves_both_ends() {
    let directory = tempdir().expect("tempdir");
    let state = directory.path().join("state");
    let provider = HostSessionFs::new("/session-state", &state);
    provider
        .write_file("/session-state/from.txt", "body", None)
        .await
        .expect("write");

    provider
        .rename("/session-state/from.txt", "/session-state/nested/to.txt")
        .await
        .expect("rename");

    assert_eq!(
        std::fs::read_to_string(state.join("nested/to.txt")).expect("read"),
        "body"
    );
}
