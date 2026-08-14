#!/usr/bin/env bash
# Download and install a published GCABB release on macOS.
set -euo pipefail

REPOSITORY="${GCABB_REPOSITORY:-constructomech/gcabb}"
INSTALL_DIR="${GCABB_INSTALL_DIR:-$HOME/Applications/GCABB}"

usage() {
  cat <<'EOF'
Usage: scripts/install-macos.sh [tag]

Downloads and installs the newest published GCABB release, including
prereleases. Pass a tag such as v0.1.0-rc.1 to install a specific release.

Environment:
  GCABB_INSTALL_DIR  Installation directory (default: ~/Applications/GCABB)
  GCABB_REPOSITORY   GitHub repository (default: constructomech/gcabb)
EOF
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  usage
  exit 0
fi
if [ "$#" -gt 1 ]; then
  usage >&2
  exit 1
fi
if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: this installer supports macOS only" >&2
  exit 1
fi

for command in curl tar; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "error: $command is required" >&2
    exit 1
  fi
done

case "$(uname -m)" in
  arm64 | aarch64) target="aarch64-apple-darwin" ;;
  x86_64)
    echo "error: GCABB releases support Apple Silicon Macs only" >&2
    exit 1
    ;;
  *)
    echo "error: unsupported Mac architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

tag="${1:-}"
if [ -z "$tag" ]; then
  releases=$(
    curl --fail --silent --show-error --location \
      --retry 3 \
      --header "Accept: application/vnd.github+json" \
      --header "User-Agent: gcabb-installer" \
      "https://api.github.com/repos/$REPOSITORY/releases?per_page=1"
  )
  tag=$(
    printf '%s\n' "$releases" |
      sed -nE 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/p' |
      head -n 1
  )
fi

if [ -z "$tag" ]; then
  echo "error: $REPOSITORY has no published releases" >&2
  exit 1
fi
case "$tag" in
  v*) version="${tag#v}" ;;
  *)
    echo "error: release tag must start with v: $tag" >&2
    exit 1
    ;;
esac

asset="gcabb-$version-$target.tar.gz"
download_url="https://github.com/$REPOSITORY/releases/download/$tag/$asset"
parent=$(dirname "$INSTALL_DIR")
name=$(basename "$INSTALL_DIR")
mkdir -p "$parent"
work=$(mktemp -d "${TMPDIR:-/tmp}/gcabb-install.XXXXXX")
staging="$parent/.$name-installing"
backup="$parent/.$name-previous"

cleanup() {
  rm -rf "$work"
  if [ -d "$staging" ]; then
    rm -rf "$staging"
  fi
}
trap cleanup EXIT

if [ -e "$staging" ] || [ -e "$backup" ]; then
  echo "error: a previous install did not finish; inspect:" >&2
  echo "  $staging" >&2
  echo "  $backup" >&2
  exit 1
fi

echo "Downloading GCABB $version for $target..."
curl --fail --show-error --location --retry 3 \
  --progress-bar \
  --speed-limit 1024 \
  --speed-time 30 \
  "$download_url" \
  --output "$work/$asset"

mkdir "$staging"
tar -xzf "$work/$asset" -C "$staging"
app="$staging/GCABB.app"
executable="$app/Contents/MacOS/gcabb-desktop"
if [ ! -f "$executable" ]; then
  echo "error: $asset does not contain GCABB.app" >&2
  exit 1
fi
# The icon is what Finder, the Dock, and the switcher read from the bundle, so
# an archive missing it would install a generic-looking application.
if [ ! -f "$app/Contents/Resources/GCABB.icns" ]; then
  echo "error: GCABB.app in $asset has no application icon" >&2
  exit 1
fi
chmod +x "$executable"

had_install=false
if [ -e "$INSTALL_DIR" ]; then
  mv "$INSTALL_DIR" "$backup"
  had_install=true
fi
if ! mv "$staging" "$INSTALL_DIR"; then
  if [ "$had_install" = true ]; then
    mv "$backup" "$INSTALL_DIR"
  fi
  echo "error: could not install GCABB at $INSTALL_DIR" >&2
  exit 1
fi
if [ "$had_install" = true ]; then
  rm -rf "$backup"
fi

# Launch Services caches the icon per bundle path; touching the installed bundle
# makes it re-read Info.plist instead of showing the previous build's icon.
touch "$INSTALL_DIR/GCABB.app"

echo "Installed GCABB $version at $INSTALL_DIR"
echo "Run:"
printf '  open %q\n' "$INSTALL_DIR/GCABB.app"
printf '  %q\n' "$INSTALL_DIR/GCABB.app/Contents/MacOS/gcabb-desktop"
