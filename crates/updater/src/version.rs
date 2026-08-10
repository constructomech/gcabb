//! Build identity: the version, channel, and target of the running binary.
//!
//! The authoritative version is the workspace `[workspace.package] version` in
//! `Cargo.toml`, surfaced here through `CARGO_PKG_VERSION`. Nothing else in the
//! tree is allowed to declare a version; release tooling reads it from cargo
//! metadata so the tag, the manifest, and the binary can never disagree.

use std::fmt;

use semver::Version;
use serde::{Deserialize, Serialize};

/// Release channel a build belongs to.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Promoted builds intended for general use.
    Stable,
    /// Self-hosting builds published ahead of promotion.
    Prerelease,
    /// A build from a developer checkout that was never published.
    #[default]
    Dev,
}

impl Channel {
    /// Parses a channel name, returning `None` for unknown values.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Self::Stable),
            "prerelease" | "pre" => Some(Self::Prerelease),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Prerelease => "prerelease",
            Self::Dev => "dev",
        }
    }

    /// Whether a build on this channel should ever look for updates.
    ///
    /// Developer builds must not: the running binary lives in a Cargo target
    /// directory that the developer rebuilds constantly, and replacing it from
    /// a release would destroy their working build.
    #[must_use]
    pub const fn checks_for_updates(self) -> bool {
        !matches!(self, Self::Dev)
    }

    /// Whether a release published on `self` is an acceptable upgrade for a
    /// client following `self`.
    ///
    /// Prerelease clients accept stable releases too, so a self-hosting install
    /// is never stranded behind a promoted build.
    #[must_use]
    pub const fn accepts(self, release: Self) -> bool {
        match self {
            Self::Stable => matches!(release, Self::Stable),
            Self::Prerelease => matches!(release, Self::Stable | Self::Prerelease),
            Self::Dev => false,
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Identity of the running build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildStamp {
    pub version: Version,
    pub channel: Channel,
    pub commit: Option<String>,
    pub target: &'static str,
}

impl BuildStamp {
    /// Build identity of the currently running binary.
    ///
    /// `GCABB_RELEASE_CHANNEL` is injected by the release workflow. Its absence
    /// is what makes a build a developer build, so `cargo run` can never offer
    /// to replace itself.
    #[must_use]
    pub fn current() -> Self {
        let version =
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0));
        let channel = option_env!("GCABB_RELEASE_CHANNEL")
            .and_then(Channel::parse)
            .unwrap_or(Channel::Dev);
        Self {
            version,
            channel,
            commit: option_env!("GCABB_BUILD_COMMIT").map(str::to_owned),
            target: current_target(),
        }
    }

    /// Whether this build was produced by the release pipeline.
    #[must_use]
    pub const fn is_release(&self) -> bool {
        self.channel.checks_for_updates()
    }

    /// Single-line identity for the UI title bar and diagnostics bundles.
    #[must_use]
    pub fn display(&self) -> String {
        use std::fmt::Write as _;

        let mut text = format!("{}", self.version);
        if self.channel != Channel::Stable {
            let _ = write!(text, " ({})", self.channel);
        }
        if let Some(commit) = &self.commit {
            let short = commit.get(..7).unwrap_or(commit.as_str());
            let _ = write!(text, " {short}");
        }
        text
    }
}

/// Rust target triple of the running binary.
///
/// Resolved from `cfg` rather than a build script so the crate stays buildable
/// without extra build-time plumbing, and so the value is a compile-time
/// constant that cannot drift from the binary it describes.
#[must_use]
pub const fn current_target() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "aarch64-pc-windows-msvc"
    }
    #[cfg(not(any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "windows",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
    )))]
    {
        "unknown"
    }
}

/// File name of the application executable on the running platform.
#[must_use]
pub const fn executable_name() -> &'static str {
    if cfg!(windows) {
        "gcabb-desktop.exe"
    } else {
        "gcabb-desktop"
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, current_target};

    #[test]
    fn dev_builds_never_check_for_updates() {
        assert!(!Channel::Dev.checks_for_updates());
        assert!(Channel::Stable.checks_for_updates());
        assert!(Channel::Prerelease.checks_for_updates());
    }

    #[test]
    fn prerelease_clients_accept_promoted_stable_releases() {
        assert!(Channel::Prerelease.accepts(Channel::Stable));
        assert!(Channel::Prerelease.accepts(Channel::Prerelease));
    }

    #[test]
    fn stable_clients_reject_prereleases() {
        assert!(!Channel::Stable.accepts(Channel::Prerelease));
        assert!(Channel::Stable.accepts(Channel::Stable));
    }

    #[test]
    fn dev_clients_accept_nothing() {
        assert!(!Channel::Dev.accepts(Channel::Stable));
        assert!(!Channel::Dev.accepts(Channel::Prerelease));
    }

    #[test]
    fn channel_names_round_trip() {
        for channel in [Channel::Stable, Channel::Prerelease, Channel::Dev] {
            assert_eq!(Channel::parse(channel.as_str()), Some(channel));
        }
        assert_eq!(Channel::parse("nightly"), None);
    }

    #[test]
    fn the_running_target_is_recognised() {
        assert_ne!(current_target(), "unknown");
    }
}
