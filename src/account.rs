//! Mail accounts: each one is `DATA/ADDRESS/` (e.g. `data/me@example.org/`),
//! holding its own database, content store and text index.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// `address` lowercased, if it can name an account directory.
pub fn validate(address: &str) -> Result<String> {
    let a = address.trim().to_ascii_lowercase();
    let ok = a.split('@').count() == 2
        && !a.starts_with(['@', '.'])
        && !a.ends_with('@')
        && !a.contains(['/', '\\'])
        && !a.chars().any(|c| c.is_whitespace() || c.is_control());
    if !ok {
        bail!("account {address:?} must be one mail address, e.g. me@example.org");
    }
    Ok(a)
}

/// Account addresses under `root`: directories holding an `email.sqlite`.
pub fn list(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    for e in entries {
        let e = e?;
        if let Some(name) = e.file_name().to_str()
            && name.contains('@')
            && e.path().join("email.sqlite").is_file()
        {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// The account directory to use: `account` if given, else the only one.
pub fn resolve(root: &Path, account: Option<&str>) -> Result<PathBuf> {
    if root.join("email.sqlite").exists() {
        bail!(
            "{} holds a database from before accounts; move its contents into {}/ADDRESS/",
            root.display(),
            root.display()
        );
    }
    let all = list(root)?;
    match account {
        Some(a) => {
            let a = validate(a)?;
            if !all.contains(&a) {
                bail!(
                    "no account {a} in {}; `mmadm account list` lists them, `account add` adds one",
                    root.display()
                );
            }
            Ok(root.join(a))
        }
        None => match &all[..] {
            [one] => Ok(root.join(one)),
            [] => bail!(
                "no accounts in {}; add one with `mmadm account add ADDRESS`",
                root.display()
            ),
            many => bail!(
                "{} accounts in {} ({}); pick one with --account, or set default_account in config.toml",
                many.len(),
                root.display(),
                many.join(", ")
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_account() {
        let root = std::env::temp_dir().join(format!("munchymail-acct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(resolve(&root, None).is_err());
        for a in ["a@x.org", "b@y.com"] {
            std::fs::create_dir_all(root.join(a)).unwrap();
            std::fs::write(root.join(a).join("email.sqlite"), "").unwrap();
        }
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        assert_eq!(list(&root).unwrap(), ["a@x.org", "b@y.com"]);
        assert!(resolve(&root, None).is_err());
        assert_eq!(
            resolve(&root, Some("B@y.com")).unwrap(),
            root.join("b@y.com")
        );
        assert!(resolve(&root, Some("c@z.net")).is_err());
        std::fs::remove_dir_all(root.join("b@y.com")).unwrap();
        assert_eq!(resolve(&root, None).unwrap(), root.join("a@x.org"));
        std::fs::write(root.join("email.sqlite"), "").unwrap();
        assert!(resolve(&root, None).is_err());
        std::fs::remove_dir_all(&root).unwrap();
        for bad in ["", "x", "a@b@c", "../a@b", "@b", "a@", ".a@b", "a b@c"] {
            assert!(validate(bad).is_err(), "{bad}");
        }
    }
}
