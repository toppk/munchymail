//! Search and read, as the CLI (and later a service) call them. Results are
//! plain data; `to_json` gives the JSON Lines form. Everything taken from a
//! message is sender-controlled, so records carry `"untrusted": true` and
//! strings are passed through `message::sanitize`.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde_json::{Value as Json, json};
use turso::Value;

use crate::extract;
use crate::headers::{self, Field};
use crate::locations::{self, Location, Sightings};
use crate::message::{self, Decoded, sanitize};
use crate::query::{Filter, fold_copies};
use crate::store::Store;
use crate::text;
use crate::util::{hex, iso};
use sha2::{Digest, Sha256};

pub const MAX_LIMIT: usize = 1000;

/// What `read` takes: a message (`m123`), which keeps its id when the mail
/// server rewrites headers, or one stored copy (`123`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ref {
    Message(i64),
    Copy(i64),
}

impl FromStr for Ref {
    type Err = String;
    fn from_str(s: &str) -> Result<Ref, String> {
        let bad = || format!("{s:?}: want a message id like m1234 or a copy id like 227617");
        match s.strip_prefix('m') {
            Some(n) => n.parse().map(Ref::Message).map_err(|_| bad()),
            None => s.parse().map(Ref::Copy).map_err(|_| bad()),
        }
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Ref::Message(n) => write!(f, "m{n}"),
            Ref::Copy(n) => write!(f, "{n}"),
        }
    }
}

/// Sort key for a missing Date header: after every dated message.
const NO_DATE: i64 = i64::MIN;

/// Fill TEMP TABLE `hit` with the matching artifacts (all of them if the
/// filter is empty), folded to one copy per message when `unique`.
pub async fn matching(store: &Store, filter: &Filter, unique: bool) -> Result<&'static str> {
    let mut set = filter.materialize(&store.conn).await?;
    if unique {
        if set == "artifact" {
            for sql in [
                "CREATE TEMP TABLE IF NOT EXISTS hit (artifact_id INTEGER PRIMARY KEY)",
                "DELETE FROM hit",
                "INSERT INTO hit SELECT artifact_id FROM artifact",
            ] {
                store.conn.execute(sql, ()).await?;
            }
            set = "hit";
        }
        fold_copies(&store.conn, set).await?;
    }
    Ok(set)
}

/// One page of ids from `set`, newest first (Date header, then id), and the
/// cursor for the next page if there is one.
pub async fn page(
    store: &Store,
    set: &str,
    limit: usize,
    after: Option<&str>,
) -> Result<(Vec<i64>, Option<String>)> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        bail!("limit must be 1..={MAX_LIMIT}, got {limit}");
    }
    let from = match set {
        "hit" => "hit JOIN artifact a ON a.artifact_id = hit.artifact_id",
        _ => "artifact a",
    };
    let key = format!("coalesce(a.date, {NO_DATE})");
    let (cond, mut params): (String, Vec<Value>) = match after {
        Some(c) => {
            let (d, id) = parse_cursor(c)?;
            (
                format!("WHERE {key} < ?1 OR ({key} = ?1 AND a.artifact_id < ?2)"),
                vec![d.into(), id.into()],
            )
        }
        None => (String::new(), Vec::new()),
    };
    params.push(((limit + 1) as i64).into());
    let mut stmt = store
        .conn
        .prepare(&format!(
            "SELECT a.artifact_id, {key} FROM {from} {cond}
             ORDER BY {key} DESC, a.artifact_id DESC LIMIT ?{}",
            params.len()
        ))
        .await?;
    let mut rows = stmt.query(params).await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push((r.get::<i64>(0)?, r.get::<i64>(1)?));
    }
    let next = (out.len() > limit).then(|| {
        out.truncate(limit);
        let (id, d) = out[limit - 1];
        format!("{d}:{id}")
    });
    Ok((out.into_iter().map(|(id, _)| id).collect(), next))
}

fn parse_cursor(c: &str) -> Result<(i64, i64)> {
    let bad = || format!("bad cursor {c:?}; pass the `next` of the previous page as --after");
    let (d, id) = c.split_once(':').with_context(bad)?;
    Ok((d.parse().with_context(bad)?, id.parse().with_context(bad)?))
}

pub struct Matched {
    /// The subject, body, an attachment or an inline part.
    pub source: &'static str,
    pub part: Option<usize>,
    pub filename: Option<String>,
    /// Byte ranges of `snippet` that matched.
    pub highlighted: Vec<std::ops::Range<usize>>,
}

pub struct Hit {
    pub id: i64,
    pub msg: i64,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body_bytes: usize,
    pub attachments: usize,
    pub snippet: String,
    /// Where `--text` matched, with `snippet` the matching fragment.
    pub matched: Option<Matched>,
    /// This copy's places, or with `unique` those of every copy of the message.
    pub locations: Vec<Location>,
    pub problem: Option<String>,
}

struct Stored {
    msg: i64,
    date: Option<i64>,
    message_id: Option<String>,
    subject: String,
    raw: Vec<u8>,
    body_len: usize,
    decoded: Decoded,
}

async fn load(store: &Store, id: i64) -> Result<Stored> {
    let mut rows = store
        .conn
        .query(
            "SELECT date, message_id, subject, header_hash, body_hash, msg_id
             FROM artifact WHERE artifact_id = ?1",
            (id,),
        )
        .await?;
    let Some(r) = rows.next().await? else {
        bail!("no stored copy {id}; `search` lists ids")
    };
    let (Value::Blob(h), Value::Blob(b)) = (r.get_value(3)?, r.get_value(4)?) else {
        bail!("bad hashes for {id}")
    };
    let (date, message_id, subject) = (r.get(0)?, r.get(1)?, r.get::<Option<String>>(2)?);
    let msg = r.get(5)?;
    drop(rows);
    let mut raw = store.object(&h).await?;
    let body = store.object(&b).await?;
    raw.extend_from_slice(&body);
    let mut decoded = message::decode(&raw, &body);
    extract::fill(store, msg, &mut decoded).await?;
    Ok(Stored {
        msg,
        date,
        message_id,
        subject: subject.unwrap_or_default(),
        body_len: body.len(),
        raw,
        decoded,
    })
}

/// The message `msg` names now: itself, or the one its copies joined.
pub async fn resolve(store: &Store, msg: i64) -> Result<i64> {
    let mut at = msg;
    for _ in 0..64 {
        let mut rows = store
            .conn
            .query("SELECT merged_into FROM message WHERE msg_id = ?1", (at,))
            .await?;
        match rows.next().await? {
            Some(r) => match r.get::<Option<i64>>(0)? {
                Some(into) => at = into,
                None => return Ok(at),
            },
            None => bail!("no message m{msg}; `search --json` lists message ids"),
        }
    }
    bail!("m{msg} is merged in a loop; the archive needs checking")
}

/// Every stored copy of message `msg` (or the one it was merged into), the one
/// to show (its preferred copy: not folded, then the earliest stored), and the
/// places of all of them.
async fn message_copies(
    store: &Store,
    sightings: &mut Sightings,
    msg: i64,
) -> Result<(Vec<i64>, i64, Vec<Location>)> {
    let msg = resolve(store, msg).await?;
    let mut rows = store
        .conn
        .query(
            "SELECT a.artifact_id, f.artifact_id IS NULL FROM artifact a
             LEFT JOIN fold f ON f.artifact_id = a.artifact_id
             WHERE a.msg_id = ?1 ORDER BY a.artifact_id",
            (msg,),
        )
        .await?;
    let mut ids = Vec::new();
    let mut primary = Vec::new();
    while let Some(r) = rows.next().await? {
        ids.push(r.get::<i64>(0)?);
        primary.push(r.get::<i64>(1)? != 0);
    }
    drop(rows);
    let mut all = Vec::new();
    for &id in &ids {
        locations::merge(&mut all, sightings.of(id).await?);
    }
    let Some(chosen) = ids
        .iter()
        .zip(&primary)
        .find(|&(_, &p)| p)
        .map(|(&id, _)| id)
        .or(ids.first().copied())
    else {
        bail!("no message m{msg}; `search --json` lists message ids")
    };
    Ok((ids, chosen, all))
}

/// One page of matching messages, with locations and a snippet each.
pub async fn search(
    store: &Store,
    filter: &Filter,
    unique: bool,
    limit: usize,
    after: Option<&str>,
) -> Result<(Vec<Hit>, Option<String>)> {
    let set = matching(store, filter, unique).await?;
    let (ids, next) = page(store, set, limit, after).await?;
    let mut sightings = Sightings::new(store).await?;
    let mut hits = Vec::with_capacity(ids.len());
    for id in ids {
        let s = load(store, id).await?;
        let locations = if unique {
            message_copies(store, &mut sightings, s.msg).await?.2
        } else {
            sightings.of(id).await?
        };
        let mut snippet = message::snippet(&s.decoded.text, 160);
        let matched = filter.text.as_ref().and_then(|t| {
            let texts: Vec<_> = text::texts(&s.decoded)
                .into_iter()
                .map(|(i, t)| (i, sanitize(t)))
                .collect();
            let m = t.find(&texts, &sanitize(&s.subject))?;
            let part = m.part.map(|i| &s.decoded.parts[i]);
            snippet = m.fragment;
            Some(Matched {
                source: match part {
                    _ if m.subject => "subject",
                    Some(p) => p.kind.as_str(),
                    None => "body",
                },
                part: m.part,
                filename: part.and_then(|p| p.filename.clone()),
                highlighted: m.highlighted,
            })
        });
        hits.push(Hit {
            id,
            msg: s.msg,
            date: s.date,
            message_id: s.message_id,
            from: store.first_header(id, "from").await?,
            to: store.first_header(id, "to").await?,
            subject: s.subject,
            body_bytes: s.body_len,
            attachments: s.decoded.files(),
            snippet,
            matched,
            locations,
            problem: s.decoded.problem,
        });
    }
    Ok((hits, next))
}

pub struct Message {
    /// The copy shown.
    pub id: i64,
    pub msg: i64,
    /// The `mN` asked for, when its copies have since joined `msg`.
    pub merged_from: Option<i64>,
    /// Every stored copy of the message, oldest first.
    pub copies: Vec<i64>,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub subject: String,
    /// Decoded header fields in order.
    pub headers: Vec<Field>,
    pub decoded: Decoded,
    pub raw_bytes: usize,
    /// The message's places when read by message id, else this copy's.
    pub locations: Vec<Location>,
    /// From the SHA-256 of the whole message, which includes headers our own
    /// server added, so a sender can't predict it to fake an end marker.
    pub nonce: String,
}

impl Message {
    pub fn header(&self, name: &str) -> &str {
        headers::first(&self.headers, name).unwrap_or("")
    }
}

/// The stored copy `r` names: itself, or the copy `read` shows of a message.
pub async fn copy(store: &Store, r: Ref) -> Result<i64> {
    match r {
        Ref::Message(msg) => {
            let mut sightings = Sightings::new(store).await?;
            Ok(message_copies(store, &mut sightings, msg).await?.1)
        }
        Ref::Copy(id) => Ok(id),
    }
}

/// Attachment `n` of `r`, numbered from 1 as `read` lists them: its file name
/// as the sender gave it, and its decoded bytes.
pub async fn attachment(store: &Store, r: Ref, n: usize) -> Result<(Option<String>, Vec<u8>)> {
    let id = copy(store, r).await?;
    let s = load(store, id).await?;
    let count = s.decoded.attachments().count();
    let Some(index) = s
        .decoded
        .parts
        .iter()
        .enumerate()
        .filter(|(_, p)| p.kind != message::Kind::Body)
        .map(|(i, _)| i)
        .nth(n.wrapping_sub(1))
    else {
        bail!("{r} has {count} attachments; `read {r}` lists them, numbered from 1");
    };
    let bytes = message::leaf(&s.raw, index).context("the part no longer parses")?;
    Ok((s.decoded.parts[index].filename.clone(), bytes))
}

pub async fn read(store: &Store, r: Ref) -> Result<Message> {
    let mut sightings = Sightings::new(store).await?;
    let id = match r {
        Ref::Message(msg) => message_copies(store, &mut sightings, msg).await?.1,
        Ref::Copy(id) => id,
    };
    let s = load(store, id).await?;
    let (copies, _, all) = message_copies(store, &mut sightings, s.msg).await?;
    let locations = match r {
        Ref::Message(_) => all,
        Ref::Copy(_) => sightings.of(id).await?,
    };
    let merged_from = match r {
        Ref::Message(m) if m != s.msg => Some(m),
        _ => None,
    };
    Ok(Message {
        id,
        msg: s.msg,
        merged_from,
        copies,
        date: s.date,
        message_id: s.message_id,
        subject: s.subject,
        headers: headers::fields(&s.raw),
        raw_bytes: s.raw.len(),
        nonce: hex(&Sha256::digest(&s.raw)[..6]),
        decoded: s.decoded,
        locations,
    })
}

fn opt_time(t: Option<i64>) -> Json {
    t.map_or(Json::Null, |t| json!(iso(t)))
}

fn location_json(l: &Location) -> Json {
    json!({
        "origin": l.origin,
        "path": l.path,
        "current": l.current,
        "first_seen": iso(l.first_seen),
        "last_seen": iso(l.last_seen),
        "origin_last_import": opt_time(l.origin_last_import),
        "origin_retired": l.origin_retired,
    })
}

impl Hit {
    pub fn to_json(&self) -> Json {
        json!({
            "type": "message",
            "id": self.id,
            "message": Ref::Message(self.msg).to_string(),
            "date": opt_time(self.date),
            "message_id": self.message_id.as_deref().map(sanitize),
            "from": sanitize(&self.from),
            "to": sanitize(&self.to),
            "subject": sanitize(&self.subject),
            "snippet": sanitize(&self.snippet),
            "match": self.matched.as_ref().map(|m| json!({
                "source": m.source,
                "part": m.part,
                "filename": m.filename.as_deref().map(sanitize),
                "highlighted": m.highlighted.iter().map(|r| [r.start, r.end]).collect::<Vec<_>>(),
            })),
            "body_bytes": self.body_bytes,
            "attachments": self.attachments,
            "locations": self.locations.iter().map(location_json).collect::<Vec<_>>(),
            "problem": self.problem,
            "untrusted": true,
        })
    }
}

impl Message {
    pub fn to_json(&self) -> Json {
        json!({
            "type": "message",
            "id": self.id,
            "message": Ref::Message(self.msg).to_string(),
            "merged_from": self.merged_from.map(|m| Ref::Message(m).to_string()),
            "copies": self.copies,
            "date": opt_time(self.date),
            "message_id": self.message_id.as_deref().map(sanitize),
            "from": sanitize(self.header("from")),
            "to": sanitize(self.header("to")),
            "cc": sanitize(self.header("cc")),
            "subject": sanitize(&self.subject),
            "headers": self.headers.iter()
                .filter(|f| !headers::MAILBOX_BOOKKEEPING.contains(&f.name.as_str()))
                .map(|f| json!([f.name, sanitize(&f.value)])).collect::<Vec<_>>(),
            "text": sanitize(&self.decoded.text),
            "text_source": self.decoded.text_source.as_str(),
            "attachments": self.decoded.attachments().map(|a| json!({
                "kind": a.kind.as_str(),
                "filename": a.filename.as_deref().map(sanitize),
                "content_type": sanitize(&a.content_type),
                "detected_type": a.detected,
                "bytes": a.size,
                "sha256": hex(&a.sha256),
                "members": a.members.iter().map(|(n, size)| json!({"name": sanitize(n), "bytes": size})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "raw_bytes": self.raw_bytes,
            "locations": self.locations.iter().map(location_json).collect::<Vec<_>>(),
            "problem": self.decoded.problem,
            "untrusted": true,
        })
    }
}

/// The closing record of a JSON Lines page.
pub fn page_json(returned: usize, next: Option<&str>) -> Json {
    json!({"type": "page", "returned": returned, "next": next})
}
