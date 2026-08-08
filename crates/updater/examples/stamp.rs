//! Prints the running build's identity.
//!
//! Used to confirm that release stamping actually reaches the binary: a build
//! produced by the release workflow must report its channel and commit, while a
//! developer build must report itself as `dev` and refuse to update.

fn main() {
    let build = updater::version::BuildStamp::current();
    println!(
        "display={} channel={} release={} target={}",
        build.display(),
        build.channel,
        build.is_release(),
        build.target
    );
}
