//! IMAP origins, read-only: each folder's messages by UID, kept current with
//! STATUS and QRESYNC, as Gmail's labels are with its history. Folders are
//! opened with EXAMINE and bodies fetched with BODY.PEEK, so nothing on the
//! server changes. Each sync that changed something is an import run whose
//! sightings are the folders (as paths), so `--folder`, `--current`,
//! `changes` and removals cover IMAP as they cover mbox trees.
//!
//! A folder's state (UIDVALIDITY, UIDNEXT, HIGHESTMODSEQ) advances only after
//! its messages are stored; an interrupted sync redoes that folder and skips
//! the UIDs it already has.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use imap_codec::imap_types::{
    command::SelectParameter,
    core::Vec1,
    fetch::{MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName},
    flag::{FlagFetch, FlagNameAttribute},
    mailbox::{ListMailbox, Mailbox},
    response::Capability,
    sequence::SequenceSet,
    status::{StatusDataItem, StatusDataItemName},
};
use io_imap::client::{
    ImapClient, ImapClientStd, ImapClientStdConnectOptions, ImapMailboxWatchStream,
    ImapMailboxWatchStreamOptions,
};
use io_imap::rfc3501::{examine::ImapMailboxExamineOptions, fetch::ImapMessageFetchOptions};
use io_sasl::{mechanism::Sasl, rfc4616::plain::SaslPlainCreds};
use pimalaya_stream::{
    proxy::Proxy,
    tls::{Tls, TlsClient, TlsServer},
};
use secrecy::SecretString;
use tracing::info;
use url::Url;

use crate::config::Config;
use crate::import::{Artifacts, prepare};
use crate::mbox;
use crate::store::{Store, now};

/// Messages fetched per UID FETCH.
const BATCH: usize = 100;

/// Whether an origin root names an IMAP server rather than a path.
pub fn is_root(root: &str) -> bool {
    let lower = root.to_ascii_lowercase();
    lower.starts_with("imaps://") || lower.starts_with("imap://")
}

/// How to reach an origin's server, from its section of config.toml.
#[derive(Clone, Debug)]
pub struct Settings {
    url: Url,
    user: String,
    password_command: Vec<String>,
    client: Option<(PathBuf, PathBuf)>,
    server_cert: Option<PathBuf>,
    /// Folder names to sync, a trailing `*` matching any rest; None for all.
    folders: Option<Vec<String>>,
}

impl Settings {
    pub fn load(config: &Config, account: &str, origin: &str) -> Result<Settings> {
        let section = format!("[accounts.\"{account}\".origins.\"{origin}\"]");
        let t = config.origin(account, origin).with_context(|| {
            format!(
                "{} has no {section}; see docs/sync.md",
                config.path.display()
            )
        })?;
        let text = |key: &str| -> Result<Option<String>> {
            match t.get(key) {
                None => Ok(None),
                Some(v) => v
                    .as_str()
                    .map(|s| Some(s.to_string()))
                    .with_context(|| format!("{section} {key} must be a string")),
            }
        };
        let need = |key: &str| -> Result<String> {
            text(key)?.with_context(|| format!("{section} needs {key}"))
        };
        let url = Url::parse(&need("imap")?).with_context(|| format!("{section} imap"))?;
        let password_command = t
            .get("password_command")
            .and_then(|v| v.as_array())
            .with_context(|| {
                format!("{section} needs password_command = [\"program\", \"arg\", ...]")
            })?
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .filter(|c| !c.is_empty())
            .with_context(|| format!("{section} password_command must be a list of strings"))?;
        let client = match (text("client_cert")?, text("client_key")?) {
            (Some(c), Some(k)) => Some((expand(&c)?, expand(&k)?)),
            (None, None) => None,
            _ => bail!("{section} needs both client_cert and client_key, or neither"),
        };
        let folders = match t.get("folders") {
            None => None,
            Some(v) => Some(
                v.as_array()
                    .and_then(|a| a.iter().map(|f| f.as_str().map(str::to_string)).collect())
                    .with_context(|| format!("{section} folders must be a list of strings"))?,
            ),
        };
        Ok(Settings {
            folders,
            url,
            user: need("user")?,
            password_command,
            client,
            server_cert: text("server_cert")?.map(|p| expand(&p)).transpose()?,
        })
    }
}

impl Settings {
    fn wants(&self, folder: &str) -> bool {
        self.folders.as_ref().is_none_or(|patterns| {
            patterns.iter().any(|p| match p.strip_suffix('*') {
                Some(prefix) => folder.starts_with(prefix),
                None => folder == p,
            })
        })
    }
}

fn expand(path: &str) -> Result<PathBuf> {
    Ok(match path.strip_prefix("~/") {
        Some(rest) => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(rest)
        }
        None => PathBuf::from(path),
    })
}

/// The password, from the configured command, kept in memory only. A command
/// that fails or prints nothing (a keyring still locked after a reboot) is an
/// error worth retrying, not one that needs someone.
fn password(command: &[String]) -> Result<SecretString> {
    let out = std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running the password command {}", command[0]))?;
    let text = String::from_utf8(out.stdout).unwrap_or_default();
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if !out.status.success() || line.is_empty() {
        bail!(
            "the password command {} gave no password (a keyring locked until the owner logs in?); will retry",
            command[0]
        );
    }
    Ok(SecretString::from(line.to_string()))
}

/// Log in, and ENABLE QRESYNC when the server has it.
fn connect(s: &Settings) -> Result<(ImapClientStd, Vec<Capability<'static>>)> {
    let (mut client, caps) = login(s)?;
    if caps.contains(&Capability::QResync) {
        client
            .enable(Vec1::from(
                imap_codec::imap_types::extensions::enable::CapabilityEnable::try_from("QRESYNC")?,
            ))
            .context("ENABLE QRESYNC")?;
    }
    Ok((client, caps))
}

fn login(s: &Settings) -> Result<(ImapClientStd, Vec<Capability<'static>>)> {
    let opts = ImapClientStdConnectOptions {
        tls: Tls {
            server: s.server_cert.clone().map(|cert| TlsServer { cert }),
            client: s.client.clone().map(|(cert, key)| TlsClient { cert, key }),
            ..Tls::default()
        },
        proxy: Proxy::None,
        sasl: Some(Sasl::Plain(SaslPlainCreds {
            authzid: None,
            authcid: s.user.clone(),
            passwd: password(&s.password_command)?,
        })),
        ..ImapClientStdConnectOptions::default()
    };
    ImapClientStd::connect(&s.url, opts)
        .with_context(|| format!("logging in to {} as {}", s.url, s.user))
}

/// Watch the INBOX with IDLE on a connection of its own; read-only (EXAMINE).
pub fn watch_inbox(s: &Settings) -> Result<ImapMailboxWatchStream> {
    // The watch sends its own ENABLE; a second one may leave QRESYNC out of ENABLED.
    let (client, caps) = login(s)?;
    let opts = ImapMailboxWatchStreamOptions {
        shutdown_poll: Duration::from_secs(30),
        ..ImapMailboxWatchStreamOptions::default()
    };
    Ok(client.watch_mailbox(Mailbox::Inbox, &caps, opts)?)
}

#[derive(Debug, Default)]
pub struct Stats {
    /// The origin's first sync: every message fetched.
    pub full: bool,
    pub folders: u64,
    pub changed_folders: u64,
    pub fetched: u64,
    pub new_artifacts: u64,
    pub gone: u64,
}

impl Stats {
    pub fn changed(&self) -> bool {
        self.fetched + self.gone > 0 || self.changed_folders > 0
    }
}

fn name_of(m: &Mailbox) -> String {
    match m {
        Mailbox::Inbox => "INBOX".into(),
        Mailbox::Other(o) => String::from_utf8_lossy(o.as_ref()).into_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
    uid_validity: u32,
    uid_next: u32,
    highest_mod_seq: u64,
}

fn flags_of(items: &[MessageDataItem]) -> Option<String> {
    items.iter().find_map(|i| match i {
        MessageDataItem::Flags(flags) => {
            // \Recent belongs to the session that first sees a message, not to the message.
            let mut f: Vec<String> = flags
                .iter()
                .filter_map(|f| match f {
                    FlagFetch::Flag(flag) => Some(flag.to_string()),
                    FlagFetch::Recent => None,
                })
                .collect();
            f.sort_unstable();
            Some(f.join(" "))
        }
        _ => None,
    })
}

fn uid_of(items: &[MessageDataItem]) -> Option<u32> {
    items.iter().find_map(|i| match i {
        MessageDataItem::Uid(u) => Some(u.get()),
        _ => None,
    })
}

async fn origin_id(store: &Store, label: &str) -> Result<i64> {
    let mut rows = store
        .conn
        .query(
            "SELECT origin_id, root FROM origin WHERE label = ?1",
            (label,),
        )
        .await?;
    let Some(r) = rows.next().await? else {
        bail!("no origin labelled {label:?}");
    };
    let root: String = r.get(1)?;
    if !is_root(&root) {
        bail!("origin {label:?} is {root}, not an IMAP server");
    }
    Ok(r.get(0)?)
}

async fn dirty(store: &Store, origin_id: i64) -> Result<()> {
    store
        .conn
        .execute(
            "INSERT INTO imap_state (origin_id, dirty) VALUES (?1, 1)
             ON CONFLICT (origin_id) DO UPDATE SET dirty = 1",
            (origin_id,),
        )
        .await?;
    Ok(())
}

/// Bring IMAP origin `label` up to date: every folder whose STATUS moved.
pub async fn sync(store: &Store, account: &str, label: &str) -> Result<Stats> {
    let t = Instant::now();
    let origin_id = origin_id(store, label).await?;
    let settings = Settings::load(&Config::load()?, account, label)?;
    let (mut client, caps) = connect(&settings)?;
    let qresync = caps.contains(&Capability::QResync);
    let mut stats = Stats::default();

    let mut stored: HashMap<String, State> = HashMap::new();
    let mut rows = store
        .conn
        .query(
            "SELECT name, uid_validity, uid_next, highest_mod_seq FROM imap_folder
             WHERE origin_id = ?1 AND gone_at IS NULL",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        stored.insert(
            r.get(0)?,
            State {
                uid_validity: r.get::<i64>(1)? as u32,
                uid_next: r.get::<i64>(2)? as u32,
                highest_mod_seq: r.get::<i64>(3)? as u64,
            },
        );
    }
    drop(rows);
    stats.full = stored.is_empty();

    let listing = client
        .list(Mailbox::try_from("")?, ListMailbox::try_from("*")?)
        .context("LIST")?
        .data;
    let mut folders = BTreeMap::new();
    for (mailbox, _, attributes) in listing {
        let unselectable = attributes.iter().any(|a| {
            matches!(a, FlagNameAttribute::Noselect)
                || a.to_string().eq_ignore_ascii_case("\\NonExistent")
        });
        let name = name_of(&mailbox);
        if !unselectable && settings.wants(&name) {
            folders.insert(name, mailbox);
        }
    }
    stats.folders = folders.len() as u64;

    let gone: Vec<String> = stored
        .keys()
        .filter(|n| !folders.contains_key(*n))
        .cloned()
        .collect();
    if !gone.is_empty() {
        store.begin().await?;
        for name in &gone {
            stats.gone += store
                .conn
                .execute(
                    "UPDATE imap_message SET gone_at = ?3
                     WHERE origin_id = ?1 AND folder = ?2 AND gone_at IS NULL",
                    (origin_id, name.as_str(), now()),
                )
                .await?;
            store
                .conn
                .execute(
                    "UPDATE imap_folder SET gone_at = ?3 WHERE origin_id = ?1 AND name = ?2",
                    (origin_id, name.as_str(), now()),
                )
                .await?;
        }
        dirty(store, origin_id).await?;
        store.commit().await?;
        stats.changed_folders += gone.len() as u64;
    }

    let mut art = Artifacts::new(store).await?;
    for (name, mailbox) in &folders {
        let status = client
            .status(
                mailbox.clone(),
                Cow::Borrowed(&[
                    StatusDataItemName::UidValidity,
                    StatusDataItemName::UidNext,
                    StatusDataItemName::HighestModSeq,
                ]),
            )
            .with_context(|| format!("STATUS {name}"))?
            .data;
        let mut now_state = State {
            uid_validity: 0,
            uid_next: 1,
            highest_mod_seq: 0,
        };
        for item in status {
            match item {
                StatusDataItem::UidValidity(v) => now_state.uid_validity = v.get(),
                StatusDataItem::UidNext(n) => now_state.uid_next = n.get(),
                StatusDataItem::HighestModSeq(m) => now_state.highest_mod_seq = m,
                _ => {}
            }
        }
        if now_state.uid_validity == 0 {
            bail!("STATUS {name} gave no UIDVALIDITY");
        }
        let before = stored.get(name).copied();
        if before == Some(now_state) {
            continue;
        }
        stats.changed_folders += 1;
        let same = before.filter(|b| b.uid_validity == now_state.uid_validity);
        let resync = same.filter(|b| qresync && b.highest_mod_seq > 0);
        let parameters = match resync {
            Some(b) => vec![SelectParameter::QResync {
                uid_validity: NonZeroU32::new(b.uid_validity).context("UIDVALIDITY 0")?,
                mod_sequence_value: std::num::NonZeroU64::new(b.highest_mod_seq)
                    .context("HIGHESTMODSEQ 0")?,
                known_uids: None,
                seq_match_data: None,
            }],
            None => Vec::new(),
        };
        let examined = client
            .examine(mailbox.clone(), ImapMailboxExamineOptions { parameters })
            .with_context(|| format!("EXAMINE {name}"))?
            .data;
        let validity = now_state.uid_validity as i64;
        let gone_before = stats.gone;

        store.begin().await?;
        // UIDs under another UIDVALIDITY (the server renumbered the folder) are gone.
        stats.gone += store
            .conn
            .execute(
                "UPDATE imap_message SET gone_at = ?4
                 WHERE origin_id = ?1 AND folder = ?2 AND uid_validity != ?3 AND gone_at IS NULL",
                (origin_id, name.as_str(), validity, now()),
            )
            .await?;
        for range in &examined.vanished_earlier {
            stats.gone += store
                .conn
                .execute(
                    "UPDATE imap_message SET gone_at = ?6
                     WHERE origin_id = ?1 AND folder = ?2 AND uid_validity = ?3
                       AND uid BETWEEN ?4 AND ?5 AND gone_at IS NULL",
                    (
                        origin_id,
                        name.as_str(),
                        validity,
                        range.start().get() as i64,
                        range.end().get() as i64,
                        now(),
                    ),
                )
                .await?;
        }
        for changed in &examined.changed {
            if let (Some(uid), Some(flags)) = (
                uid_of(changed.items.as_ref()),
                flags_of(changed.items.as_ref()),
            ) {
                store
                    .conn
                    .execute(
                        "UPDATE imap_message SET flags = ?5
                         WHERE origin_id = ?1 AND folder = ?2 AND uid_validity = ?3 AND uid = ?4",
                        (
                            origin_id,
                            name.as_str(),
                            validity,
                            uid as i64,
                            flags.as_str(),
                        ),
                    )
                    .await?;
                flag_history(store, origin_id, name, validity, uid as i64, &flags).await?;
            }
        }
        if stats.gone > gone_before {
            dirty(store, origin_id).await?;
        }
        store.commit().await?;

        // New UIDs: those from where the last complete sync left off, less any
        // an interrupted sync already stored.
        let from = same.map_or(1, |b| b.uid_next.max(1));
        let mut new = Vec::new();
        if examined.exists == Some(0) {
            store.begin().await?;
            let emptied = store
                .conn
                .execute(
                    "UPDATE imap_message SET gone_at = ?3
                     WHERE origin_id = ?1 AND folder = ?2 AND gone_at IS NULL",
                    (origin_id, name.as_str(), now()),
                )
                .await?;
            if emptied > 0 {
                stats.gone += emptied;
                dirty(store, origin_id).await?;
            }
            store.commit().await?;
        } else {
            let listed = client
                .fetch(
                    SequenceSet::try_from(format!("{from}:*").as_str())?,
                    MacroOrMessageDataItemNames::MessageDataItemNames(vec![
                        MessageDataItemName::Uid,
                    ]),
                    ImapMessageFetchOptions {
                        uid: true,
                        ..Default::default()
                    },
                )
                .with_context(|| format!("UID FETCH {from}:* in {name}"))?
                .data;
            let mut have = std::collections::HashSet::new();
            let mut rows = store
                .conn
                .query(
                    "SELECT uid FROM imap_message
                     WHERE origin_id = ?1 AND folder = ?2 AND uid_validity = ?3
                       AND uid >= ?4 AND gone_at IS NULL",
                    (origin_id, name.as_str(), validity, from as i64),
                )
                .await?;
            while let Some(r) = rows.next().await? {
                have.insert(r.get::<i64>(0)? as u32);
            }
            drop(rows);
            let on_server: std::collections::HashSet<u32> = listed
                .values()
                .filter_map(|items| uid_of(items.as_ref()))
                .filter(|u| *u >= from)
                .collect();
            new = on_server
                .iter()
                .filter(|u| !have.contains(u))
                .copied()
                .collect();
            new.sort_unstable();
            if from == 1 {
                // A full listing (no completed sync yet): what the server lacks is gone.
                store.begin().await?;
                let lost_before = stats.gone;
                for uid in have.difference(&on_server) {
                    stats.gone += store
                        .conn
                        .execute(
                            "UPDATE imap_message SET gone_at = ?5
                             WHERE origin_id = ?1 AND folder = ?2 AND uid_validity = ?3 AND uid = ?4",
                            (origin_id, name.as_str(), validity, *uid as i64, now()),
                        )
                        .await?;
                }
                if stats.gone > lost_before {
                    dirty(store, origin_id).await?;
                }
                store.commit().await?;
            }
        }
        for chunk in new.chunks(BATCH) {
            let uids: Vec<NonZeroU32> = chunk.iter().filter_map(|u| NonZeroU32::new(*u)).collect();
            let fetched = client
                .fetch(
                    SequenceSet::try_from(uids)?,
                    MacroOrMessageDataItemNames::MessageDataItemNames(vec![
                        MessageDataItemName::Uid,
                        MessageDataItemName::Flags,
                        MessageDataItemName::InternalDate,
                        MessageDataItemName::BodyExt {
                            section: None,
                            partial: None,
                            peek: true,
                        },
                    ]),
                    ImapMessageFetchOptions {
                        uid: true,
                        ..Default::default()
                    },
                )
                .with_context(|| format!("UID FETCH {} messages in {name}", chunk.len()))?
                .data;
            store.begin().await?;
            for items in fetched.values() {
                let items = items.as_ref();
                let Some(uid) = uid_of(items) else { continue };
                let raw = items.iter().find_map(|i| match i {
                    MessageDataItem::BodyExt { data, .. } => data.clone().into_option(),
                    _ => None,
                });
                let Some(raw) = raw else { continue };
                let internal_date = items.iter().find_map(|i| match i {
                    MessageDataItem::InternalDate(d) => Some(d.as_ref().timestamp()),
                    _ => None,
                });
                let (h, b) = mbox::split_message(&raw);
                let p = prepare(&raw[h.clone()], &raw[b.clone()]);
                let (artifact, fresh) = art.put(store, &raw[h], &raw[b], &p).await?;
                store
                    .conn
                    .execute(
                        "INSERT OR REPLACE INTO imap_message
                         (origin_id, folder, uid_validity, uid, artifact_id, flags, internal_date, gone_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                        (
                            origin_id,
                            name.as_str(),
                            validity,
                            uid as i64,
                            artifact,
                            flags_of(items).unwrap_or_default(),
                            internal_date,
                        ),
                    )
                    .await?;
                let flags = flags_of(items).unwrap_or_default();
                flag_history(store, origin_id, name, validity, uid as i64, &flags).await?;
                stats.fetched += 1;
                stats.new_artifacts += fresh as u64;
            }
            dirty(store, origin_id).await?;
            store.commit().await?;
            if stats.fetched % 5000 < BATCH as u64 {
                info!(folder = name.as_str(), fetched = stats.fetched, elapsed = ?t.elapsed(), "imap sync");
            }
        }

        // The STATUS taken before EXAMINE: anything since is seen again next time.
        store.begin().await?;
        store
            .conn
            .execute(
                "INSERT OR REPLACE INTO imap_folder
                 (origin_id, name, uid_validity, uid_next, highest_mod_seq, gone_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                (
                    origin_id,
                    name.as_str(),
                    validity,
                    now_state.uid_next as i64,
                    now_state.highest_mod_seq as i64,
                ),
            )
            .await?;
        store.commit().await?;
    }
    let _ = client.logout();
    record_run(store, origin_id).await?;
    Ok(stats)
}

/// Record `flags` as the message's flags from now on, unless they already are.
async fn flag_history(
    store: &Store,
    origin_id: i64,
    folder: &str,
    validity: i64,
    uid: i64,
    flags: &str,
) -> Result<()> {
    store
        .conn
        .execute(
            "INSERT INTO imap_flags (origin_id, folder, uid_validity, uid, flags, seen_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE (SELECT flags FROM imap_flags WHERE origin_id = ?1 AND folder = ?2
                    AND uid_validity = ?3 AND uid = ?4 ORDER BY rowid DESC LIMIT 1) IS NOT ?5",
            (origin_id, folder, validity, uid, flags, now()),
        )
        .await?;
    Ok(())
}

/// When stored changes await one, record a run: each folder's messages as
/// presence (only what changed is written), and what left since the last run.
async fn record_run(store: &Store, origin_id: i64) -> Result<()> {
    let mut rows = store
        .conn
        .query(
            "SELECT dirty FROM imap_state WHERE origin_id = ?1",
            (origin_id,),
        )
        .await?;
    let dirty = rows.next().await?.map(|r| r.get::<i64>(0)).transpose()? == Some(1);
    drop(rows);
    if !dirty {
        return Ok(());
    }
    let mut paths = crate::presence::Listing::new();
    let mut rows = store
        .conn
        .query(
            "SELECT folder, uid_validity, uid, artifact_id FROM imap_message
             WHERE origin_id = ?1 AND gone_at IS NULL ORDER BY folder, uid",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        let (folder, validity, uid, artifact): (String, i64, i64, i64) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        paths
            .entry(folder)
            .or_default()
            .push((format!("{validity}:{uid}"), artifact));
    }
    drop(rows);
    crate::presence::run(store, origin_id, &paths).await?;
    store
        .conn
        .execute(
            "UPDATE imap_state SET dirty = 0 WHERE origin_id = ?1",
            (origin_id,),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_kept_only_when_they_change() {
        let dir = std::env::temp_dir().join(format!("munchymail-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        pollster::block_on(async {
            let store = Store::open(&dir).await.unwrap();
            for flags in ["", "", "\\Seen", "\\Seen", "\\Answered \\Seen", ""] {
                flag_history(&store, 1, "INBOX", 7, 3, flags).await.unwrap();
            }
            let mut rows = store
                .conn
                .query("SELECT group_concat(flags, '|') FROM imap_flags", ())
                .await
                .unwrap();
            let all: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
            assert_eq!(all, "|\\Seen|\\Answered \\Seen|");
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn imap_roots_are_urls() {
        assert!(is_root("imaps://imap.example.org"));
        assert!(is_root("IMAP://localhost:143"));
        assert!(!is_root("/var/mail/me"));
        assert!(!is_root("gmail:me@example.org"));
    }

    #[test]
    fn folder_patterns() {
        let config = Config::parse(
            "c.toml".into(),
            "[accounts.\"a@x.org\".origins.i]\nimap = \"imaps://h\"\nuser = \"u\"\npassword_command = [\"true\"]\nfolders = [\"INBOX\", \"lists/*\"]\n",
        )
        .unwrap();
        let s = Settings::load(&config, "a@x.org", "i").unwrap();
        assert!(s.wants("INBOX") && s.wants("lists/rpm") && s.wants("lists/"));
        assert!(!s.wants("INBOXES") && !s.wants("Trash"));
        let all = Settings { folders: None, ..s };
        assert!(all.wants("Trash"));
    }

    #[test]
    fn settings_need_both_halves_of_a_client_cert() {
        let config = Config::parse(
            "c.toml".into(),
            "[accounts.\"a@x.org\".origins.i]\nimap = \"imaps://h\"\nuser = \"u\"\npassword_command = [\"true\"]\nclient_cert = \"c.pem\"\n",
        )
        .unwrap();
        assert!(Settings::load(&config, "a@x.org", "i").is_err());
    }

    #[test]
    fn a_silent_password_command_is_retried() {
        let e = password(&["true".into()]).unwrap_err();
        assert_eq!(crate::sync::classify(&e), crate::sync::Failure::Transient);
        assert!(password(&["false".into()]).is_err());
        let ok = password(&["echo".into(), "s3cret".into()]).unwrap();
        assert_eq!(secrecy::ExposeSecret::expose_secret(&ok), "s3cret");
    }

    #[test]
    fn flags_are_sorted() {
        use imap_codec::imap_types::flag::Flag;
        let items = [MessageDataItem::Flags(vec![
            FlagFetch::Flag(Flag::Seen),
            FlagFetch::Flag(Flag::Answered),
        ])];
        assert_eq!(flags_of(&items).unwrap(), "\\Answered \\Seen");
    }
}
