//! Checks that the macOS bundle configuration cannot lose its application icon.
//!
//! `Info.plist` and `GCABB.icns` are plain packaging inputs that no compilation
//! step references, so a rename or a deletion would only surface as a generic
//! icon on a packaged build. These tests fail on a pull request instead, on
//! every platform, because the files live in the repository.

use std::path::{Path, PathBuf};

const ICON_FILE: &str = "GCABB.icns";
/// Identifier the desktop binary reports to the platform.
const APP_ID: &str = "com.constructomech.gcabb";
/// Executable location the updater expects inside a macOS payload.
const BUNDLED_EXECUTABLE: &str = "GCABB.app/Contents/MacOS/gcabb-desktop";

fn resources() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/macos")
}

fn plist() -> String {
    std::fs::read_to_string(resources().join("Info.plist")).expect("read Info.plist")
}

/// Value of the `<string>` entry that follows the given key.
fn plist_string(source: &str, key: &str) -> String {
    let after = source
        .split_once(&format!("<key>{key}</key>"))
        .unwrap_or_else(|| panic!("Info.plist has no {key}"))
        .1;
    let value = after
        .split_once("<string>")
        .expect("a string value follows the key")
        .1;
    value
        .split_once("</string>")
        .expect("the string value is closed")
        .0
        .to_owned()
}

#[test]
fn the_bundle_declares_the_committed_icon() {
    let icon = resources().join(ICON_FILE);
    let bytes = std::fs::read(&icon).expect("the macOS icon asset is committed");

    assert_eq!(
        &bytes[..4],
        b"icns",
        "{} is not an icns file",
        icon.display()
    );
    assert_eq!(plist_string(&plist(), "CFBundleIconFile"), ICON_FILE);
}

#[test]
fn the_icon_carries_every_size_macos_asks_for() {
    let bytes = std::fs::read(resources().join(ICON_FILE)).expect("read the icon");
    let icon = String::from_utf8_lossy(&bytes);

    // Each embedded image is introduced by its type code. A missing size makes
    // macOS rescale a neighbouring one, which is what a blurry or stale-looking
    // icon in the Dock or the switcher usually is.
    for kind in [
        "ic04", "ic05", "ic07", "ic08", "ic09", "ic10", "ic11", "ic12", "ic13", "ic14",
    ] {
        assert!(
            icon.contains(kind),
            "{kind} is missing from {ICON_FILE}; regenerate it with scripts/make-macos-icns.sh"
        );
    }
}

#[test]
fn the_bundle_identity_matches_the_application() {
    let plist = plist();

    assert_eq!(plist_string(&plist, "CFBundleIdentifier"), APP_ID);
    assert_eq!(plist_string(&plist, "CFBundlePackageType"), "APPL");
    assert_eq!(
        format!(
            "GCABB.app/Contents/MacOS/{}",
            plist_string(&plist, "CFBundleExecutable")
        ),
        BUNDLED_EXECUTABLE,
        "Info.plist must name the executable the updater looks for in a payload"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn the_updater_looks_for_the_executable_the_bundle_declares() {
    assert_eq!(
        updater::version::executable_relative_path(),
        BUNDLED_EXECUTABLE
    );
}
