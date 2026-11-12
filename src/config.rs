//! `$XDG_CONFIG_HOME/munchymail/config.toml` (default `~/.config`): what
//! munchymail must be told rather than guess, such as each origin's mbox variant.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::mbox::Variant;
use crate::takeout::SOURCE;

/// `$XDG_CONFIG_HOME/munchymail` (default `~/.config/munchymail`); not created.
pub fn dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("munchymail"))
}

pub struct Config {
    pub path: PathBuf,
    table: toml::Table,
}

impl Config {
    /// The config file; a missing one reads as empty.
    pub fn load() -> Result<Config> {
        let path = dir()?.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Config::parse(path, &text)
    }

    fn parse(path: PathBuf, text: &str) -> Result<Config> {
        let table = text
            .parse::<toml::Table>()
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Config { path, table })
    }

    fn origins(&self, account: &str) -> Option<&toml::Table> {
        self.table
            .get("accounts")?
            .get(account)?
            .get("origins")?
            .as_table()
    }

    /// `[accounts."ACCOUNT".origins."ORIGIN"] mbox`; required, there is no default.
    pub fn mbox(&self, account: &str, origin: &str) -> Result<Variant> {
        let v = self
            .origins(account)
            .and_then(|o| o.get(origin))
            .and_then(|o| o.get("mbox"));
        let Some(v) = v else {
            bail!(
                "origin {origin:?} of {account} has no mbox variant; add to {}:\n\n{}",
                self.path.display(),
                Config::mbox_entry(account, origin)
            );
        };
        match v.as_str().and_then(Variant::parse) {
            Some(variant) => Ok(variant),
            None => bail!(
                "origin {origin:?} of {account}: mbox = {v} in {} must be \"dovecot\" or \"mboxrd\"",
                self.path.display()
            ),
        }
    }

    /// Whether the origin's `source` says it is a Takeout of the account's Gmail.
    pub fn is_takeout(&self, account: &str, origin: &str) -> Result<bool> {
        match self
            .origins(account)
            .and_then(|o| o.get(origin))
            .and_then(|o| o.get("source"))
        {
            None => Ok(false),
            Some(v) if v.as_str() == Some(SOURCE) => Ok(true),
            Some(v) => bail!(
                "origin {origin:?} of {account}: source = {v} in {} must be \"{SOURCE}\" or absent",
                self.path.display()
            ),
        }
    }

    /// Labels of the account's origins that are Takeouts of its Gmail.
    pub fn takeouts(&self, account: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for label in self.origins(account).into_iter().flat_map(|o| o.keys()) {
            if self.is_takeout(account, label)? {
                out.push(label.clone());
            }
        }
        Ok(out)
    }

    /// The TOML that sets an origin's variant, for messages.
    pub fn mbox_entry(account: &str, origin: &str) -> String {
        format!(
            "[accounts.\"{account}\".origins.\"{origin}\"]\nmbox = \"dovecot\"  # or \"mboxrd\" (e.g. Google Takeout)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbox_variant_is_required() {
        let c = Config::parse(
            PathBuf::from("config.toml"),
            "[accounts.\"a@x.org\".origins.mail]\nmbox = \"dovecot\"\n\
             [accounts.\"a@x.org\".origins.\"takeout-1\"]\nmbox = \"mboxrd\"\nsource = \"gmail-takeout\"\n\
             [accounts.\"a@x.org\".origins.odd]\nmbox = \"mboxo\"\n",
        )
        .unwrap();
        assert_eq!(c.mbox("a@x.org", "mail").unwrap(), Variant::Dovecot);
        assert_eq!(c.mbox("a@x.org", "takeout-1").unwrap(), Variant::Mboxrd);
        assert!(c.mbox("a@x.org", "odd").is_err());
        assert!(c.mbox("a@x.org", "other").is_err());
        assert!(c.mbox("b@x.org", "mail").is_err());
        assert_eq!(c.takeouts("a@x.org").unwrap(), ["takeout-1"]);
        assert!(!c.is_takeout("a@x.org", "mail").unwrap());
    }
}
