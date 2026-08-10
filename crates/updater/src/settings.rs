//! User control over update behaviour.
//!
//! Stored as JSON next to the application's other user data rather than in the
//! session database, so update preferences survive independently of session
//! state and can be read before the rest of the application starts.

use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::version::Channel;

/// File name of the settings document within the data directory.
pub const SETTINGS_FILE: &str = "update-settings.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Whether the client checks for updates on its own.
    ///
    /// When false the user can still check by hand; automatic checking is what
    /// is disabled, not the feature.
    pub automatic_checks: bool,
    /// Channel this installation follows, when overriding its build channel.
    pub channel: Option<Channel>,
    /// A version the user chose to skip.
    pub deferred_version: Option<Version>,
    /// RFC 3339 timestamp of the last completed check.
    pub last_checked_at: Option<String>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            automatic_checks: true,
            channel: None,
            deferred_version: None,
            last_checked_at: None,
        }
    }
}

impl UpdateSettings {
    /// Loads settings, falling back to defaults when absent or unreadable.
    ///
    /// A corrupt settings file must not prevent the application from starting,
    /// so parse failures are logged and defaulted rather than propagated.
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "update settings unreadable; using defaults");
            Self::default()
        })
    }

    /// Writes settings to the data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be written.
    pub fn save(&self, data_dir: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(data_dir)?;
        let encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        std::fs::write(Self::path(data_dir), encoded)
    }

    #[must_use]
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    /// Channel this installation should follow.
    #[must_use]
    pub fn effective_channel(&self, build_channel: Channel) -> Channel {
        self.channel.unwrap_or(build_channel)
    }

    /// Whether a specific version was deferred by the user.
    #[must_use]
    pub fn is_deferred(&self, version: &Version) -> bool {
        self.deferred_version
            .as_ref()
            .is_some_and(|deferred| deferred == version)
    }

    /// Records that the user chose to skip a version.
    pub fn defer(&mut self, version: Version) {
        self.deferred_version = Some(version);
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::UpdateSettings;
    use crate::version::Channel;

    #[test]
    fn automatic_checks_are_on_by_default() {
        assert!(UpdateSettings::default().automatic_checks);
    }

    #[test]
    fn settings_round_trip_through_the_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        let settings = UpdateSettings {
            automatic_checks: false,
            channel: Some(Channel::Prerelease),
            deferred_version: Some(Version::parse("0.3.0").unwrap()),
            ..UpdateSettings::default()
        };
        settings.save(temp.path()).unwrap();

        assert_eq!(UpdateSettings::load(temp.path()), settings);
    }

    #[test]
    fn missing_settings_fall_back_to_defaults() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(UpdateSettings::load(temp.path()), UpdateSettings::default());
    }

    #[test]
    fn corrupt_settings_do_not_prevent_startup() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(UpdateSettings::path(temp.path()), b"{ not json").unwrap();
        assert_eq!(UpdateSettings::load(temp.path()), UpdateSettings::default());
    }

    #[test]
    fn a_configured_channel_overrides_the_build_channel() {
        let mut settings = UpdateSettings::default();
        assert_eq!(settings.effective_channel(Channel::Stable), Channel::Stable);
        settings.channel = Some(Channel::Prerelease);
        assert_eq!(
            settings.effective_channel(Channel::Stable),
            Channel::Prerelease
        );
    }

    #[test]
    fn only_the_deferred_version_is_skipped() {
        let mut settings = UpdateSettings::default();
        settings.defer(Version::parse("0.3.0").unwrap());
        assert!(settings.is_deferred(&Version::parse("0.3.0").unwrap()));
        assert!(!settings.is_deferred(&Version::parse("0.4.0").unwrap()));
    }
}
