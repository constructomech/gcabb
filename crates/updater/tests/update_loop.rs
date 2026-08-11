//! End-to-end exercise of the update loop against a stubbed release feed.
//!
//! This is the closest thing to the Phase 4 exit criteria that can run without
//! publishing a real release: a client at version N discovers version N+1,
//! verifies its signature, downloads and verifies the artifact, swaps it into
//! place, and can roll back. The adversarial cases run through exactly the same
//! path so that a tampered or corrupted release is proven to stop before it
//! reaches the installation.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer as _, SigningKey};
use rand::rngs::OsRng;
use semver::Version;
use updater::install::InstallLayout;
use updater::settings::UpdateSettings;
use updater::source::{BoxFuture, GitHubReleaseSource, HttpClient, ProgressCallback, SourceError};
use updater::verify::{TrustStore, TrustedKey, sha256_hex};
use updater::version::{BuildStamp, Channel, current_target, executable_name};
use updater::{UpdateStatus, Updater};

const KEY_ID: &str = "release-2026";
const API_BASE: &str = "https://api.test";
const REPOSITORY: &str = "constructomech/gcabb";

#[derive(Default)]
struct StubHttp {
    responses: Mutex<HashMap<String, Vec<u8>>>,
}

impl StubHttp {
    fn insert(&self, url: &str, body: Vec<u8>) {
        self.responses
            .lock()
            .expect("stub lock")
            .insert(url.to_owned(), body);
    }
}

impl HttpClient for StubHttp {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, SourceError>> {
        let found = self.responses.lock().expect("stub lock").get(url).cloned();
        Box::pin(async move {
            found.ok_or_else(|| SourceError::Transport(format!("no stub for {url}")))
        })
    }
}

/// Builds a `tar.gz` containing a GCABB executable with the given contents.
fn build_artifact(marker: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(marker.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, executable_name(), marker)
        .expect("append executable");
    let tar = builder.into_inner().expect("finish tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).expect("compress");
    encoder.finish().expect("finish gzip")
}

struct Release {
    version: &'static str,
    artifact: Vec<u8>,
    manifest: Vec<u8>,
}

fn build_release(version: &'static str, marker: &[u8]) -> Release {
    let artifact = build_artifact(marker);
    let manifest = serde_json::json!({
        "schema": 1,
        "version": version,
        "channel": "prerelease",
        "tag": format!("v{version}"),
        "published_at": "2026-08-08T00:00:00Z",
        "notes": "Self-hosting build.",
        "artifacts": [{
            "target": current_target(),
            "format": "tar.gz",
            "url": format!("https://x/{version}/artifact.tar.gz"),
            "size": artifact.len(),
            "sha256": sha256_hex(&artifact),
        }]
    });
    Release {
        version,
        artifact,
        manifest: serde_json::to_vec(&manifest).expect("encode manifest"),
    }
}

/// Wires a stub feed serving one release, returning the updater under test.
fn updater_for(
    install_dir: &Path,
    installed_version: &str,
    release: &Release,
    tamper: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>),
) -> Updater {
    let signing = SigningKey::generate(&mut OsRng);
    let trust = TrustStore::new(vec![
        TrustedKey::from_base64(KEY_ID, &BASE64.encode(signing.verifying_key().to_bytes()))
            .expect("trusted key"),
    ]);

    let signature = serde_json::json!({
        "algorithm": "ed25519",
        "key_id": KEY_ID,
        "signature": BASE64.encode(signing.sign(&release.manifest).to_bytes()),
    });

    let mut manifest_bytes = release.manifest.clone();
    let mut artifact_bytes = release.artifact.clone();
    tamper(&mut manifest_bytes, &mut artifact_bytes);

    let version = release.version;
    let http = Arc::new(StubHttp::default());
    http.insert(
        &format!("{API_BASE}/repos/{REPOSITORY}/releases?per_page=10"),
        serde_json::to_vec(&serde_json::json!([{
            "tag_name": format!("v{version}"),
            "draft": false,
            "prerelease": true,
            "assets": [
                {"name": "update-manifest.json",
                 "browser_download_url": format!("https://x/{version}/m")},
                {"name": "update-manifest.json.sig",
                 "browser_download_url": format!("https://x/{version}/s")}
            ]
        }]))
        .expect("encode feed"),
    );
    http.insert(&format!("https://x/{version}/m"), manifest_bytes);
    http.insert(
        &format!("https://x/{version}/s"),
        serde_json::to_vec(&signature).expect("encode signature"),
    );
    http.insert(
        &format!("https://x/{version}/artifact.tar.gz"),
        artifact_bytes,
    );

    let source = Arc::new(
        GitHubReleaseSource::new(Box::new(Arc::clone(&http)), REPOSITORY).with_api_base(API_BASE),
    );

    Updater::new(
        BuildStamp {
            version: Version::parse(installed_version).expect("installed version"),
            channel: Channel::Prerelease,
            commit: None,
            target: current_target(),
        },
        trust,
        InstallLayout::for_install_dir(install_dir),
        source,
        http,
        UpdateSettings::default(),
    )
}

fn install_existing(dir: &Path, marker: &[u8]) {
    std::fs::create_dir_all(dir).expect("create install dir");
    std::fs::write(dir.join(executable_name()), marker).expect("write executable");
}

fn no_progress() -> ProgressCallback {
    Arc::new(|_, _| {})
}

#[tokio::test]
async fn an_installation_discovers_verifies_and_applies_the_next_release() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.1.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let updater = updater_for(&install, "0.1.0", &release, |_, _| {});

    let status = updater.check(false).await.expect("check");
    let UpdateStatus::Available(available) = status else {
        panic!("expected an available update, got {status:?}");
    };
    assert_eq!(available.manifest.version.to_string(), "0.2.0");

    let staged = updater
        .stage(&available, no_progress())
        .await
        .expect("stage");
    updater.apply(&staged).expect("apply");

    assert_eq!(
        std::fs::read(install.join(executable_name())).expect("read installed"),
        b"version 0.2.0"
    );
    // The replaced installation is retained so the update can be undone.
    assert!(updater.layout().backup_root.exists());
}

#[tokio::test]
async fn a_forged_manifest_never_reaches_the_installation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.1.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let updater = updater_for(&install, "0.1.0", &release, |manifest, _| {
        // Repoint the download at an attacker-controlled URL after signing.
        let text = String::from_utf8(manifest.clone()).expect("utf8");
        *manifest = text
            .replace(
                "https://x/0.2.0/artifact.tar.gz",
                "https://evil/payload.tar.gz",
            )
            .into_bytes();
    });

    let error = updater.check(false).await.expect_err("must reject");

    assert!(
        error.to_string().contains("signature does not match"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(install.join(executable_name())).expect("read installed"),
        b"version 0.1.0"
    );
}

#[tokio::test]
async fn a_corrupted_artifact_never_reaches_the_installation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.1.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let updater = updater_for(&install, "0.1.0", &release, |_, artifact| {
        artifact.truncate(artifact.len() / 2);
    });

    let UpdateStatus::Available(available) = updater.check(false).await.expect("check") else {
        panic!("expected an available update");
    };
    let error = updater
        .stage(&available, no_progress())
        .await
        .expect_err("must reject");

    assert!(
        error.to_string().contains("size mismatch"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(install.join(executable_name())).expect("read installed"),
        b"version 0.1.0"
    );
}

#[tokio::test]
async fn an_installation_already_on_the_latest_release_reports_up_to_date() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.2.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let updater = updater_for(&install, "0.2.0", &release, |_, _| {});

    assert_eq!(
        updater.check(false).await.expect("check"),
        UpdateStatus::UpToDate
    );
}

#[tokio::test]
async fn a_deferred_version_is_not_offered_again() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.1.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let mut updater = updater_for(&install, "0.1.0", &release, |_, _| {});
    updater
        .settings_mut()
        .defer(Version::parse("0.2.0").expect("version"));

    assert_eq!(
        updater.check(false).await.expect("check"),
        UpdateStatus::Deferred(Version::parse("0.2.0").expect("version"))
    );
}

#[tokio::test]
async fn an_update_can_be_rolled_back_after_it_is_applied() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.1.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let updater = updater_for(&install, "0.1.0", &release, |_, _| {});

    let UpdateStatus::Available(available) = updater.check(false).await.expect("check") else {
        panic!("expected an available update");
    };
    let staged = updater
        .stage(&available, no_progress())
        .await
        .expect("stage");
    updater.apply(&staged).expect("apply");
    updater::install::rollback(updater.layout()).expect("rollback");

    assert_eq!(
        std::fs::read(install.join(executable_name())).expect("read installed"),
        b"version 0.1.0"
    );
}

#[tokio::test]
async fn download_progress_is_reported() {
    let temp = tempfile::tempdir().expect("temp dir");
    let install = temp.path().join("gcabb");
    install_existing(&install, b"version 0.1.0");
    let release = build_release("0.2.0", b"version 0.2.0");
    let updater = updater_for(&install, "0.1.0", &release, |_, _| {});

    let UpdateStatus::Available(available) = updater.check(false).await.expect("check") else {
        panic!("expected an available update");
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let progress: ProgressCallback = Arc::new(move |received, total| {
        recorder
            .lock()
            .expect("progress lock")
            .push((received, total));
    });

    updater.stage(&available, progress).await.expect("stage");

    let reported = seen.lock().expect("progress lock").clone();
    assert!(!reported.is_empty(), "progress was never reported");
    let (received, total) = *reported.last().expect("final progress");
    assert_eq!(Some(received), total);
}
