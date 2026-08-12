---
name: create-pr
description: Open a pull request against constructomech/gcabb after reproducing CI locally. Use when asked to "create a PR", open or submit a pull request, push a branch for review, or when finishing work that is meant to land on main.
---

# Create a GCABB Pull Request

Reproduce CI on the submitting machine before the pull request exists. CI's
`Format and lint` job is the most common source of avoidable red builds, and it
runs ordinary cargo tooling that works locally. Do not push a branch and let CI
discover a lint failure.

## 1. Review the change

Confirm the working tree contains only the intended change:

```sh
git status --short
git --no-pager diff --stat origin/main...HEAD
```

## 2. Run the CI `Format and lint` checks

These commands are exactly what `.github/workflows/ci.yml` runs in the
`Format and lint` job. Every one of them must pass:

```sh
bash -n scripts/install-macos.sh scripts/install-linux.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Keep `-D warnings`. Clippy lints that are merely warnings locally are hard
errors in CI — a `clippy::manual_range_contains` warning is what failed the
build on PR #48.

If the workflow file changes, re-read it and run whatever it actually runs
rather than trusting this list.

## 3. Run the tests native to this machine

CI's `Test` job builds and tests on `ubuntu-22.04`, `macos-14`, and
`windows-2022`. Run the same commands for the host platform you are on:

```sh
cargo build --workspace --locked
cargo test --workspace --locked
```

On Windows, also validate the installer script that job checks:

```powershell
[scriptblock]::Create((Get-Content -Raw scripts/install-windows.ps1)) | Out-Null
```

Use the toolchain pinned in `rust-toolchain.toml` so local results match CI.

Host tests cannot prove the other two platforms pass. A platform-specific
failure may still appear after pushing; that is expected, and it is the only
class of failure CI should be discovering.

### Heavy compilation on shared machines

Where the machine has the shared heavy-job queue, route compilation-based
validation through it instead of running cargo directly:

```powershell
& "$HOME\.copilot\tools\Invoke-HeavyJob.ps1" "Validate pull request" {
    cargo clippy --workspace --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --workspace --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo test --workspace --locked
}
```

`cargo fmt --all -- --check` is cheap and runs directly.

## 4. Fix and re-run

If any check fails, fix the cause and re-run the failed command plus anything
downstream of it. Do not open the pull request with a known-failing check, and
do not silence a lint with `#[allow(...)]` when the flagged code can simply be
written the way clippy suggests.

## 5. Commit and push

Commit with the standard trailer:

```text
<Imperative summary of the change>

Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>
```

Push the branch, then create the pull request:

```sh
git push -u origin <branch>
gh pr create --repo constructomech/gcabb --base main --head <branch> \
    --title "<title>" --body "<body>"
```

The body should say what changed and why, and note anything the local run could
not cover, such as the platforms whose tests were not run.

## 6. Watch the run

Check the pull request's checks and address failures rather than leaving them:

```sh
gh pr checks <number> --repo constructomech/gcabb
gh run view --job <job-id> --log-failed
```

When amending an existing pull request's commits, re-run the checks in steps 2
and 3 before force-pushing, and use `--force-with-lease`.
