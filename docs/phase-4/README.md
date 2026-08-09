# Phase 4: Tagged Releases and Auto-Update

Phase 4 produces the first installable GCABB and the mechanism that moves an
installation to the next tag. Before this phase the application only ever ran
from a developer checkout, so both halves of the dogfooding loop are built and
validated here, between tag N and tag N+1.

## The loop

```text
  bump Cargo.toml version ──► push tag vN+1
                                   │
                                   ▼
                     .github/workflows/release.yml
                       ├─ prepare   derive version/channel, check tag == Cargo.toml
                       ├─ build     linux + macOS (x64, arm64) + windows
                       ├─ validate  fmt, clippy, workspace tests
                       └─ publish   manifest ──► sign ──► verify ──► GitHub Release
                                   │
                                   ▼
      installed GCABB at vN  ──►  check ──► verify ──► download ──► stage ──► apply
                                   │                                            │
                                   └──────────── restart, resume sessions ◄──────┘
```

## Version as a single source of truth

`[workspace.package] version` in the root `Cargo.toml` is the only declared
version. The binary reads it through `CARGO_PKG_VERSION`, and the release
workflow fails the build if the pushed tag disagrees with it. There is no second
place to update and therefore no way for the tag, the manifest, and the binary
to drift apart.

## Developer builds never update themselves

A build is a release build only if `GCABB_RELEASE_CHANNEL` was set when it was
compiled, which only the release workflow does. Without it the build reports
channel `dev` and refuses to check for or apply updates. This matters because
the running binary in a developer checkout lives in a Cargo target directory the
developer rebuilds constantly; replacing it from a release would destroy their
working build.

```console
$ cargo run -q -p updater --example stamp
display=0.1.0 (dev) channel=dev release=false target=x86_64-unknown-linux-gnu
```

## Trust

Update signing is independent of platform code signing. It works identically on
all three targets and keeps working before OS certificates exist, which is what
Phase 7 adds on top.

- Releases are signed with ed25519 over the exact bytes of
  `update-manifest.json`.
- Clients verify **before** parsing, because the signature covers the
  transmitted bytes and unverified bytes must not influence any decision.
- The public key is compiled into the client. A build without one has an empty
  trust store and accepts nothing, which is the correct failure: an unsigned
  update stream is worse than no updates.
- Artifacts are additionally checked against the size and SHA-256 in the signed
  manifest, so a truncated or substituted download fails before it is unpacked.

Transport security is not treated as sufficient. A release is trusted because it
carries a valid signature from a key the build ships, not because it arrived
over HTTPS from a plausible URL.

### One-time key setup

```sh
cargo run -p gcabb-release -- keygen
```

Then, in repository settings:

| Where | Name | Value |
| --- | --- | --- |
| Secret | `GCABB_UPDATE_PRIVATE_KEY` | the printed private key |
| Variable | `GCABB_UPDATE_PUBLIC_KEY` | the printed public key |
| Variable | `GCABB_UPDATE_KEY_ID` | an identifier, e.g. `release-2026` |

The private key never enters the repository and reaches the signing step only as
a secret in the environment, never as a command line argument, so it cannot
appear in process listings or build logs.

Rotating a key means publishing a client that ships the new public key before
signing releases with the new private key. Clients report an unknown `key_id`
distinctly from a bad signature so a rotation mistake is diagnosable.

## Channels

The tag selects the channel and nothing else does:

| Tag | Channel | GitHub Release |
| --- | --- | --- |
| `v0.2.0` | `stable` | normal |
| `v0.2.0-rc.1` | `prerelease` | prerelease |

Stable clients are offered only stable releases. Prerelease clients accept both,
so a self-hosting install is never stranded behind a promoted build.

## Applying an update

All three platforms use one strategy, because every supported OS allows renaming
a directory that contains a running executable — Windows forbids only
overwriting or deleting the running image:

1. Unpack the verified artifact into a staging directory beside the install.
2. Rename the current installation to a backup directory.
3. Rename staging into the install location.
4. On failure, rename the backup back.
5. On the next successful startup, delete the backup.

Staging and backup are siblings of the install directory rather than children of
the user data directory, because a rename is only atomic within one filesystem
and user data is frequently on a different mount. When a rename across
filesystems is unavoidable the updater falls back to copy-then-delete.

A read-only or system-managed installation is detected up front, before anything
is downloaded, and reported as an actionable state rather than failing halfway
through.

## User control

`update-settings.json` in the data directory holds automatic-check opt-out,
channel override, and a deferred version. It is stored as JSON next to the other
user data rather than in the session database so update preferences survive
independently of session state and can be read before the rest of the
application starts. A corrupt settings file falls back to defaults rather than
preventing startup.

## Cutting a release

```sh
# 1. Set the version in the one place it is declared.
#    Cargo.toml -> [workspace.package] version = "0.2.0"
cargo check --workspace     # refresh Cargo.lock

git commit -am "Release 0.2.0"
git tag v0.2.0
git push origin main --tags
```

The workflow does the rest. `--locked` is used for release builds so the
published binary is built from the committed lockfile.

## Verifying without publishing

The update path is the part of a release that cannot be fixed by a later
release, so it is covered by tests that run the real code end to end against a
stubbed release feed:

```sh
cargo test -p updater
```

`crates/updater/tests/update_loop.rs` drives discovery, verification, download,
staging, application, and rollback, and proves the adversarial cases stop before
the installation is touched:

- a manifest altered after signing (download URL repointed) is rejected;
- a truncated artifact is rejected;
- a signature from an untrusted key is rejected even when it claims a trusted
  key id;
- an installation already on the newest release reports up to date;
- a deferred version is not offered again;
- an applied update can be rolled back.

The banner itself is covered by the desktop interaction tests, which click the
real buttons in a rendered window:

```sh
cargo test -p gcabb-desktop
```

The release tooling can also be exercised locally:

```sh
cargo run -p gcabb-release -- keygen
cargo run -p gcabb-release -- manifest --version 0.2.0 --channel prerelease \
  --tag v0.2.0 --published-at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --base-url https://example.invalid/download --artifacts-dir dist \
  --out dist/update-manifest.json
GCABB_UPDATE_PRIVATE_KEY=... cargo run -p gcabb-release -- sign \
  --input dist/update-manifest.json --key-id release-2026 \
  --out dist/update-manifest.json.sig
cargo run -p gcabb-release -- verify --input dist/update-manifest.json \
  --signature dist/update-manifest.json.sig --key-id release-2026 \
  --public-key ...
```

## The update prompt

A release build checks once at startup, honouring the automatic-check setting.
When an update is offered, a banner appears above the session view:

| State | Banner |
| --- | --- |
| Checking | "Checking for updates…" |
| Offered | "GCABB *x.y.z* is available", first line of the notes, **Update** / **Later** |
| Downloading | "Downloading update… *n*%" |
| Applied | "GCABB *x.y.z* is installed and starts on restart", **Restart** |
| Failed | "Update failed: *reason*", **Dismiss** |

**Later** defers that specific version, so a newer one is still offered.
**Restart** starts the replacement build before this process exits, so a failure
to launch is reported while there is still a window to report it in.

States that are not actionable never take up space: a background check that
finds nothing, and a build that cannot update at all (developer build, no
signing key, read-only install), leave no banner. The latter is logged instead,
since it is a normal deployment rather than an error.

Update work runs on its own thread with its own Tokio runtime rather than on the
session service. An update check is unrelated to session state, must not queue
behind a long-running agent command, and must keep working when the provider has
failed to start — which is exactly when a user is most likely to want a newer
build.

## What Phase 4 does not do

Portable archives, not native installers. Production OS code signing,
notarization, MSI/NSIS, DMG, and distro packaging are Phase 7. The update
mechanism is designed so that adding them changes the packaging step and not the
trust or swap logic.

## Remaining work

**The first tagged release.** Before a tag is pushed, the signing key must exist
in repository settings (see the key setup table above). The workflow fails
loudly when the key or public-key variable is missing rather than publishing
builds no client could verify, so this cannot be forgotten silently.

Until a release exists there is nothing for an installed client to discover, so
the tag-N to tag-N+1 exit criterion can only be met once the key is configured
and the first tag is cut.
