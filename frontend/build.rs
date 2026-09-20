//! Embeds `git describe` in the bundle for the site footer (`env!("GIT_DESCRIBE")`).
//! Set GIT_DESCRIBE to override, e.g. in Docker builds where `.git` isn't available.

use std::process::Command;

fn main() {
    let version = std::env::var("GIT_DESCRIBE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            let out = Command::new("git").args(["describe", "--always", "--dirty", "--tags"]).output().ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_DESCRIBE={version}");
    println!("cargo:rerun-if-env-changed=GIT_DESCRIBE");
    // Rebuild when the checkout moves or the working tree changes.
    for p in ["HEAD", "refs", "index"] {
        println!("cargo:rerun-if-changed=../.git/{p}");
    }
}
