#!/usr/bin/env bash
# Build, test, and run GCABB from inside a running GCABB session.
#
# The Windows counterpart (scripts/self-dev.ps1) exists because Windows locks a
# running executable. Linux and macOS allow replacing a running binary, but
# sharing one target directory between the GCABB you are running and the GCABB
# your session is building still causes long rebuild stalls whenever the two
# use different feature sets or profiles. Using a separate target directory
# keeps a self-hosting session's builds independent of the developer's.
set -euo pipefail

task="${1:-build}"
shift || true

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "${GCABB_SELF_DEV_TARGET_DIR:-}" ]]; then
    # Key by worktree name so concurrent sessions never share a target dir.
    worktree_name="$(basename "$repo_root")"
    cache_home="${XDG_CACHE_HOME:-$HOME/.cache}"
    target_dir="$cache_home/gcabb/self-dev/$worktree_name"
else
    target_dir="$GCABB_SELF_DEV_TARGET_DIR"
fi

mkdir -p "$target_dir"
export CARGO_TARGET_DIR="$target_dir"

echo "GCABB self-development build"
echo "  repo:   $repo_root"
echo "  target: $target_dir"
echo "  task:   $task"

case "$task" in
    build) cargo build --workspace "$@" ;;
    test) cargo test --workspace "$@" ;;
    clippy) cargo clippy --workspace --all-targets "$@" ;;
    fmt) cargo fmt --all --check "$@" ;;
    run) cargo run -p gcabb-desktop "$@" ;;
    *)
        echo "unknown task: $task (expected build, test, clippy, fmt, or run)" >&2
        exit 2
        ;;
esac

echo "cargo $task succeeded"
