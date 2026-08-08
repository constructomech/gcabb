//! Release-side tooling for building and signing GCABB update metadata.
//!
//! This runs in CI, next to the built artifacts. It exists as a Rust binary
//! rather than a shell script so that the manifest is produced by the same
//! types the client parses, on all three runner platforms, without depending on
//! whatever `openssl`, `python`, or `jq` happens to be installed.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer as _, SigningKey};
use semver::Version;
use updater::manifest::{Artifact, ArtifactFormat, SCHEMA_VERSION, UpdateManifest};
use updater::verify::{ALGORITHM, DetachedSignature, TrustStore, TrustedKey, sha256_hex};
use updater::version::Channel;

/// Environment variable holding the base64 ed25519 signing key.
///
/// Read from the environment so the key reaches the signing step as a CI secret
/// and never as a command line argument, which would appear in process listings
/// and build logs.
const PRIVATE_KEY_ENV: &str = "GCABB_UPDATE_PRIVATE_KEY";

#[derive(Parser)]
#[command(name = "gcabb-release", about = "Build and sign GCABB update metadata")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generates a signing key pair for one-time setup.
    Keygen,
    /// Builds an update manifest from the artifacts in a directory.
    Manifest {
        #[arg(long)]
        version: Version,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        published_at: String,
        /// Base URL that release assets are downloaded from.
        #[arg(long)]
        base_url: String,
        /// Directory containing the built artifacts.
        #[arg(long)]
        artifacts_dir: PathBuf,
        /// File containing the release notes.
        #[arg(long)]
        notes_file: Option<PathBuf>,
        /// Oldest installed version permitted to upgrade directly.
        #[arg(long)]
        minimum_version: Option<Version>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Signs a manifest with the key in the environment.
    Sign {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        key_id: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Verifies a signed manifest, as a release gate.
    Verify {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        signature: PathBuf,
        #[arg(long)]
        key_id: String,
        /// Base64 ed25519 public key that clients will ship.
        #[arg(long)]
        public_key: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Keygen => {
            keygen();
            Ok(())
        }
        Command::Manifest {
            version,
            channel,
            tag,
            published_at,
            base_url,
            artifacts_dir,
            notes_file,
            minimum_version,
            out,
        } => build_manifest(&ManifestArgs {
            version,
            channel,
            tag,
            published_at,
            base_url,
            artifacts_dir,
            notes_file,
            minimum_version,
            out,
        }),
        Command::Sign { input, key_id, out } => sign(&input, &key_id, &out),
        Command::Verify {
            input,
            signature,
            key_id,
            public_key,
        } => verify(&input, &signature, &key_id, &public_key),
    }
}

fn keygen() {
    let signing = SigningKey::generate(&mut rand::rngs::OsRng);
    println!("private key (store as the {PRIVATE_KEY_ENV} secret, never commit it):");
    println!("{}", BASE64.encode(signing.to_bytes()));
    println!();
    println!("public key (build clients with GCABB_UPDATE_PUBLIC_KEY set to this):");
    println!("{}", BASE64.encode(signing.verifying_key().to_bytes()));
}

struct ManifestArgs {
    version: Version,
    channel: String,
    tag: String,
    published_at: String,
    base_url: String,
    artifacts_dir: PathBuf,
    notes_file: Option<PathBuf>,
    minimum_version: Option<Version>,
    out: PathBuf,
}

/// Recovers the target triple from a release artifact file name.
///
/// Artifacts are named `gcabb-<version>-<target>.<ext>`, so the target is
/// whatever sits between the version and the extension.
fn target_from_filename(name: &str, version: &Version) -> Option<(String, ArtifactFormat)> {
    let stem = name.strip_prefix(&format!("gcabb-{version}-"))?;
    if let Some(target) = stem.strip_suffix(".tar.gz") {
        return Some((target.to_owned(), ArtifactFormat::TarGz));
    }
    stem.strip_suffix(".zip")
        .map(|target| (target.to_owned(), ArtifactFormat::Zip))
}

fn build_manifest(args: &ManifestArgs) -> anyhow::Result<()> {
    let channel = Channel::parse(&args.channel)
        .with_context(|| format!("unknown release channel {}", args.channel))?;
    if channel == Channel::Dev {
        bail!("refusing to publish a release on the dev channel");
    }

    let notes = match &args.notes_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading release notes from {}", path.display()))?,
        None => String::new(),
    };

    let mut artifacts = Vec::new();
    let entries = std::fs::read_dir(&args.artifacts_dir)
        .with_context(|| format!("reading {}", args.artifacts_dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((target, format)) = target_from_filename(&name, &args.version) else {
            continue;
        };
        let bytes =
            std::fs::read(entry.path()).with_context(|| format!("reading artifact {name}"))?;
        artifacts.push(Artifact {
            target,
            format,
            url: format!("{}/{name}", args.base_url.trim_end_matches('/')),
            size: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
        });
    }

    if artifacts.is_empty() {
        bail!(
            "no artifacts named gcabb-{}-<target>.(tar.gz|zip) were found in {}",
            args.version,
            args.artifacts_dir.display()
        );
    }
    artifacts.sort_by(|left, right| left.target.cmp(&right.target));

    let manifest = UpdateManifest {
        schema: SCHEMA_VERSION,
        version: args.version.clone(),
        channel,
        tag: args.tag.clone(),
        published_at: args.published_at.clone(),
        notes,
        minimum_version: args.minimum_version.clone(),
        artifacts,
    };

    let encoded = serde_json::to_vec_pretty(&manifest)?;
    write_out(&args.out, &encoded)?;
    println!(
        "wrote {} with {} artifact(s)",
        args.out.display(),
        manifest.artifacts.len()
    );
    Ok(())
}

fn sign(input: &Path, key_id: &str, out: &Path) -> anyhow::Result<()> {
    let secret =
        std::env::var(PRIVATE_KEY_ENV).with_context(|| format!("{PRIVATE_KEY_ENV} is not set"))?;
    let raw = BASE64
        .decode(secret.trim())
        .context("signing key is not valid base64")?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key is not 32 bytes"))?;
    let signing = SigningKey::from_bytes(&bytes);

    let payload = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let signature = DetachedSignature {
        algorithm: ALGORITHM.to_owned(),
        key_id: key_id.to_owned(),
        signature: BASE64.encode(signing.sign(&payload).to_bytes()),
    };

    write_out(out, &serde_json::to_vec_pretty(&signature)?)?;
    println!("signed {} as {}", input.display(), out.display());
    Ok(())
}

/// Verifies a signed manifest exactly as a client would.
///
/// Run in CI before publishing so a key or signing mistake fails the release
/// rather than shipping metadata that every client will reject.
fn verify(
    input: &Path,
    signature_path: &Path,
    key_id: &str,
    public_key: &str,
) -> anyhow::Result<()> {
    let payload = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let signature: DetachedSignature = serde_json::from_slice(&std::fs::read(signature_path)?)
        .context("parsing the signature file")?;
    let store = TrustStore::new(vec![
        TrustedKey::from_base64(key_id, public_key).context("parsing the public key")?,
    ]);
    store
        .verify(&payload, &signature)
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    let manifest: UpdateManifest =
        serde_json::from_slice(&payload).context("parsing the manifest")?;
    println!(
        "verified {} for {} on {} with {} artifact(s)",
        manifest.tag,
        manifest.version,
        manifest.channel,
        manifest.artifacts.len()
    );
    for artifact in &manifest.artifacts {
        println!("  {} {}", artifact.target, artifact.sha256);
    }
    Ok(())
}

fn write_out(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use semver::Version;
    use updater::manifest::ArtifactFormat;

    use super::target_from_filename;

    #[test]
    fn artifact_names_yield_target_and_format() {
        let version = Version::parse("0.2.0").unwrap();
        assert_eq!(
            target_from_filename("gcabb-0.2.0-x86_64-unknown-linux-gnu.tar.gz", &version),
            Some(("x86_64-unknown-linux-gnu".to_owned(), ArtifactFormat::TarGz))
        );
        assert_eq!(
            target_from_filename("gcabb-0.2.0-x86_64-pc-windows-msvc.zip", &version),
            Some(("x86_64-pc-windows-msvc".to_owned(), ArtifactFormat::Zip))
        );
    }

    #[test]
    fn unrelated_files_are_ignored() {
        let version = Version::parse("0.2.0").unwrap();
        assert_eq!(target_from_filename("checksums.txt", &version), None);
        assert_eq!(
            target_from_filename("gcabb-0.1.0-x86_64-unknown-linux-gnu.tar.gz", &version),
            None
        );
    }
}
