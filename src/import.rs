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

const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const HEAD_LEN: usize = 64 * 1024;

type Hash = [u8; 32];

fn sha256(b: &[u8]) -> Hash {
    Sha256::digest(b).into()
}

/// Everything about one message that can be computed without the database.
struct Prepared {
    header_hash: Hash,
    body_hash: Hash,
    fields: Vec<Field>,
    summary: Summary,
}

fn prepare(buf: &[u8], m: &mbox::Message) -> Prepared {
    let header = &buf[m.header.clone()];
    let fields = headers::fields(header);
    let summary = headers::summary(&fields);
    Prepared {
        header_hash: sha256(header),
        body_hash: sha256(&buf[m.body.clone()]),
        fields,
        summary,
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
}

struct Stmts {
    sel_content: Statement,
    sel_parents: Statement,
    ins_content: Statement,
    ins_object: Statement,
    ins_artifact: Statement,
    sel_artifact: Statement,
    ins_header: Statement,
    ins_observation: Statement,
    ins_sighting: Statement,
}

impl Stmts {
    async fn new(store: &Store) -> Result<Stmts> {
        let c = &store.conn;
        Ok(Stmts {
            sel_content: c.prepare("SELECT content_id FROM file_content WHERE hash = ?1").await?,
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
                     VALUES (?1, ?2, ?3, 'mbox', ?4, ?5, ?6)",
                )
                .await?,
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
            ins_header: c
                .prepare("INSERT INTO header (artifact_id, ordinal, name, nth, value) VALUES (?1, ?2, ?3, ?4, ?5)")
                .await?,
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
    pub async fn new(store: &'a Store, label: &str) -> Result<Importer<'a>> {
        let mut rows = store
            .conn
            .query(
                "SELECT origin_id, root, retired_at IS NOT NULL FROM origin WHERE label = ?1",
                (label,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            bail!("no origin labelled {label:?}; add it with `origin add`");
        };
        let origin_id: i64 = row.get(0)?;
        let root: String = row.get(1)?;
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
            Some(r) => Some(r.get::<i64>(0)?),
            None => None,
        };
        drop(rows);

        let content_id = match known {
            Some(id) => {
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
        let parsed = mbox::split(buf)?;
        let split = t0.elapsed();
        let head_hash = sha256(&buf[..buf.len().min(HEAD_LEN)]);
        let parent = self.find_parent(buf, &head_hash).await?;
        let prepared: Vec<Prepared> = parsed
            .messages
            .par_iter()
            .map(|m| prepare(buf, m))
            .collect();
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
            ))
            .await?;
        let content_id = self.store.conn.last_insert_rowid();

        let mut new_artifacts = 0;
        for (ordinal, (m, p)) in parsed.messages.iter().zip(&prepared).enumerate() {
            self.put_object(&p.header_hash, &buf[m.header.clone()])
                .await?;
            self.put_object(&p.body_hash, &buf[m.body.clone()]).await?;
            let s = &p.summary;
            let inserted = self
                .stmts
                .ins_artifact
                .execute((
                    p.header_hash.to_vec(),
                    p.body_hash.to_vec(),
                    m.raw().len() as i64,
                    s.date,
                    s.from.clone(),
                    s.subject.clone(),
                    s.message_id.clone(),
                ))
                .await?;
            let artifact_id = if inserted == 1 {
                new_artifacts += 1;
                let id = self.store.conn.last_insert_rowid();
                let mut seen: HashMap<&str, i64> = HashMap::new();
                for (i, f) in p.fields.iter().enumerate() {
                    let nth = seen.entry(f.name.as_str()).or_default();
                    self.stmts
                        .ins_header
                        .execute((id, i as i64, f.name.as_str(), *nth, f.value.as_str()))
                        .await?;
                    *nth += 1;
                }
                id
            } else {
                let mut rows = self
                    .stmts
                    .sel_artifact
                    .query((p.header_hash.to_vec(), p.body_hash.to_vec()))
                    .await?;
                rows.next().await?.context("artifact vanished")?.get(0)?
            };
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

    async fn put_object(&mut self, hash: &Hash, bytes: &[u8]) -> Result<()> {
        let inline = (bytes.len() <= INLINE_MAX).then_some(bytes);
        let inserted = self
            .stmts
            .ins_object
            .execute((
                hash.to_vec(),
                bytes.len() as i64,
                inline.map_or(Value::Null, Value::from),
            ))
            .await?;
        if inserted == 1 && inline.is_none() {
            self.store.put_external(hash, bytes)?;
        }
        Ok(())
    }
}
