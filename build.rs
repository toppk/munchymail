//! Sets MUNCHYMAIL_COMMIT to the short commit, "-dirty" when tracked files differ.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    let commit = match git(&["rev-parse", "--short", "HEAD"]) {
        Some(sha)
            if git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty()) =>
        {
            format!("{sha}-dirty")
        }
        Some(sha) => sha,
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=MUNCHYMAIL_COMMIT={commit}");
    for p in [".git/HEAD", ".git/index", "src", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
