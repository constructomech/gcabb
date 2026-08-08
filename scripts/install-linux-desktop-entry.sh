#!/usr/bin/env bash
# Install the GCABB desktop entry and icons so Linux desktop environments can
# resolve a taskbar and app-bar icon for the running window.
#
# Wayland compositors cannot receive an icon from the client directly; they match
# the window's xdg_toplevel app ID against an installed desktop entry. X11 uses
# WM_CLASS with the same lookup. Both are set to APP_ID by the desktop binary.
set -euo pipefail

APP_ID="com.constructomech.gcabb"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESOURCES="${REPO_ROOT}/apps/desktop/resources"

DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
DESKTOP_DIR="${DATA_HOME}/applications"
ICON_ROOT="${DATA_HOME}/icons/hicolor"

BIN_PATH="${GCABB_BIN:-}"
if [[ -z "${BIN_PATH}" ]]; then
  for candidate in "${REPO_ROOT}/target/release/gcabb-desktop" "${REPO_ROOT}/target/debug/gcabb-desktop"; do
    if [[ -x "${candidate}" ]]; then
      BIN_PATH="${candidate}"
      break
    fi
  done
fi

if [[ -z "${BIN_PATH}" ]]; then
  echo "error: no gcabb-desktop binary found; build one or set GCABB_BIN" >&2
  exit 1
fi

install -d "${DESKTOP_DIR}"
sed "s|^Exec=.*|Exec=${BIN_PATH}|" \
  "${RESOURCES}/linux/${APP_ID}.desktop" \
  >"${DESKTOP_DIR}/${APP_ID}.desktop"

# Scalable SVG is preferred by most themes; the PNG covers themes that only
# index fixed sizes.
install -d "${ICON_ROOT}/scalable/apps"
install -m 644 "${RESOURCES}/gcabb-icon.svg" "${ICON_ROOT}/scalable/apps/${APP_ID}.svg"

if command -v magick >/dev/null 2>&1; then
  RESIZE_CMD=(magick)
elif command -v convert >/dev/null 2>&1; then
  RESIZE_CMD=(convert)
else
  RESIZE_CMD=()
fi

# Rasterize from the SVG rather than downscaling the PNG: the source vector
# carries the rounded-corner alpha, and `-background none` keeps it instead of
# flattening the icon onto an opaque square.
for size in 16 32 48 64 128 256 512; do
  install -d "${ICON_ROOT}/${size}x${size}/apps"
  if [[ ${#RESIZE_CMD[@]} -gt 0 ]]; then
    "${RESIZE_CMD[@]}" -background none "${RESOURCES}/gcabb-icon.svg" \
      -resize "${size}x${size}" \
      "PNG32:${ICON_ROOT}/${size}x${size}/apps/${APP_ID}.png"
  fi
done

# The prebuilt PNG already carries the rounded-corner alpha; install it when no
# rasterizer is available so at least one raster size is present.
if [[ ${#RESIZE_CMD[@]} -eq 0 ]]; then
  install -d "${ICON_ROOT}/512x512/apps"
  install -m 644 "${RESOURCES}/gcabb-icon.png" "${ICON_ROOT}/512x512/apps/${APP_ID}.png"
  echo "note: ImageMagick not found; installed the 1024x1024 source as 512x512" >&2
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "${ICON_ROOT}" >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${DESKTOP_DIR}" >/dev/null 2>&1 || true
fi

echo "installed ${DESKTOP_DIR}/${APP_ID}.desktop"
echo "installed icons under ${ICON_ROOT}"
echo "Exec=${BIN_PATH}"
