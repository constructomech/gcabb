#!/usr/bin/env bash
# Rehearse the whole update loop locally, without publishing anything.
#
# The parts of Phase 4 that unit tests cannot reach are the ones that depend on
# the operating system: replacing an executable while it is running, and doing
# so identically on Linux, macOS, and Windows. This script builds two real
# GCABB versions, installs the older one, serves a signed release feed from
# localhost, and makes the installed build update itself to the newer one.
#
# It runs on all three platforms so CI can prove the swap semantics on each
# rather than reasoning about them.
#
# Usage: scripts/update-rehearsal.sh [--keep]
set -euo pipefail

FROM_VERSION="9.9.0"
TO_VERSION="9.9.1"
KEY_ID="rehearsal-key"
PORT="${GCABB_REHEARSAL_PORT:-8757}"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

keep=false
[ "${1:-}" = "--keep" ] && keep=true

python_bin=""
for candidate in python3 python; do
  if command -v "$candidate" >/dev/null 2>&1; then
    python_bin="$candidate"
    break
  fi
done
if [ -z "$python_bin" ]; then
  echo "error: python 3 is required to serve the stub release feed" >&2
  exit 1
fi

target=$(rustc -vV | awk '/^host: /{print $2}')
case "$target" in
  *windows*) exe="gcabb-desktop.exe"; format="zip" ;;
  *)         exe="gcabb-desktop";     format="tar.gz" ;;
esac

work=$(mktemp -d)
server_pid=""
original_version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)

cleanup() {
  local status=$?
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
  # The build version is edited in place, so it must always be restored.
  set_workspace_version "$original_version"
  if [ "$keep" = true ]; then
    echo "artifacts kept in $work"
  else
    rm -rf "$work"
  fi
  exit "$status"
}

set_workspace_version() {
  "$python_bin" - "$1" <<'PY'
import re, sys
version = sys.argv[1]
with open("Cargo.toml", encoding="utf-8") as handle:
    text = handle.read()
# Only the [workspace.package] version is rewritten, never a dependency's.
text = re.sub(
    r'(\[workspace\.package\][^\[]*?\nversion = ")[^"]+(")',
    lambda m: m.group(1) + version + m.group(2),
    text,
    count=1,
    flags=re.S,
)
with open("Cargo.toml", "w", encoding="utf-8") as handle:
    handle.write(text)
PY
}

# Builds GCABB stamped as a real prerelease build at the given version.
build_version() {
  local version="$1" outdir="$2"
  set_workspace_version "$version"
  GCABB_RELEASE_CHANNEL=prerelease \
  GCABB_BUILD_COMMIT="rehearsal" \
  GCABB_UPDATE_PUBLIC_KEY="$PUBLIC_KEY" \
  GCABB_UPDATE_KEY_ID="$KEY_ID" \
    cargo build -q -p gcabb-desktop
  mkdir -p "$outdir"
  cp "target/debug/$exe" "$outdir/"
}

package() {
  local version="$1" srcdir="$2" outdir="$3"
  mkdir -p "$outdir"
  if [ "$format" = "zip" ]; then
    if ! command -v 7z >/dev/null 2>&1; then
      echo "error: 7z is required to package a Windows artifact" >&2
      exit 1
    fi
    (cd "$srcdir" && 7z a -tzip -bso0 -bsp0 "$outdir/gcabb-$version-$target.zip" .)
  else
    tar -czf "$outdir/gcabb-$version-$target.tar.gz" -C "$srcdir" .
  fi
}

trap cleanup EXIT
echo "==> rehearsing $FROM_VERSION -> $TO_VERSION on $target"

# 1. A throwaway signing key. The client only trusts the key compiled into it,
#    so the rehearsal needs its own pair rather than the production one.
cargo run -q -p gcabb-release -- keygen > "$work/keys.txt"
PRIVATE_KEY=$(sed -n '2p' "$work/keys.txt")
PUBLIC_KEY=$(sed -n '5p' "$work/keys.txt")
export PUBLIC_KEY

# 2. Build and "install" the older version.
echo "==> building $FROM_VERSION"
build_version "$FROM_VERSION" "$work/install"

# 3. Build and package the newer version as a release artifact.
echo "==> building $TO_VERSION"
build_version "$TO_VERSION" "$work/staging"
package "$TO_VERSION" "$work/staging" "$work/serve/download"
set_workspace_version "$original_version"

# 4. Sign the release exactly as the workflow does.
echo "==> signing the release"
printf 'Rehearsal build.\n' > "$work/notes.md"
cargo run -q -p gcabb-release -- manifest \
  --version "$TO_VERSION" \
  --channel prerelease \
  --tag "v$TO_VERSION" \
  --published-at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --base-url "http://127.0.0.1:$PORT/download" \
  --artifacts-dir "$work/serve/download" \
  --notes-file "$work/notes.md" \
  --out "$work/serve/download/update-manifest.json"

GCABB_UPDATE_PRIVATE_KEY="$PRIVATE_KEY" cargo run -q -p gcabb-release -- sign \
  --input "$work/serve/download/update-manifest.json" \
  --key-id "$KEY_ID" \
  --out "$work/serve/download/update-manifest.json.sig"

# 5. A stub of the GitHub releases feed. The query string is ignored by the
#    static server, so the endpoint is served as a plain file.
mkdir -p "$work/serve/repos/constructomech/gcabb"
cat > "$work/serve/repos/constructomech/gcabb/releases" <<JSON
[
  {
    "tag_name": "v$TO_VERSION",
    "draft": false,
    "prerelease": true,
    "assets": [
      {"name": "update-manifest.json",
       "browser_download_url": "http://127.0.0.1:$PORT/download/update-manifest.json"},
      {"name": "update-manifest.json.sig",
       "browser_download_url": "http://127.0.0.1:$PORT/download/update-manifest.json.sig"}
    ]
  }
]
JSON

# 6. Serve it.
# `exec` so $! is the server itself. Without it the subshell is killed and
# the server survives, holding the port and serving a deleted directory on
# the next run.
(cd "$work/serve" && exec "$python_bin" -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
server_pid=$!
feed_ready=false
for _ in $(seq 1 50); do
  if curl -fsS "http://127.0.0.1:$PORT/repos/constructomech/gcabb/releases" >/dev/null 2>&1; then
    feed_ready=true
    break
  fi
  sleep 0.2
done
if [ "$feed_ready" != true ]; then
  echo "FAIL: the stub release feed did not come up on port $PORT" >&2
  exit 1
fi

export GCABB_UPDATE_API_BASE="http://127.0.0.1:$PORT"
# Git Bash rewrites POSIX paths in arguments but not in environment variables,
# so a native path is needed or the app would resolve this against the drive
# root instead of the temporary directory.
if [ "$format" = "zip" ] && command -v cygpath >/dev/null 2>&1; then
  export GCABB_DATA_DIR="$(cygpath -w "$work/data")"
else
  export GCABB_DATA_DIR="$work/data"
fi

installed="$work/install/$exe"

# 7. The installed build must report the old version before anything happens.
before=$("$installed" --version 2>/dev/null)
echo "==> installed reports: $before"
case "$before" in
  "$FROM_VERSION"*) ;;
  *) echo "FAIL: expected $FROM_VERSION, got $before" >&2; exit 1 ;;
esac

# 8. Discover and apply, as a user pressing Update would.
echo "==> applying the update"
if ! "$installed" --apply-update; then
  echo "FAIL: the update did not apply" >&2
  exit 1
fi

# 9. The previous installation must still be on disk right after the swap, so
#    a build that fails to start can be rolled back to.
backup="$work/.install-update-backup/$exe"
for _ in $(seq 1 100); do
  # Both paths existing means the helper completed both halves of the swap.
  # Starting the new build any earlier could clean the backup while the helper
  # still needs it for rollback.
  [ -f "$backup" ] && [ -f "$installed" ] && break
  sleep 0.1
done
if [ ! -f "$backup" ] || [ ! -f "$installed" ]; then
  echo "FAIL: no rollback copy of the previous installation was kept" >&2
  exit 1
fi
echo "==> rollback copy present after the swap"

# 10. The replaced installation must run and report the new version. This is
#     the assertion that only a real swap on a real platform can satisfy.
after=$("$installed" --version 2>/dev/null)
echo "==> installed now reports: $after"
case "$after" in
  "$TO_VERSION"*) ;;
  *) echo "FAIL: expected $TO_VERSION after updating, got $after" >&2; exit 1 ;;
esac

# 11. Having started, the new build has proven itself, so the rollback copy is
#     released. Leaving it would waste a full installation of disk forever.
if [ -d "$work/.install-update-backup" ]; then
  echo "FAIL: the backup should be cleared once the new build has started" >&2
  exit 1
fi
echo "==> rollback copy released after a successful start"

# 12. A second check must find nothing to do rather than looping.
if "$installed" --check-update; then
  echo "FAIL: a second check should find nothing to do" >&2
  exit 1
fi

echo "==> PASS: $FROM_VERSION updated itself to $TO_VERSION on $target"
