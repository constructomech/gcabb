//! The signed update manifest that describes one published release.
//!
//! A manifest is the only thing a client trusts. It is signed as raw bytes, so
//! it must be verified before it is parsed into anything the client acts on,
//! and the exact bytes fetched must be the bytes verified.

use std::fmt;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::version::Channel;

/// Manifest schema version, bumped when the format changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// Packaging format of a release artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactFormat {
    #[serde(rename = "tar.gz")]
    TarGz,
    Zip,
}

impl ArtifactFormat {
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Zip => "zip",
        }
    }

    /// Packaging used for a target triple.
    ///
    /// Windows gets zip because it is the format Windows can expand without
    /// extra tooling; the Unix targets get tar.gz because it preserves the
    /// executable bit, which a zip would silently drop.
    #[must_use]
    pub fn for_target(target: &str) -> Self {
        if target.contains("windows") {
            Self::Zip
        } else {
            Self::TarGz
        }
    }
}

impl fmt::Display for ArtifactFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.extension())
    }
}

/// One downloadable build within a release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Artifact {
    /// Rust target triple this build runs on.
    pub target: String,
    pub format: ArtifactFormat,
    pub url: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the artifact bytes.
    pub sha256: String,
}

/// A published release, as described to update clients.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateManifest {
    pub schema: u32,
    pub version: Version,
    pub channel: Channel,
    pub tag: String,
    pub published_at: String,
    #[serde(default)]
    pub notes: String,
    /// Oldest installed version that can upgrade directly to this release.
    ///
    /// Set when a release changes on-disk state in a way an older client cannot
    /// be trusted to migrate. Clients below it are told to reinstall rather
    /// than being walked into a broken state.
    #[serde(default)]
    pub minimum_version: Option<Version>,
    pub artifacts: Vec<Artifact>,
}

impl UpdateManifest {
    /// Artifact matching a target triple, if this release built for it.
    #[must_use]
    pub fn artifact_for(&self, target: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.target == target)
    }

    /// Whether a client at `installed` is allowed to take this release.
    #[must_use]
    pub fn permits_upgrade_from(&self, installed: &Version) -> bool {
        self.minimum_version
            .as_ref()
            .is_none_or(|minimum| installed >= minimum)
    }
}

/// Why a candidate release was not offered to the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// Manifest schema is newer than this client understands.
    UnsupportedSchema { found: u32, supported: u32 },
    /// Release is on a channel the client does not follow.
    ChannelMismatch { release: Channel, client: Channel },
    /// Release is not newer than what is installed.
    NotNewer {
        release: Version,
        installed: Version,
    },
    /// Release did not build for the client's platform.
    NoArtifact { target: String },
    /// Installed version is too old to upgrade directly.
    UpgradeTooFar {
        installed: Version,
        minimum: Version,
    },
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { found, supported } => write!(
                formatter,
                "update manifest uses schema {found}, this build understands {supported}; \
                 install a newer release manually"
            ),
            Self::ChannelMismatch { release, client } => {
                write!(
                    formatter,
                    "release is on {release}, this build follows {client}"
                )
            }
            Self::NotNewer { release, installed } => {
                write!(
                    formatter,
                    "{release} is not newer than the installed {installed}"
                )
            }
            Self::NoArtifact { target } => {
                write!(formatter, "release has no build for {target}")
            }
            Self::UpgradeTooFar { installed, minimum } => write!(
                formatter,
                "installed version {installed} is older than the minimum {minimum} required to \
                 upgrade directly; reinstall from a current release"
            ),
        }
    }
}

/// Decides whether a verified manifest is an update this client should take.
///
/// Every rejection is explicit rather than a bare `None` so the UI can explain
/// why an install is staying where it is, instead of silently reporting that it
/// is up to date when it is actually stranded.
///
/// # Errors
///
/// Returns the specific [`Rejection`] when the release is not applicable.
pub fn evaluate(
    manifest: &UpdateManifest,
    installed: &Version,
    client_channel: Channel,
    target: &str,
) -> Result<(), Rejection> {
    if manifest.schema > SCHEMA_VERSION {
        return Err(Rejection::UnsupportedSchema {
            found: manifest.schema,
            supported: SCHEMA_VERSION,
        });
    }
    if !client_channel.accepts(manifest.channel) {
        return Err(Rejection::ChannelMismatch {
            release: manifest.channel,
            client: client_channel,
        });
    }
    if manifest.version <= *installed {
        return Err(Rejection::NotNewer {
            release: manifest.version.clone(),
            installed: installed.clone(),
        });
    }
    if !manifest.permits_upgrade_from(installed) {
        return Err(Rejection::UpgradeTooFar {
            installed: installed.clone(),
            minimum: manifest
                .minimum_version
                .clone()
                .unwrap_or_else(|| Version::new(0, 0, 0)),
        });
    }
    if manifest.artifact_for(target).is_none() {
        return Err(Rejection::NoArtifact {
            target: target.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::{Artifact, ArtifactFormat, Rejection, SCHEMA_VERSION, UpdateManifest, evaluate};
    use crate::version::Channel;

    const TARGET: &str = "x86_64-unknown-linux-gnu";

    fn manifest(version: &str, channel: Channel) -> UpdateManifest {
        UpdateManifest {
            schema: SCHEMA_VERSION,
            version: Version::parse(version).unwrap(),
            channel,
            tag: format!("v{version}"),
            published_at: "2026-01-01T00:00:00Z".to_owned(),
            notes: String::new(),
            minimum_version: None,
            artifacts: vec![Artifact {
                target: TARGET.to_owned(),
                format: ArtifactFormat::TarGz,
                url: "https://example.invalid/a.tar.gz".to_owned(),
                size: 10,
                sha256: "00".repeat(32),
            }],
        }
    }

    #[test]
    fn a_newer_release_on_the_followed_channel_is_offered() {
        let candidate = manifest("0.2.0", Channel::Prerelease);
        let installed = Version::parse("0.1.0").unwrap();
        assert_eq!(
            evaluate(&candidate, &installed, Channel::Prerelease, TARGET),
            Ok(())
        );
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        let candidate = manifest("0.1.0", Channel::Prerelease);
        let installed = Version::parse("0.1.0").unwrap();
        assert!(matches!(
            evaluate(&candidate, &installed, Channel::Prerelease, TARGET),
            Err(Rejection::NotNewer { .. })
        ));
    }

    #[test]
    fn a_downgrade_is_never_offered() {
        let candidate = manifest("0.1.0", Channel::Prerelease);
        let installed = Version::parse("0.9.0").unwrap();
        assert!(matches!(
            evaluate(&candidate, &installed, Channel::Prerelease, TARGET),
            Err(Rejection::NotNewer { .. })
        ));
    }

    #[test]
    fn stable_clients_are_not_offered_prereleases() {
        let candidate = manifest("0.2.0", Channel::Prerelease);
        let installed = Version::parse("0.1.0").unwrap();
        assert!(matches!(
            evaluate(&candidate, &installed, Channel::Stable, TARGET),
            Err(Rejection::ChannelMismatch { .. })
        ));
    }

    #[test]
    fn a_release_without_a_build_for_this_platform_is_rejected() {
        let candidate = manifest("0.2.0", Channel::Prerelease);
        let installed = Version::parse("0.1.0").unwrap();
        assert!(matches!(
            evaluate(
                &candidate,
                &installed,
                Channel::Prerelease,
                "aarch64-apple-darwin"
            ),
            Err(Rejection::NoArtifact { .. })
        ));
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_guessed_at() {
        let mut candidate = manifest("0.2.0", Channel::Prerelease);
        candidate.schema = SCHEMA_VERSION + 1;
        let installed = Version::parse("0.1.0").unwrap();
        assert!(matches!(
            evaluate(&candidate, &installed, Channel::Prerelease, TARGET),
            Err(Rejection::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn an_install_below_the_minimum_is_told_to_reinstall() {
        let mut candidate = manifest("0.5.0", Channel::Prerelease);
        candidate.minimum_version = Some(Version::parse("0.3.0").unwrap());
        let installed = Version::parse("0.1.0").unwrap();
        assert!(matches!(
            evaluate(&candidate, &installed, Channel::Prerelease, TARGET),
            Err(Rejection::UpgradeTooFar { .. })
        ));
    }

    #[test]
    fn prerelease_ordering_follows_semver() {
        let candidate = manifest("0.2.0", Channel::Prerelease);
        let installed = Version::parse("0.2.0-rc.1").unwrap();
        assert_eq!(
            evaluate(&candidate, &installed, Channel::Prerelease, TARGET),
            Ok(())
        );
    }

    #[test]
    fn manifests_round_trip_through_json() {
        let candidate = manifest("0.2.0", Channel::Stable);
        let encoded = serde_json::to_vec(&candidate).unwrap();
        let decoded: UpdateManifest = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, candidate);
    }
}
