---
name: publish-update
description: Publish a signed GCABB desktop update. Use when asked to "publish an update", cut a release, create a release candidate, tag a new build, or ship a newer GCABB client.
---

# Publish a GCABB Update

Use this process for releases from `constructomech/gcabb`. Complete each phase
in order. Do not create or move a tag until the release commit is on `main`.

## 1. Choose the version

Inspect the current workspace version, published releases, and existing tags:

```powershell
Select-String -Path Cargo.toml -Pattern '^version\s*=' | Select-Object -First 1
git --no-pager tag --sort=-version:refname | Select-Object -First 10
gh release list --repo constructomech/gcabb --limit 10
```

Tags are semantic versions prefixed with `v`:

- `v0.2.0` publishes to the stable channel.
- `v0.2.0-rc.1` publishes to the prerelease channel.

If the requested version is ambiguous, ask the user which version to publish.
Never reuse a version that already has a published GitHub Release.

## 2. Prepare the release commit

Change `[workspace.package].version` in the root `Cargo.toml`. Refresh
`Cargo.lock` with Cargo rather than manually changing only selected packages.
Compilation and tests must use the machine's shared heavy-job queue:

```powershell
& "$HOME\.copilot\tools\Invoke-HeavyJob.ps1" "Refresh release lockfile" {
    cargo check --workspace
}
```

Confirm every local workspace package in `Cargo.lock`, including `updater`,
uses the new version. A stale `updater` entry caused `v0.1.0-rc.2` to fail on
every platform because release builds pass `--locked`.

Validate the lockfile without modifying it:

```powershell
cargo metadata --locked --format-version 1 --no-deps | Out-Null
```

Run formatting directly, then run compilation-based validation through the
heavy-job queue:

```powershell
cargo fmt --all -- --check
& "$HOME\.copilot\tools\Invoke-HeavyJob.ps1" "Validate release" {
    cargo clippy --workspace --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo test --workspace --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --release --locked --target x86_64-pc-windows-msvc -p gcabb-desktop
}
```

Commit `Cargo.toml` and `Cargo.lock` together:

```text
Prepare v<version>

Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>
```

## 3. Land the commit before tagging

The release workflow checks out the tag itself. Verify the intended release
commit is reachable from `origin/main`:

```powershell
git fetch origin main --tags
git merge-base --is-ancestor <release-commit> origin/main
```

If it is not on `main`, push the branch and create a pull request. Do not tag
the feature branch and do not tag an unmerged commit. Continue only after the
release commit has landed on `main`.

## 4. Rehearse the update before tagging

Run the cross-platform self-update rehearsal against the release commit on
`main`, then wait for it to finish:

```powershell
$releaseCommit = git rev-parse origin/main
$previousRun = gh run list --repo constructomech/gcabb `
    --workflow update-rehearsal.yml --branch main --commit $releaseCommit `
    --event workflow_dispatch --limit 1 --json databaseId `
    --jq '.[0].databaseId'
gh workflow run update-rehearsal.yml --repo constructomech/gcabb --ref main
do {
    Start-Sleep -Seconds 2
    $run = gh run list --repo constructomech/gcabb `
        --workflow update-rehearsal.yml --branch main --commit $releaseCommit `
        --event workflow_dispatch --limit 1 --json databaseId `
        --jq '.[0].databaseId'
} while (!$run -or $run -eq $previousRun)
gh run watch $run --repo constructomech/gcabb --exit-status
```

Do not create the tag unless the Linux, macOS, and Windows rehearsal jobs all
pass. If any job fails, inspect it with `gh run view $run --log-failed`, make
and land the correction on `main`, then dispatch and pass a new rehearsal run.

## 5. Create and push the tag

Fetch `main` and confirm it still points to the commit that passed rehearsal.
If it advanced, return to step 4 and rehearse the new commit. Otherwise verify
its version and clean lockfile, then create an annotated tag on that exact
commit:

```powershell
git fetch origin main --tags
$currentMain = git rev-parse origin/main
if ($currentMain -ne $releaseCommit) {
    throw "origin/main advanced after rehearsal; rehearse $currentMain before tagging"
}
git --no-pager show origin/main:Cargo.toml |
    Select-String -Pattern '^version\s*=' |
    Select-Object -First 1
git tag -a "v<version>" $releaseCommit -m "GCABB v<version>"
git push origin "v<version>"
```

Pushing `v*` triggers `.github/workflows/release.yml`. The workflow derives the
channel from the version, checks the tag against `Cargo.toml`, builds all
platform artifacts, validates the workspace, signs update metadata, and
publishes the GitHub Release.

## 6. Monitor and verify publication

Find and watch the tag's release run:

```powershell
gh run list --repo constructomech/gcabb --workflow release.yml --limit 10
gh run watch <run-id> --repo constructomech/gcabb --exit-status
```

If it fails, inspect jobs and failed logs before changing anything:

```powershell
gh run view <run-id> --repo constructomech/gcabb --json jobs,conclusion,url
gh run view <run-id> --repo constructomech/gcabb --log-failed
```

After success, verify the release exists and includes at least:

- `gcabb-<version>-x86_64-pc-windows-msvc.zip`
- `update-manifest.json`
- `update-manifest.json.sig`
- `checksums.txt`

```powershell
gh release view "v<version>" --repo constructomech/gcabb
```

Only then report that the update is published. Prerelease clients discover a
newer prerelease on their next launch and offer **Update**, then **Restart**.

## Reusing a failed tag

Rerunning a workflow does not change the commit a tag references. If the tagged
commit is broken, an ordinary rerun fails the same way.

It is technically possible to delete and recreate an unpublished tag on a
corrected commit, then push it again or invoke the workflow manually. Avoid
moving a tag once a GitHub Release or artifacts have been published. Prefer a
new patch or release-candidate tag because tags are expected to be immutable.
