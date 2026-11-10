//! Search and read, as the CLI (and later a service) call them. Results are
//! plain data; `to_json` gives the JSON Lines form. Everything taken from a
//! message is sender-controlled, so records carry `"untrusted": true` and
//! strings are passed through `message::sanitize`.

use anyhow::{Context, Result, bail};
use serde_json::{Value as Json, json};
use turso::Value;

use crate::headers::{self, Field};
use crate::locations::{Location, Sightings};
use crate::message::{self, Decoded, sanitize};
use crate::query::{Filter, fold_copies};
use crate::store::Store;
use crate::util::{hex, iso};
use sha2::{Digest, Sha256};

pub const MAX_LIMIT: usize = 1000;

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
    let (d, id) = c.split_once(':').context("bad cursor")?;
    Ok((
        d.parse().context("bad cursor")?,
        id.parse().context("bad cursor")?,
    ))
}

pub struct Hit {
    pub id: i64,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body_bytes: usize,
    pub attachments: usize,
    pub snippet: String,
    pub locations: Vec<Location>,
    pub problem: Option<String>,
}

struct Stored {
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
            "SELECT date, message_id, subject, header_hash, body_hash FROM artifact WHERE artifact_id = ?1",
            (id,),
        )
        .await?;
    let Some(r) = rows.next().await? else {
        bail!("no message {id}")
    };
    let (Value::Blob(h), Value::Blob(b)) = (r.get_value(3)?, r.get_value(4)?) else {
        bail!("bad hashes for {id}")
    };
    let (date, message_id, subject) = (r.get(0)?, r.get(1)?, r.get::<Option<String>>(2)?);
    drop(rows);
    let mut raw = store.object(&h).await?;
    let body = store.object(&b).await?;
    raw.extend_from_slice(&body);
    let decoded = message::decode(&raw, &body);
    Ok(Stored {
        date,
        message_id,
        subject: subject.unwrap_or_default(),
        body_len: body.len(),
        raw,
        decoded,
    })
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
        hits.push(Hit {
            id,
            date: s.date,
            message_id: s.message_id,
            from: store.first_header(id, "from").await?,
            to: store.first_header(id, "to").await?,
            subject: s.subject,
            body_bytes: s.body_len,
            attachments: s.decoded.attachments.len(),
            snippet: message::snippet(&s.decoded.text, 160),
            locations: sightings.of(id).await?,
            problem: s.decoded.problem,
        });
    }
    Ok((hits, next))
}

pub struct Message {
    pub id: i64,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub subject: String,
    /// Decoded header fields in order.
    pub headers: Vec<Field>,
    pub decoded: Decoded,
    pub raw_bytes: usize,
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

pub async fn read(store: &Store, id: i64) -> Result<Message> {
    let s = load(store, id).await?;
    let mut sightings = Sightings::new(store).await?;
    Ok(Message {
        id,
        date: s.date,
        message_id: s.message_id,
        subject: s.subject,
        headers: headers::fields(&s.raw),
        raw_bytes: s.raw.len(),
        nonce: hex(&Sha256::digest(&s.raw)[..6]),
        decoded: s.decoded,
        locations: sightings.of(id).await?,
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
            "date": opt_time(self.date),
            "message_id": self.message_id.as_deref().map(sanitize),
            "from": sanitize(&self.from),
            "to": sanitize(&self.to),
            "subject": sanitize(&self.subject),
            "snippet": sanitize(&self.snippet),
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
            "date": opt_time(self.date),
            "message_id": self.message_id.as_deref().map(sanitize),
            "from": sanitize(self.header("from")),
            "to": sanitize(self.header("to")),
            "cc": sanitize(self.header("cc")),
            "subject": sanitize(&self.subject),
            "headers": self.headers.iter().map(|f| json!([f.name, sanitize(&f.value)])).collect::<Vec<_>>(),
            "text": sanitize(&self.decoded.text),
            "text_source": self.decoded.text_source.as_str(),
            "attachments": self.decoded.attachments.iter().map(|a| json!({
                "filename": a.filename.as_deref().map(sanitize),
                "content_type": sanitize(&a.content_type),
                "bytes": a.size,
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
