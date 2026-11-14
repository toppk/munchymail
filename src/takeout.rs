//! Google Takeout copies of Gmail mail. Each Takeout copy joins the message of
//! its Gmail API copy, found by thread id (`X-GM-THRID`) and Message-ID, when
//! their bytes differ only in ways Takeout is known to change them. Both raw
//! copies stay; `fold` records which is the lesser copy and why.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::{Context, Result};
use turso::Statement;

use crate::store::Store;

/// The config `source` naming an origin as a Takeout of the account's Gmail.
pub const SOURCE: &str = "gmail-takeout";

fn starts_with_name(line: &[u8], name: &[u8]) -> bool {
    line.len() >= name.len() && line[..name.len()].eq_ignore_ascii_case(name)
}

/// Whether `p` is only header lines Takeout adds (`X-GM-THRID` first, then
/// `X-GM-*`/`X-Gmail-*` such as `X-Gmail-Labels`, with continuations).
fn takeout_lines(p: &[u8]) -> bool {
    starts_with_name(p, b"x-gm-thrid:")
        && p.ends_with(b"\n")
        && p.split_inclusive(|&b| b == b'\n').all(|line| {
            starts_with_name(line, b"x-gm-")
                || starts_with_name(line, b"x-gmail-")
                || matches!(line.first(), Some(b' ' | b'\t'))
        })
}

/// Gmail's thread id (hex, as the API gives it) from Takeout's `X-GM-THRID` (decimal).
pub fn thread_hex(decimal: &str) -> Option<String> {
    decimal.trim().parse::<u64>().ok().map(|n| format!("{n:x}"))
}

/// Takeout's `X-GM-THRID` (decimal) for an API thread id (hex).
pub fn thread_decimal(hex: &str) -> Option<String> {
    u64::from_str_radix(hex, 16).ok().map(|n| n.to_string())
}

/// `orig`, then `orig` with invalid UTF-8 replaced by U+FFFD if that differs.
fn forms(orig: &[u8]) -> Vec<(Cow<'_, [u8]>, bool)> {
    let mut out = vec![(Cow::Borrowed(orig), false)];
    if let Cow::Owned(s) = String::from_utf8_lossy(orig) {
        out.push((Cow::Owned(s.into_bytes()), true));
    }
    out
}

/// Why the Takeout copy (`t_*`) is the API copy (`api_*`) though its bytes
/// differ, e.g. "takeout final-crlf"; None if it is not a copy of it.
pub fn compare(api_h: &[u8], api_b: &[u8], t_h: &[u8], t_b: &[u8]) -> Option<String> {
    let h_damaged = forms(api_h).into_iter().find_map(|(o, damaged)| {
        (t_h.len() > o.len() && t_h.ends_with(&o) && takeout_lines(&t_h[..t_h.len() - o.len()]))
            .then_some(damaged)
    })?;
    let (crlf, b_damaged) = forms(api_b).into_iter().find_map(|(o, damaged)| {
        if t_b == &*o {
            Some((false, damaged))
        } else if !o.ends_with(b"\n")
            && t_b.len() == o.len() + 2
            && t_b.starts_with(&o)
            && t_b.ends_with(b"\r\n")
        {
            Some((true, damaged))
        } else {
            None
        }
    })?;
    let mut why = String::from("takeout");
    if crlf {
        why.push_str(" final-crlf");
    }
    if h_damaged || b_damaged {
        why.push_str(" utf8-damage");
    }
    Some(why)
}

#[derive(Debug, Default)]
pub struct Stats {
    /// Takeout copies joined, by reason.
    pub linked: BTreeMap<String, u64>,
    /// Takeout copies with no API copy found (yet), artifact ids.
    pub unmatched: Vec<i64>,
}

impl Stats {
    pub fn add(&mut self, other: Stats) {
        for (k, n) in other.linked {
            *self.linked.entry(k).or_default() += n;
        }
        self.unmatched.extend(other.unmatched);
    }

    pub fn total(&self) -> u64 {
        self.linked.values().sum()
    }
}

struct Artifact {
    id: i64,
    header_hash: Vec<u8>,
    body_hash: Vec<u8>,
    message_id: Option<String>,
    msg_id: i64,
}

/// Statements for joining copies; use within a transaction.
pub struct Linker<'a> {
    store: &'a Store,
    sel_artifact: Statement,
    folded: Statement,
    api_in_thread: Statement,
    thread_of_api: Statement,
    takeout_in_thread: Statement,
    origins_of: Statement,
    ins_fold: Statement,
    move_artifact: Statement,
    still_used: Statement,
    del_parts: Statement,
    retire_message: Statement,
    forget_text: Statement,
    reindex: Statement,
}

impl<'a> Linker<'a> {
    pub async fn new(store: &'a Store) -> Result<Linker<'a>> {
        let c = &store.conn;
        Ok(Linker {
            store,
            sel_artifact: c
                .prepare(
                    "SELECT header_hash, body_hash, message_id, msg_id FROM artifact
                     WHERE artifact_id = ?1",
                )
                .await?,
            folded: c
                .prepare("SELECT 1 FROM fold WHERE artifact_id = ?1")
                .await?,
            api_in_thread: c
                .prepare(
                    "SELECT artifact_id FROM gmail_message
                     WHERE origin_id = ?1 AND thread_id = ?2 ORDER BY artifact_id",
                )
                .await?,
            thread_of_api: c
                .prepare("SELECT thread_id FROM gmail_message WHERE artifact_id = ?1")
                .await?,
            takeout_in_thread: c
                .prepare(
                    "SELECT artifact_id FROM header
                     WHERE name = 'x-gm-thrid' AND nth = 0 AND value = ?1 ORDER BY artifact_id",
                )
                .await?,
            origins_of: c
                .prepare(
                    "SELECT DISTINCT s.origin_id FROM observation o
                     JOIN file_sighting s ON s.content_id = o.content_id
                     WHERE o.artifact_id = ?1",
                )
                .await?,
            ins_fold: c
                .prepare(
                    "INSERT OR REPLACE INTO fold (artifact_id, copy_of, reason) VALUES (?1, ?2, ?3)",
                )
                .await?,
            move_artifact: c
                .prepare("UPDATE artifact SET msg_id = ?2 WHERE artifact_id = ?1")
                .await?,
            still_used: c
                .prepare("SELECT 1 FROM artifact WHERE msg_id = ?1 LIMIT 1")
                .await?,
            del_parts: c.prepare("DELETE FROM part WHERE msg_id = ?1").await?,
            retire_message: c
                .prepare("UPDATE message SET merged_into = ?2, key = NULL WHERE msg_id = ?1")
                .await?,
            forget_text: c
                .prepare("INSERT OR IGNORE INTO text_forget (msg_id) VALUES (?1)")
                .await?,
            reindex: c
                .prepare(
                    "UPDATE message SET parts_version = NULL, text_version = NULL WHERE msg_id = ?1",
                )
                .await?,
        })
    }

    async fn artifact(&mut self, id: i64) -> Result<Artifact> {
        let mut rows = self.sel_artifact.query((id,)).await?;
        let r = rows
            .next()
            .await?
            .with_context(|| format!("no artifact {id}"))?;
        Ok(Artifact {
            id,
            header_hash: r.get(0)?,
            body_hash: r.get(1)?,
            message_id: r.get(2)?,
            msg_id: r.get(3)?,
        })
    }

    async fn is_folded(&mut self, id: i64) -> Result<bool> {
        Ok(self.folded.query((id,)).await?.next().await?.is_some())
    }

    async fn ids(stmt: &mut Statement, params: impl turso::IntoParams) -> Result<Vec<i64>> {
        let mut rows = stmt.query(params).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(r.get(0)?);
        }
        Ok(out)
    }

    /// Why `t` is a Takeout copy of `api`, if it is.
    async fn verify(&self, api: &Artifact, t: &Artifact) -> Result<Option<String>> {
        if api.message_id != t.message_id {
            return Ok(None);
        }
        let api_h = self.store.object(&api.header_hash).await?;
        let t_h = self.store.object(&t.header_hash).await?;
        if api.body_hash == t.body_hash {
            return Ok(compare(&api_h, b"", &t_h, b""));
        }
        let api_b = self.store.object(&api.body_hash).await?;
        let t_b = self.store.object(&t.body_hash).await?;
        Ok(compare(&api_h, &api_b, &t_h, &t_b))
    }

    /// Put Takeout copy `t` in `api`'s message; the message `t` leaves, if now
    /// empty, stays as merged into it.
    async fn link(&mut self, t: &Artifact, api: &Artifact, why: &str) -> Result<()> {
        self.ins_fold.execute((t.id, api.id, why)).await?;
        if t.msg_id == api.msg_id {
            if t.id < api.id {
                self.reindex.execute((api.msg_id,)).await?;
            }
            return Ok(());
        }
        self.move_artifact.execute((t.id, api.msg_id)).await?;
        if self
            .still_used
            .query((t.msg_id,))
            .await?
            .next()
            .await?
            .is_none()
        {
            self.del_parts.execute((t.msg_id,)).await?;
            self.retire_message.execute((t.msg_id, api.msg_id)).await?;
            self.forget_text.execute((t.msg_id,)).await?;
        }
        Ok(())
    }

    /// Join Takeout artifacts `ids` to the messages of their copies in Gmail origin `gmail`.
    pub async fn from_takeout(&mut self, gmail: i64, ids: &[i64]) -> Result<Stats> {
        let mut stats = Stats::default();
        for &id in ids {
            if self.is_folded(id).await? {
                continue;
            }
            let t = self.artifact(id).await?;
            let thread = self.store.first_header(id, "x-gm-thrid").await?;
            let mut found = None;
            if let Some(hex) = thread_hex(&thread) {
                for a in Self::ids(&mut self.api_in_thread, (gmail, hex.as_str())).await? {
                    let api = self.artifact(a).await?;
                    if let Some(why) = self.verify(&api, &t).await? {
                        found = Some((api, why));
                        break;
                    }
                }
            }
            match found {
                Some((api, why)) => {
                    self.link(&t, &api, &why).await?;
                    *stats.linked.entry(why).or_default() += 1;
                }
                None => stats.unmatched.push(id),
            }
        }
        Ok(stats)
    }

    /// Join the Takeout copies, among origins `takeouts`, of API artifacts `ids`.
    pub async fn from_api(&mut self, takeouts: &[i64], ids: &[i64]) -> Result<Stats> {
        let mut stats = Stats::default();
        if takeouts.is_empty() {
            return Ok(stats);
        }
        for &id in ids {
            let Some(hex) = Self::first_text(&mut self.thread_of_api, (id,)).await? else {
                continue;
            };
            let Some(dec) = thread_decimal(&hex) else {
                continue;
            };
            let api = self.artifact(id).await?;
            for t in Self::ids(&mut self.takeout_in_thread, (dec.as_str(),)).await? {
                if self.is_folded(t).await? {
                    continue;
                }
                let origins = Self::ids(&mut self.origins_of, (t,)).await?;
                if !origins.iter().any(|o| takeouts.contains(o)) {
                    continue;
                }
                let t = self.artifact(t).await?;
                if let Some(why) = self.verify(&api, &t).await? {
                    self.link(&t, &api, &why).await?;
                    *stats.linked.entry(why).or_default() += 1;
                }
            }
        }
        Ok(stats)
    }

    async fn first_text(
        stmt: &mut Statement,
        params: impl turso::IntoParams,
    ) -> Result<Option<String>> {
        let mut rows = stmt.query(params).await?;
        Ok(match rows.next().await? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }
}

/// Origin ids of `labels`, skipping labels with no origin.
pub async fn origin_ids(store: &Store, labels: &[String]) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for l in labels {
        let mut rows = store
            .conn
            .query(
                "SELECT origin_id FROM origin WHERE label = ?1",
                (l.as_str(),),
            )
            .await?;
        if let Some(r) = rows.next().await? {
            out.push(r.get(0)?);
        }
    }
    Ok(out)
}

/// The account's Gmail API origin, if it has synced.
pub async fn gmail_origin(store: &Store, address: &str) -> Result<Option<i64>> {
    let mut rows = store
        .conn
        .query(
            "SELECT origin_id FROM origin WHERE root = ?1",
            (format!("gmail:{address}"),),
        )
        .await?;
    Ok(match rows.next().await? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{Artifacts, Importer, prepare};
    use crate::mbox;

    const API: &[u8] =
        b"From: a@x\r\nMessage-ID: <m@x>\r\nDate: Fri, 6 Nov 2026 21:47:12 +0000\r\n\r\nFrom me\r\nbye";
    const TAKEOUT: &[u8] = b"From 1881147056200054295@xxx Fri Nov 06 21:47:12 +0000 2026\r\n\
X-GM-THRID: 1881147056200054295\r\nX-Gmail-Labels: Sent\r\n\
From: a@x\r\nMessage-ID: <m@x>\r\nDate: Fri, 6 Nov 2026 21:47:12 +0000\r\n\r\n>From me\r\nbye\r\n\r\n";

    async fn one(store: &Store, sql: &str) -> i64 {
        let mut rows = store.conn.query(sql, ()).await.unwrap();
        rows.next().await.unwrap().unwrap().get(0).unwrap()
    }

    async fn sync_api(store: &Store, gmail: i64, takeout: i64) {
        let (h, b) = mbox::split_message(API);
        let p = prepare(&API[h.clone()], &API[b.clone()]);
        store.begin().await.unwrap();
        let (id, _) = Artifacts::new(store)
            .await
            .unwrap()
            .put(store, &API[h], &API[b], &p)
            .await
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO gmail_message (gmail_id, origin_id, artifact_id, thread_id, labels)
                 VALUES ('1a1b2d47e7137617', ?1, ?2, '1a1b2d47e7137617', 'SENT')",
                (gmail, id),
            )
            .await
            .unwrap();
        Linker::new(store)
            .await
            .unwrap()
            .from_api(&[takeout], &[id])
            .await
            .unwrap();
        store.commit().await.unwrap();
    }

    async fn scenario(api_first: bool) {
        let dir = std::env::temp_dir().join(format!(
            "munchymail-takeout-{}-{api_first}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mbox_path = dir.join("takeout.mbox");
        std::fs::write(&mbox_path, TAKEOUT).unwrap();
        let store = Store::open(&dir.join("a@x.org")).await.unwrap();
        for (label, root) in [
            ("gmail", "gmail:a@x.org"),
            ("t", mbox_path.to_str().unwrap()),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO origin (label, root, created_at) VALUES (?1, ?2, 0)",
                    (label, root),
                )
                .await
                .unwrap();
        }
        let gmail = one(&store, "SELECT origin_id FROM origin WHERE label = 'gmail'").await;
        let takeout = one(&store, "SELECT origin_id FROM origin WHERE label = 't'").await;
        if api_first {
            sync_api(&store, gmail, takeout).await;
        }
        let mut imp = Importer::new(&store, "t", mbox::Variant::Mboxrd)
            .await
            .unwrap();
        imp.takeout_of = Some(gmail);
        imp.run().await.unwrap();
        assert_eq!(imp.stats.takeout.total(), api_first as u64);
        if !api_first {
            sync_api(&store, gmail, takeout).await;
        }
        assert_eq!(one(&store, "SELECT count(*) FROM artifact").await, 2);
        assert_eq!(
            one(
                &store,
                "SELECT count(*) FROM message WHERE merged_into IS NULL"
            )
            .await,
            1
        );
        assert_eq!(
            one(&store, "SELECT count(DISTINCT msg_id) FROM artifact").await,
            1
        );
        let mut rows = store
            .conn
            .query("SELECT reason FROM fold", ())
            .await
            .unwrap();
        let why: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(why, "takeout final-crlf");
        drop(rows);
        // Identity adds the API copy's missing final newline, so the Takeout copy
        // joins its message at once: no second message to drop.
        assert_eq!(one(&store, "SELECT count(*) FROM text_forget").await, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn either_order_gives_one_message() {
        pollster::block_on(scenario(true));
        pollster::block_on(scenario(false));
    }

    const API_H: &[u8] = b"From: a@x\r\nSubject: hi\r\n\r\n";
    const T_H: &[u8] = b"X-GM-THRID: 1881147056200054295\r\nX-Gmail-Labels: Sent,\r\n Archived\r\nFrom: a@x\r\nSubject: hi\r\n\r\n";

    #[test]
    fn takeout_differences() {
        assert_eq!(
            compare(API_H, b"x\r\n", T_H, b"x\r\n").as_deref(),
            Some("takeout")
        );
        assert_eq!(
            compare(API_H, b"x", T_H, b"x\r\n").as_deref(),
            Some("takeout final-crlf")
        );
        assert_eq!(
            compare(API_H, b"\xa3 5", T_H, "\u{fffd} 5\r\n".as_bytes()).as_deref(),
            Some("takeout final-crlf utf8-damage")
        );
        let snoozed = [
            &T_H[..T_H.len() - API_H.len()],
            b"X-Gmail-Snooze-Due-Date: Fri, 22 Jan 2021\r\n",
            API_H,
        ]
        .concat();
        assert_eq!(
            compare(API_H, b"x", &snoozed, b"x").as_deref(),
            Some("takeout")
        );
        let other = [&T_H[..T_H.len() - API_H.len()], b"X-Other: 1\r\n", API_H].concat();
        assert_eq!(compare(API_H, b"x", &other, b"x"), None);
        assert_eq!(compare(API_H, b"x\r\n", T_H, b"y\r\n"), None);
        assert_eq!(compare(API_H, b"x\r\n", T_H, b"x\r\n\r\n"), None);
        assert_eq!(compare(API_H, b"x\r\n", API_H, b"x\r\n"), None);
        assert_eq!(
            compare(
                API_H,
                b"x",
                b"X-Gmail-Labels: Sent\r\nFrom: a@x\r\nSubject: hi\r\n\r\n",
                b"x"
            ),
            None
        );
    }

    #[test]
    fn thread_ids() {
        assert_eq!(
            thread_hex("1881147056200054295").as_deref(),
            Some("1a1b2d47e7137617")
        );
        assert_eq!(
            thread_decimal("1a1b2d47e7137617").as_deref(),
            Some("1881147056200054295")
        );
        assert_eq!(thread_hex("x"), None);
    }
}
