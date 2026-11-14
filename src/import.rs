//! Import mbox files from an origin into the corpus.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tracing::{debug, info};
use turso::{Statement, Value};

use crate::headers::{self, Field, Summary};
use crate::mbox;
use crate::source;
use crate::store::{INLINE_MAX, Store, now};
use crate::takeout::{self, Linker};

const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const HEAD_LEN: usize = 64 * 1024;

type Hash = [u8; 32];

fn sha256(b: &[u8]) -> Hash {
    Sha256::digest(b).into()
}

/// Everything about one message that can be computed without the database.
pub(crate) struct Prepared {
    header_hash: Hash,
    body_hash: Hash,
    /// What makes two copies one message: the body with CRLF read as LF.
    identity: Hash,
    fields: Vec<Field>,
    summary: Summary,
}

/// A body's identity, the same rule for every source: its hash with each CRLF
/// read as LF and each NUL as 0x80. A store keeps the same email as LF (an mbox
/// file) or CRLF (IMAP, Gmail), and IMAP can't carry NUL, which Dovecot serves
/// as 0x80; read this way they are one message. Other bodies hash as they are.
pub(crate) fn identity(body: &[u8]) -> Hash {
    let crlf = body.windows(2).any(|w| w == b"\r\n");
    if !crlf && !body.contains(&0) {
        return sha256(body);
    }
    let mut canonical = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\r' && body.get(i + 1) == Some(&b'\n') {
            i += 1;
        }
        canonical.push(if body[i] == 0 { 0x80 } else { body[i] });
        i += 1;
    }
    sha256(&canonical)
}

pub(crate) fn prepare(header: &[u8], body: &[u8]) -> Prepared {
    let fields = headers::fields(header);
    let summary = headers::summary(&fields);
    Prepared {
        header_hash: sha256(header),
        body_hash: sha256(body),
        identity: identity(body),
        fields,
        summary,
    }
}

/// Statements that store one message: its objects, artifact, message and headers.
pub(crate) struct Artifacts {
    ins_object: Statement,
    ins_artifact: Statement,
    sel_artifact: Statement,
    sel_message: Statement,
    ins_message: Statement,
    set_message: Statement,
    ins_header: Statement,
}

impl Artifacts {
    pub(crate) async fn new(store: &Store) -> Result<Artifacts> {
        let c = &store.conn;
        Ok(Artifacts {
            ins_object: c
                .prepare("INSERT OR IGNORE INTO object (hash, length, inline_bytes) VALUES (?1, ?2, ?3)")
                .await?,
            ins_artifact: c
                .prepare(
                    "INSERT OR IGNORE INTO artifact
                     (header_hash, body_hash, length, date, sender, subject, message_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .await?,
            sel_artifact: c
                .prepare("SELECT artifact_id FROM artifact WHERE header_hash = ?1 AND body_hash = ?2")
                .await?,
            sel_message: c
                .prepare(
                    "SELECT msg_id FROM message
                     WHERE body_hash = ?1 AND message_id IS ?2 AND date IS ?3",
                )
                .await?,
            ins_message: c
                .prepare("INSERT INTO message (body_hash, message_id, date) VALUES (?1, ?2, ?3)")
                .await?,
            set_message: c
                .prepare("UPDATE artifact SET msg_id = ?1 WHERE artifact_id = ?2")
                .await?,
            ins_header: c
                .prepare("INSERT INTO header (artifact_id, ordinal, name, nth, value) VALUES (?1, ?2, ?3, ?4, ?5)")
                .await?,
        })
    }

    /// The artifact for `header` ‖ `body`, and whether it is new.
    pub(crate) async fn put(
        &mut self,
        store: &Store,
        header: &[u8],
        body: &[u8],
        p: &Prepared,
    ) -> Result<(i64, bool)> {
        self.put_object(store, &p.header_hash, header).await?;
        self.put_object(store, &p.body_hash, body).await?;
        let s = &p.summary;
        let inserted = self
            .ins_artifact
            .execute((
                p.header_hash.to_vec(),
                p.body_hash.to_vec(),
                (header.len() + body.len()) as i64,
                s.date,
                s.from.clone(),
                s.subject.clone(),
                s.message_id.clone(),
            ))
            .await?;
        if inserted == 0 {
            let mut rows = self
                .sel_artifact
                .query((p.header_hash.to_vec(), p.body_hash.to_vec()))
                .await?;
            return Ok((
                rows.next().await?.context("artifact vanished")?.get(0)?,
                false,
            ));
        }
        let id = store.conn.last_insert_rowid();
        let msg = self.message(store, &p.identity, s).await?;
        self.set_message.execute((msg, id)).await?;
        let mut seen: HashMap<&str, i64> = HashMap::new();
        for (i, f) in p.fields.iter().enumerate() {
            let nth = seen.entry(f.name.as_str()).or_default();
            self.ins_header
                .execute((id, i as i64, f.name.as_str(), *nth, f.value.as_str()))
                .await?;
            *nth += 1;
        }
        Ok((id, true))
    }

    async fn message(&mut self, store: &Store, body_hash: &Hash, s: &Summary) -> Result<i64> {
        let key = (body_hash.to_vec(), s.message_id.clone(), s.date);
        let mut rows = self.sel_message.query(key.clone()).await?;
        if let Some(r) = rows.next().await? {
            return Ok(r.get(0)?);
        }
        drop(rows);
        self.ins_message.execute(key).await?;
        Ok(store.conn.last_insert_rowid())
    }

    async fn put_object(&mut self, store: &Store, hash: &Hash, bytes: &[u8]) -> Result<()> {
        let inline = (bytes.len() <= INLINE_MAX).then_some(bytes);
        let inserted = self
            .ins_object
            .execute((
                hash.to_vec(),
                bytes.len() as i64,
                inline.map_or(Value::Null, Value::from),
            ))
            .await?;
        if inserted == 1 && inline.is_none() {
            store.put_external(hash, bytes)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub files: u64,
    pub skipped: u64,
    pub known_files: u64,
    pub messages: u64,
    pub new_artifacts: u64,
    pub bytes: u64,
    /// Takeout copies joined to their Gmail API copies' messages.
    pub takeout: takeout::Stats,
}

struct Stmts {
    sel_content: Statement,
    sel_parents: Statement,
    ins_content: Statement,
    art: Artifacts,
    ins_observation: Statement,
    ins_sighting: Statement,
}

impl Stmts {
    async fn new(store: &Store) -> Result<Stmts> {
        let c = &store.conn;
        Ok(Stmts {
            sel_content: c
                .prepare("SELECT content_id, format FROM file_content WHERE hash = ?1")
                .await?,
            sel_parents: c
                .prepare(
                    "SELECT content_id, hash, length FROM file_content
                     WHERE head_hash = ?1 AND length < ?2 AND length >= ?3 ORDER BY length DESC",
                )
                .await?,
            ins_content: c
                .prepare(
                    "INSERT INTO file_content
                     (hash, length, head_hash, format, message_count, preamble_length, parent_content_id)
                     VALUES (?1, ?2, ?3, ?7, ?4, ?5, ?6)",
                )
                .await?,
            art: Artifacts::new(store).await?,
            ins_observation: c
                .prepare(
                    "INSERT INTO observation
                     (content_id, ordinal, offset, artifact_id, from_line, separator, used_content_length)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .await?,
            ins_sighting: c
                .prepare(
                    "INSERT INTO file_sighting
                     (run_id, origin_id, path, content_id, dev, ino, mtime_ns, ctime_ns, changed_during_read)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .await?,
        })
    }
}

pub struct Importer<'a> {
    store: &'a Store,
    stmts: Stmts,
    pub origin_id: i64,
    pub run_id: i64,
    root: PathBuf,
    variant: mbox::Variant,
    /// The account's Gmail API origin, when this origin is a Takeout of it.
    pub takeout_of: Option<i64>,
    pub stats: Stats,
}

/// Mbox files under `root` (or `root` itself), skipping dot-entries such as Dovecot's `.imap`.
fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let meta = std::fs::metadata(root).with_context(|| format!("reading {}", root.display()))?;
    if meta.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() || (ft.is_symlink() && entry.path().is_file()) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn looks_like_mbox(head: &[u8]) -> bool {
    head.is_empty() || {
        let line = head.split(|&b| b == b'\n').next().unwrap_or(head);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line.strip_prefix(b"From ")
            .and_then(mbox::parse_from_line)
            .is_some()
    }
}

impl<'a> Importer<'a> {
    pub async fn new(
        store: &'a Store,
        label: &str,
        variant: mbox::Variant,
    ) -> Result<Importer<'a>> {
        let mut rows = store
            .conn
            .query(
                "SELECT origin_id, root, retired_at IS NOT NULL FROM origin WHERE label = ?1",
                (label,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            bail!(
                "no origin labelled {label:?}; `mmadm origin list` lists them, `origin add` adds one"
            );
        };
        let origin_id: i64 = row.get(0)?;
        let root: String = row.get(1)?;
        if root.starts_with("gmail:") {
            bail!("origin {label:?} is Gmail; `mmadm gmail sync` brings it up to date");
        }
        if crate::imap::is_root(&root) {
            bail!("origin {label:?} is an IMAP server; `munchymail sync` brings it up to date");
        }
        if row.get::<i64>(2)? != 0 {
            bail!(
                "origin {label:?} is retired ({root} may now be another origin's); undo with `origin retire --undo {label}`"
            );
        }
        drop(rows);
        store.begin().await?;
        store
            .conn
            .execute(
                "INSERT INTO import_run (origin_id, started_at) VALUES (?1, ?2)",
                (origin_id, now()),
            )
            .await?;
        let run_id = store.conn.last_insert_rowid();
        store.commit().await?;
        Ok(Importer {
            store,
            stmts: Stmts::new(store).await?,
            origin_id,
            run_id,
            root: PathBuf::from(root),
            variant,
            takeout_of: None,
            stats: Stats::default(),
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        for path in walk(&self.root)? {
            let rel = match path.strip_prefix(&self.root) {
                Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
                _ => PathBuf::from(path.file_name().context("root has no file name")?),
            };
            let rel = rel.to_string_lossy().into_owned();
            if !looks_like_mbox(&source::read_head(&path, 4096)?) {
                self.stats.skipped += 1;
                continue;
            }
            self.store.begin().await?;
            match self.import_file(&path, &rel).await {
                Ok(()) => {
                    let t = Instant::now();
                    self.store.commit().await?;
                    debug!(file = rel.as_str(), commit = ?t.elapsed(), "committed");
                }
                Err(e) => {
                    self.store.rollback().await?;
                    return Err(e.context(format!("importing {}", path.display())));
                }
            }
        }
        self.store
            .conn
            .execute(
                "UPDATE import_run SET finished_at = ?1 WHERE run_id = ?2",
                (now(), self.run_id),
            )
            .await?;
        Ok(())
    }

    async fn find_parent(&mut self, buf: &[u8], head_hash: &Hash) -> Result<Option<i64>> {
        if buf.len() <= HEAD_LEN {
            return Ok(None);
        }
        let mut candidates = Vec::new();
        let mut rows = self
            .stmts
            .sel_parents
            .query((head_hash.to_vec(), buf.len() as i64, HEAD_LEN as i64))
            .await?;
        while let Some(r) = rows.next().await? {
            let id: i64 = r.get(0)?;
            let hash: Vec<u8> = r.get(1)?;
            let len: i64 = r.get(2)?;
            candidates.push((id, hash, len as usize));
        }
        Ok(candidates
            .into_iter()
            .find(|(_, hash, len)| sha256(&buf[..*len]).as_slice() == hash.as_slice())
            .map(|(id, _, _)| id))
    }

    async fn import_file(&mut self, path: &Path, rel: &str) -> Result<()> {
        let snap = source::read_locked(path, LOCK_TIMEOUT)?;
        let buf = &snap.bytes;
        let file_hash = sha256(buf);
        self.stats.files += 1;
        self.stats.bytes += buf.len() as u64;

        let mut rows = self.stmts.sel_content.query((file_hash.to_vec(),)).await?;
        let known = match rows.next().await? {
            Some(r) => Some((r.get::<i64>(0)?, r.get::<String>(1)?)),
            None => None,
        };
        drop(rows);
        if let Some((_, format)) = &known
            && format != self.variant.name()
        {
            bail!(
                "these bytes were imported before as {format}, not {}; the config disagrees with that import",
                self.variant.name()
            );
        }

        let content_id = match known {
            Some((id, _)) => {
                self.stats.known_files += 1;
                info!(file = rel, "content already known, sighting only");
                id
            }
            None => self.import_content(buf, &file_hash, rel).await?,
        };

        self.stmts
            .ins_sighting
            .execute((
                self.run_id,
                self.origin_id,
                rel,
                content_id,
                snap.dev as i64,
                snap.ino as i64,
                snap.mtime_ns,
                snap.ctime_ns,
                snap.changed_during_read,
            ))
            .await?;
        Ok(())
    }

    async fn import_content(&mut self, buf: &[u8], file_hash: &Hash, rel: &str) -> Result<i64> {
        let t0 = Instant::now();
        let parsed = mbox::split(buf, self.variant)?;
        let split = t0.elapsed();
        let head_hash = sha256(&buf[..buf.len().min(HEAD_LEN)]);
        let parent = self.find_parent(buf, &head_hash).await?;
        let content = parsed
            .messages
            .par_iter()
            .map(|m| parsed.content(buf, m))
            .collect::<Result<Vec<_>, _>>()?;
        let prepared: Vec<Prepared> = content.par_iter().map(|(h, b)| prepare(h, b)).collect();
        let t1 = Instant::now();

        self.stmts
            .ins_content
            .execute((
                file_hash.to_vec(),
                buf.len() as i64,
                head_hash.to_vec(),
                parsed.messages.len() as i64,
                parsed.preamble.len() as i64,
                parent,
                self.variant.name(),
            ))
            .await?;
        let content_id = self.store.conn.last_insert_rowid();

        let mut new_artifacts = 0;
        let mut ids = Vec::with_capacity(content.len());
        for (ordinal, ((m, (h, b)), p)) in parsed
            .messages
            .iter()
            .zip(&content)
            .zip(&prepared)
            .enumerate()
        {
            let (artifact_id, new) = self.stmts.art.put(self.store, h, b, p).await?;
            new_artifacts += new as u64;
            ids.push(artifact_id);
            self.stmts
                .ins_observation
                .execute((
                    content_id,
                    ordinal as i64,
                    m.from_line.start as i64,
                    artifact_id,
                    &buf[m.from_line.clone()],
                    &buf[m.separator.clone()],
                    m.used_content_length,
                ))
                .await?;
        }

        if let Some(gmail) = self.takeout_of {
            let linked = Linker::new(self.store)
                .await?
                .from_takeout(gmail, &ids)
                .await?;
            self.stats.takeout.add(linked);
        }
        self.stats.messages += parsed.messages.len() as u64;
        self.stats.new_artifacts += new_artifacts;
        info!(
            file = rel,
            messages = parsed.messages.len(),
            new_artifacts,
            parent_content = parent,
            ?split,
            hash_and_parse = ?(t1 - t0 - split),
            insert = ?t1.elapsed(),
            "imported content"
        );
        Ok(content_id)
    }
}

/// Record run `run_id`'s sightings of listings that aren't files (Gmail labels,
/// IMAP folders): each path's (id, artifact) entries as one content of
/// `format`, reused when the same listing was seen before.
pub(crate) async fn sight_listings(
    store: &Store,
    origin_id: i64,
    run_id: i64,
    format: &str,
    paths: &std::collections::BTreeMap<String, Vec<(String, i64)>>,
) -> Result<()> {
    use sha2::Digest;
    let mut sel = store
        .conn
        .prepare("SELECT content_id FROM file_content WHERE hash = ?1")
        .await?;
    let mut ins_content = store
        .conn
        .prepare(
            "INSERT INTO file_content
             (hash, length, head_hash, format, message_count, preamble_length, parent_content_id)
             VALUES (?1, ?2, ?1, ?4, ?3, 0, NULL)",
        )
        .await?;
    let mut ins_obs = store
        .conn
        .prepare(
            "INSERT INTO observation
             (content_id, ordinal, offset, artifact_id, from_line, separator, used_content_length)
             VALUES (?1, ?2, ?2, ?3, ?4, x'', 0)",
        )
        .await?;
    let mut ins_sighting = store
        .conn
        .prepare(
            "INSERT INTO file_sighting
             (run_id, origin_id, path, content_id, dev, ino, mtime_ns, ctime_ns, changed_during_read)
             VALUES (?1, ?2, ?3, ?4, 0, 0, 0, 0, 0)",
        )
        .await?;
    for (path, entries) in paths {
        let listing: String = entries
            .iter()
            .map(|(id, a)| format!("{id} {a}\n"))
            .collect();
        let hash = sha2::Sha256::digest(format!("{format}\n{listing}").as_bytes()).to_vec();
        let mut rows = sel.query((hash.clone(),)).await?;
        let known: Option<i64> = rows.next().await?.map(|r| r.get(0)).transpose()?;
        drop(rows);
        let content = match known {
            Some(c) => c,
            None => {
                ins_content
                    .execute((hash, listing.len() as i64, entries.len() as i64, format))
                    .await?;
                let c = store.conn.last_insert_rowid();
                for (i, (id, a)) in entries.iter().enumerate() {
                    ins_obs
                        .execute((c, i as i64, *a, turso::Value::Blob(id.as_bytes().to_vec())))
                        .await?;
                }
                c
            }
        };
        ins_sighting
            .execute((run_id, origin_id, path.as_str(), content))
            .await?;
    }
    Ok(())
}

/// Move messages stored before identities read CRLF as LF onto that
/// identity, joining any that turn out to be one message. Runs once.
pub async fn line_ending_identities(store: &Store) -> Result<u64> {
    let done = store
        .conn
        .query(
            "SELECT 1 FROM meta WHERE key = 'identity' AND value = 'lf-nul'",
            (),
        )
        .await?
        .next()
        .await?
        .is_some();
    if done {
        return Ok(0);
    }
    let mut todo = Vec::new();
    let mut rows = store
        .conn
        .query(
            "SELECT m.msg_id, m.body_hash, m.message_id, m.date,
                    (SELECT a.body_hash FROM artifact a WHERE a.msg_id = m.msg_id LIMIT 1)
             FROM message m",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        let body: Option<Vec<u8>> = r.get(4)?;
        if let Some(body) = body {
            todo.push((
                r.get::<i64>(0)?,
                r.get::<Vec<u8>>(1)?,
                r.get::<Option<String>>(2)?,
                r.get::<Option<i64>>(3)?,
                body,
            ));
        }
    }
    drop(rows);
    let mut changed = 0;
    store.begin().await?;
    for (n, (msg, old, message_id, date, body_hash)) in todo.iter().enumerate() {
        let new = identity(&store.object(body_hash).await?);
        if new.as_slice() == old.as_slice() {
            continue;
        }
        changed += 1;
        let mut rows = store
            .conn
            .query(
                "SELECT msg_id FROM message WHERE body_hash = ?1 AND message_id IS ?2 AND date IS ?3",
                (new.to_vec(), message_id.clone(), *date),
            )
            .await?;
        let same: Option<i64> = rows.next().await?.map(|r| r.get(0)).transpose()?;
        drop(rows);
        match same {
            Some(into) => {
                // One email stored with both line endings: keep `into`, retire `msg`.
                let c = &store.conn;
                c.execute(
                    "UPDATE artifact SET msg_id = ?2 WHERE msg_id = ?1",
                    (*msg, into),
                )
                .await?;
                c.execute("DELETE FROM part WHERE msg_id = ?1", (*msg,))
                    .await?;
                c.execute("DELETE FROM message WHERE msg_id = ?1", (*msg,))
                    .await?;
                c.execute(
                    "INSERT OR IGNORE INTO text_forget (msg_id) VALUES (?1)",
                    (*msg,),
                )
                .await?;
                c.execute(
                    "UPDATE message SET parts_version = NULL, text_version = NULL WHERE msg_id = ?1",
                    (into,),
                )
                .await?;
            }
            None => {
                store
                    .conn
                    .execute(
                        "UPDATE message SET body_hash = ?2 WHERE msg_id = ?1",
                        (*msg, new.to_vec()),
                    )
                    .await?;
            }
        }
        if (n + 1) % 5000 == 0 {
            store.commit().await?;
            tracing::info!(
                done = n + 1,
                of = todo.len(),
                changed,
                "line-ending identities"
            );
            store.begin().await?;
        }
    }
    store
        .conn
        .execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('identity', 'lf-nul')",
            (),
        )
        .await?;
    store.commit().await?;
    Ok(changed)
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn identity_reads_crlf_as_lf() {
        assert_eq!(identity(b"a\r\nb\r\n"), sha256(b"a\nb\n"));
        assert_eq!(identity(b"a\nb\n"), sha256(b"a\nb\n"));
        // A lone CR is content, not a line ending.
        assert_eq!(identity(b"a\rb\n"), sha256(b"a\rb\n"));
        // NUL reads as 0x80, as Dovecot serves it over IMAP.
        assert_eq!(identity(b"a\x00b\n"), identity(b"a\x80b\r\n"));
        assert_eq!(identity(b"a\x00b\n"), sha256(b"a\x80b\n"));
        assert_ne!(identity(b"a\r\n"), identity(b"a\r\r\n"));
    }
}
