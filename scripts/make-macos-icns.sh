#!/usr/bin/env bash
# Regenerate the macOS application icon from the canonical GCABB artwork.
#
# macOS reads the Finder, Dock, and switcher icon from an .icns inside the
# application bundle, so the committed .icns is a build product of the same
# source image Linux and Windows use. Regenerate it whenever the artwork
# changes; the packaging script only copies what this produces.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESOURCES="${REPO_ROOT}/apps/desktop/resources"
OUTPUT="${1:-${RESOURCES}/macos/GCABB.icns}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: iconutil is macOS-only, so the icns must be regenerated on macOS" >&2
  exit 1
fi
for command in iconutil sips; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "error: ${command} is required" >&2
    exit 1
  fi
done

# Rasterize from the vector when a rasterizer is available: the SVG carries the
# rounded-corner alpha at every size. The 1024px PNG carries the same alpha and
# is the fallback, matching what the Linux icon installer does.
if command -v rsvg-convert >/dev/null 2>&1; then
  RENDER=rsvg
elif command -v magick >/dev/null 2>&1; then
  RENDER=magick
else
  RENDER=sips
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/gcabb-icns.XXXXXX")"
iconset="${work}/GCABB.iconset"
mkdir -p "${iconset}"
trap 'rm -rf "${work}"' EXIT

render() {
  local size="$1" out="$2"
  case "${RENDER}" in
    rsvg)
      rsvg-convert -w "${size}" -h "${size}" \
        "${RESOURCES}/gcabb-icon.svg" -o "${out}"
      ;;
    magick)
      magick -background none "${RESOURCES}/gcabb-icon.svg" \
        -resize "${size}x${size}" "PNG32:${out}"
      ;;
    sips)
      sips -s format png -z "${size}" "${size}" \
        "${RESOURCES}/gcabb-icon.png" --out "${out}" >/dev/null
      ;;
  esac
}

# The set macOS expects; a missing size falls back to a blurry rescale, which is
# exactly the stale-looking icon this is meant to avoid.
for base in 16 32 128 256 512; do
  render "${base}" "${iconset}/icon_${base}x${base}.png"
  render "$((base * 2))" "${iconset}/icon_${base}x${base}@2x.png"
done

mkdir -p "$(dirname "${OUTPUT}")"
iconutil --convert icns --output "${OUTPUT}" "${iconset}"
echo "wrote ${OUTPUT}"
