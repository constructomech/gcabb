//! Tag-driven update client for GCABB.
//!
//! The update path is deliberately fail-closed at every step. An update is
//! applied only when a release is discovered, its manifest carries a valid
//! signature from a key this build ships, the release is applicable to this
//! install, the artifact matches the size and hash the signed manifest
//! promised, and the unpacked payload contains the expected executable. Any
//! failure leaves the existing installation exactly as it was.

pub mod install;
pub mod manifest;
pub mod settings;
pub mod source;
pub mod verify;
pub mod version;

use std::sync::Arc;

use semver::Version;

use crate::install::{InstallError, InstallLayout, StagedUpdate};
use crate::manifest::{Artifact, UpdateManifest, evaluate};
use crate::settings::UpdateSettings;
use crate::source::{ProgressCallback, ReleaseSource, SourceError};
use crate::verify::{TrustStore, VerifyError, verify_artifact_bytes};
use crate::version::{BuildStamp, Channel};

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Verify(#[from] VerifyError),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error("update manifest could not be parsed: {0}")]
    MalformedManifest(String),
}

/// Why this installation is not checking for updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisabledReason {
    /// A build from a developer checkout.
    DeveloperBuild,
    /// The user turned automatic checks off.
    AutomaticChecksOff,
    /// The build ships no signing keys, so nothing could be trusted anyway.
    NoTrustedKeys,
    /// The installation cannot be replaced in place.
    ReadOnlyInstall,
}

impl DisabledReason {
    /// Message suitable for showing in the UI.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::DeveloperBuild => "Running a developer build, so updates are disabled.",
            Self::AutomaticChecksOff => "Automatic update checks are turned off.",
            Self::NoTrustedKeys => {
                "This build has no update signing key, so updates cannot be verified."
            }
            Self::ReadOnlyInstall => {
                "This installation is managed externally and cannot update itself."
            }
        }
    }
}

/// An update that passed every check and may be offered to the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate {
    pub manifest: UpdateManifest,
    pub artifact: Artifact,
}

/// Outcome of an update check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateStatus {
    Disabled(DisabledReason),
    UpToDate,
    /// An update exists but the user chose to skip this version.
    Deferred(Version),
    Available(Box<AvailableUpdate>),
    /// A newer release exists but cannot be applied to this install.
    Blocked(String),
}

/// Checks for, downloads, and applies updates.
pub struct Updater {
    build: BuildStamp,
    trust: TrustStore,
    layout: InstallLayout,
    source: Arc<dyn ReleaseSource>,
    http: Arc<dyn source::HttpClient>,
    settings: UpdateSettings,
}

impl Updater {
    #[must_use]
    pub fn new(
        build: BuildStamp,
        trust: TrustStore,
        layout: InstallLayout,
        source: Arc<dyn ReleaseSource>,
        http: Arc<dyn source::HttpClient>,
        settings: UpdateSettings,
    ) -> Self {
        Self {
            build,
            trust,
            layout,
            source,
            http,
            settings,
        }
    }

    #[must_use]
    pub const fn build(&self) -> &BuildStamp {
        &self.build
    }

    #[must_use]
    pub const fn settings(&self) -> &UpdateSettings {
        &self.settings
    }

    #[must_use]
    pub const fn settings_mut(&mut self) -> &mut UpdateSettings {
        &mut self.settings
    }

    #[must_use]
    pub const fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    /// Channel this installation follows.
    #[must_use]
    pub fn channel(&self) -> Channel {
        self.settings.effective_channel(self.build.channel)
    }

    /// Reason updates are unavailable, if any.
    ///
    /// Evaluated before any network access so a build that could never install
    /// an update never asks for one.
    #[must_use]
    pub fn disabled_reason(&self, automatic: bool) -> Option<DisabledReason> {
        if !self.build.is_release() {
            return Some(DisabledReason::DeveloperBuild);
        }
        if self.trust.is_empty() {
            return Some(DisabledReason::NoTrustedKeys);
        }
        if automatic && !self.settings.automatic_checks {
            return Some(DisabledReason::AutomaticChecksOff);
        }
        if self.layout.ensure_writable().is_err() {
            return Some(DisabledReason::ReadOnlyInstall);
        }
        None
    }

    /// Looks for an applicable update.
    ///
    /// Pass `automatic = true` for a background check so the user's preference
    /// to disable automatic checking is honoured; a check the user asked for
    /// explicitly should pass `false`.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError`] when discovery or verification fails in a way
    /// that is not a normal "nothing to install" outcome.
    pub async fn check(&self, automatic: bool) -> Result<UpdateStatus, UpdateError> {
        if let Some(reason) = self.disabled_reason(automatic) {
            return Ok(UpdateStatus::Disabled(reason));
        }

        let channel = self.channel();
        let include_prereleases = channel.accepts(Channel::Prerelease);
        let candidates = self.source.candidates(include_prereleases).await?;

        let mut blocked: Option<String> = None;
        for candidate in candidates {
            if candidate.version <= self.build.version {
                // Candidates are newest first, so nothing further can apply.
                break;
            }

            let signed = self.source.manifest(&candidate).await?;
            // Verify before parsing: the signature covers the fetched bytes,
            // and unverified bytes must not influence any decision.
            self.trust.verify(&signed.bytes, &signed.signature)?;
            let manifest: UpdateManifest = serde_json::from_slice(&signed.bytes)
                .map_err(|error| UpdateError::MalformedManifest(error.to_string()))?;

            match evaluate(&manifest, &self.build.version, channel, self.build.target) {
                Ok(()) => {
                    if self.settings.is_deferred(&manifest.version) {
                        return Ok(UpdateStatus::Deferred(manifest.version));
                    }
                    // `evaluate` already confirmed an artifact exists for this
                    // target. Treating its absence as "keep looking" rather
                    // than panicking keeps a malformed manifest from taking
                    // down the application.
                    if let Some(artifact) = manifest.artifact_for(self.build.target).cloned() {
                        return Ok(UpdateStatus::Available(Box::new(AvailableUpdate {
                            manifest,
                            artifact,
                        })));
                    }
                }
                Err(rejection) => {
                    blocked.get_or_insert_with(|| rejection.to_string());
                }
            }
        }

        Ok(blocked.map_or(UpdateStatus::UpToDate, UpdateStatus::Blocked))
    }

    /// Downloads and verifies an update's artifact, then unpacks it to staging.
    ///
    /// Nothing in the installation is touched here, so a failure at any point
    /// costs only the staged bytes.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError`] when the download, its verification, or
    /// unpacking fails.
    pub async fn stage(
        &self,
        update: &AvailableUpdate,
        progress: ProgressCallback,
    ) -> Result<StagedUpdate, UpdateError> {
        let bytes = self.http.download(&update.artifact.url, progress).await?;
        verify_artifact_bytes(&bytes, update.artifact.size, &update.artifact.sha256)?;
        let staged = install::stage(
            &self.layout,
            &bytes,
            update.artifact.format,
            &update.manifest.version.to_string(),
        )?;
        Ok(staged)
    }

    /// Swaps a staged update into place.
    ///
    /// The caller restarts the application afterwards; the replaced
    /// installation is retained until the next successful startup.
    ///
    /// # Errors
    ///
    /// Returns [`UpdateError`] when the swap fails. The previous installation
    /// is restored unless the error says otherwise.
    pub fn apply(&self, staged: &StagedUpdate) -> Result<(), UpdateError> {
        install::apply(&self.layout, staged)?;
        Ok(())
    }

    /// Clears update leftovers now that this build has started successfully.
    pub fn complete_pending_update(&self) {
        self.layout.clean_completed_updates();
    }
}
