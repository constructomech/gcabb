#!/usr/bin/env bash
# Assemble the macOS GCABB.app bundle around a built gcabb-desktop binary.
#
# macOS only shows an application icon for a bundle, so the released macOS
# payload is a bundle rather than a bare executable. The release workflow calls
# this, and it works the same way locally, so a developer build and a published
# build are assembled from the same inputs.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESOURCES="${REPO_ROOT}/apps/desktop/resources/macos"
BUNDLE_NAME="GCABB.app"
ICON_FILE="GCABB.icns"

usage() {
  cat <<'EOF'
Usage: scripts/package-macos-app.sh [--binary PATH] [--version VERSION] [--out DIR]

Builds GCABB.app from an existing gcabb-desktop binary.

  --binary   Path to gcabb-desktop (default: the newest release then debug build)
  --version  Version recorded in Info.plist (default: the workspace version)
  --out      Directory the bundle is created in (default: target/macos)
EOF
}

binary=""
version=""
out_dir="${REPO_ROOT}/target/macos"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary) binary="${2:?--binary needs a path}"; shift 2 ;;
    --version) version="${2:?--version needs a value}"; shift 2 ;;
    --out) out_dir="${2:?--out needs a path}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 1 ;;
  esac
done

if [[ -z "${binary}" ]]; then
  for candidate in \
    "${REPO_ROOT}/target/release/gcabb-desktop" \
    "${REPO_ROOT}/target/debug/gcabb-desktop"; do
    if [[ -x "${candidate}" ]]; then
      binary="${candidate}"
      break
    fi
  done
fi
if [[ -z "${binary}" || ! -f "${binary}" ]]; then
  echo "error: no gcabb-desktop binary found; build one or pass --binary" >&2
  exit 1
fi

# The workspace Cargo.toml is the single declaration of the version, so the
# bundle reads it from there rather than carrying a second copy.
if [[ -z "${version}" ]]; then
  version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "${REPO_ROOT}/Cargo.toml" | head -n1)"
fi
if [[ -z "${version}" ]]; then
  echo "error: could not read the workspace version; pass --version" >&2
  exit 1
fi

if [[ ! -f "${RESOURCES}/${ICON_FILE}" ]]; then
  echo "error: ${RESOURCES}/${ICON_FILE} is missing; run scripts/make-macos-icns.sh" >&2
  exit 1
fi

bundle="${out_dir}/${BUNDLE_NAME}"
rm -rf "${bundle}"
mkdir -p "${bundle}/Contents/MacOS" "${bundle}/Contents/Resources"

install -m 755 "${binary}" "${bundle}/Contents/MacOS/gcabb-desktop"
install -m 644 "${RESOURCES}/${ICON_FILE}" "${bundle}/Contents/Resources/${ICON_FILE}"
# CFBundleShortVersionString is the version people see; CFBundleVersion must be
# a plain numeric version, so a prerelease tag such as 0.2.0-rc.1 contributes
# only its release part there.
plist="${bundle}/Contents/Info.plist"
install -m 644 "${RESOURCES}/Info.plist" "${plist}"
plutil -replace CFBundleShortVersionString -string "${version}" "${plist}"
plutil -replace CFBundleVersion -string "${version%%-*}" "${plist}"
plutil -lint "${plist}" >/dev/null
printf 'APPL????' >"${bundle}/Contents/PkgInfo"

# An unsigned bundle inherits an ad-hoc signature from the linker, but signing
# explicitly keeps the icon and identity stable after the resources are added;
# without it macOS can keep serving a cached generic icon.
if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "${bundle}" >/dev/null 2>&1 ||
    echo "note: ad-hoc codesign failed; the bundle is still usable" >&2
fi

echo "built ${bundle}"
