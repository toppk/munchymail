//! The corpus: `data/email.sqlite` plus `data/content/aa/<sha256>` for large objects.

use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::info;
use turso::{Builder, Connection, Database, Value};

use crate::util::hex;

/// Objects at most this size live inline in SQLite.
pub const INLINE_MAX: usize = 16 * 1024;

const SCHEMA_VERSION: i64 = 9;

/// Upgrades from the previous version; index `i` brings version `i + 1` to `i + 2`.
/// New tables and indexes need no entry: `SCHEMA` creates them if missing.
const MIGRATIONS: [&str; 8] = [
    "ALTER TABLE origin ADD COLUMN retired_at INTEGER",
    "",
    "ALTER TABLE artifact ADD COLUMN msg_id INTEGER;
     INSERT INTO message (body_hash, message_id, date)
         SELECT DISTINCT body_hash, message_id, date FROM artifact;
     UPDATE artifact SET msg_id = (SELECT m.msg_id FROM message m
         WHERE m.body_hash = artifact.body_hash AND m.message_id IS artifact.message_id
           AND m.date IS artifact.date);",
    "ALTER TABLE message ADD COLUMN parts_version INTEGER;
     ALTER TABLE message ADD COLUMN parts_problem TEXT;",
    "ALTER TABLE message ADD COLUMN text_version INTEGER",
    "",
    "",
    "UPDATE file_content SET format = 'dovecot' WHERE format = 'mbox'",
];

/// Indexes on columns a migration may have just added.
const LATE_SCHEMA: &str = "CREATE INDEX IF NOT EXISTS artifact_msg ON artifact (msg_id);";

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE IF NOT EXISTS object (
    hash BLOB PRIMARY KEY,
    length INTEGER NOT NULL,
    inline_bytes BLOB
);

CREATE TABLE IF NOT EXISTS artifact (
    artifact_id INTEGER PRIMARY KEY,
    header_hash BLOB NOT NULL,
    body_hash BLOB NOT NULL,
    length INTEGER NOT NULL,
    date INTEGER,
    sender TEXT,
    subject TEXT,
    message_id TEXT,
    -- the message this is a copy of
    msg_id INTEGER,
    UNIQUE (header_hash, body_hash)
);
CREATE INDEX IF NOT EXISTS artifact_date ON artifact (date);
CREATE INDEX IF NOT EXISTS artifact_message_id ON artifact (message_id);

-- One email, whatever copies of it were stored: same body, Message-ID and Date.
-- Dovecot's Status/X-Keywords rewrites and SpamAssassin's subject tag fold
-- together; Date splits reused Message-IDs and ID-less mail sharing a body.
CREATE TABLE IF NOT EXISTS message (
    msg_id INTEGER PRIMARY KEY,
    body_hash BLOB NOT NULL,
    message_id TEXT,
    date INTEGER,
    -- parts::VERSION that filled its `part` rows; NULL = not yet
    parts_version INTEGER,
    -- why its MIME structure didn't parse, if it didn't
    parts_problem TEXT,
    -- text::VERSION that put it in data/text/; NULL = not yet
    text_version INTEGER
);
CREATE INDEX IF NOT EXISTS message_body ON message (body_hash);

-- Every MIME leaf part of a message (`munchymail index`).
CREATE TABLE IF NOT EXISTS part (
    msg_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    -- body (message text), inline, or attachment (has a file name)
    kind TEXT NOT NULL,
    filename TEXT,
    ext TEXT,
    declared_type TEXT NOT NULL,
    -- from the decoded bytes (magic numbers); NULL if not recognised
    detected_type TEXT,
    size INTEGER NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY (msg_id, ordinal)
);
CREATE INDEX IF NOT EXISTS part_hash ON part (hash);

-- Text extracted from a part's bytes by a tool, once per part hash.
CREATE TABLE IF NOT EXISTS extract (
    hash BLOB PRIMARY KEY,
    -- extract::VERSION
    version INTEGER NOT NULL,
    -- e.g. 'pdftotext 26.01.0', 'office-xml'
    tool TEXT NOT NULL,
    -- ok, empty, truncated, timeout, or 'failed: ' and the tool's message
    outcome TEXT NOT NULL,
    text TEXT
);

-- File names inside a zip part, by the part's hash.
CREATE TABLE IF NOT EXISTS archive_member (
    hash BLOB NOT NULL,
    ordinal INTEGER NOT NULL,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (hash, ordinal)
);

CREATE TABLE IF NOT EXISTS header (
    artifact_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    name TEXT NOT NULL,
    -- occurrence of this name within the message, 0 = first (Sieve :index - 1)
    nth INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (artifact_id, ordinal)
);
-- value search/top by name; nth = 0 counts messages per name
CREATE INDEX IF NOT EXISTS header_name_nth_value ON header (name, nth, value);
-- header names of a filtered set of messages
CREATE INDEX IF NOT EXISTS header_artifact_nth_name ON header (artifact_id, nth, name);

-- The account's own addresses: Sieve :matches patterns, e.g. '*@example.org'.
CREATE TABLE IF NOT EXISTS alias (
    pattern TEXT PRIMARY KEY,
    added_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS origin (
    origin_id INTEGER PRIMARY KEY,
    label TEXT NOT NULL UNIQUE,
    root TEXT NOT NULL,
    host TEXT,
    note TEXT,
    created_at INTEGER NOT NULL,
    retired_at INTEGER
);

CREATE TABLE IF NOT EXISTS import_run (
    run_id INTEGER PRIMARY KEY,
    origin_id INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE IF NOT EXISTS file_content (
    content_id INTEGER PRIMARY KEY,
    hash BLOB NOT NULL UNIQUE,
    length INTEGER NOT NULL,
    head_hash BLOB NOT NULL,
    format TEXT NOT NULL,
    message_count INTEGER NOT NULL,
    preamble_length INTEGER NOT NULL,
    parent_content_id INTEGER
);
CREATE INDEX IF NOT EXISTS file_content_head ON file_content (head_hash);

CREATE TABLE IF NOT EXISTS file_sighting (
    sighting_id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL,
    origin_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    content_id INTEGER NOT NULL,
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    changed_during_read INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS file_sighting_path ON file_sighting (origin_id, path);
CREATE INDEX IF NOT EXISTS file_sighting_content ON file_sighting (content_id);
CREATE INDEX IF NOT EXISTS file_sighting_run ON file_sighting (run_id);

CREATE TABLE IF NOT EXISTS observation (
    content_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    artifact_id INTEGER NOT NULL,
    from_line BLOB NOT NULL,
    separator BLOB NOT NULL,
    used_content_length INTEGER NOT NULL,
    PRIMARY KEY (content_id, ordinal)
);
CREATE INDEX IF NOT EXISTS observation_artifact ON observation (artifact_id);

-- Gmail as the source reports it (gmail sync), kept apart from the archive's
-- own decisions: a message Gmail deletes stays archived, with gone_at set.
CREATE TABLE IF NOT EXISTS gmail_label (
    origin_id INTEGER NOT NULL,
    label_id TEXT NOT NULL,
    name TEXT NOT NULL,
    -- system or user
    type TEXT NOT NULL,
    PRIMARY KEY (origin_id, label_id)
);
CREATE TABLE IF NOT EXISTS gmail_message (
    gmail_id TEXT PRIMARY KEY,
    origin_id INTEGER NOT NULL,
    artifact_id INTEGER NOT NULL,
    thread_id TEXT NOT NULL,
    -- when Gmail received it, Unix seconds
    internal_date INTEGER,
    -- label ids, sorted, space-separated
    labels TEXT NOT NULL,
    -- when Gmail reported it deleted, or a completed full listing lacked it
    gone_at INTEGER,
    -- when a sync last asked Gmail about it
    checked_at INTEGER
);
CREATE INDEX IF NOT EXISTS gmail_message_artifact ON gmail_message (artifact_id);
-- Sync checkpoint: every change up to history_id is stored, as of run_id.
CREATE TABLE IF NOT EXISTS gmail_state (
    origin_id INTEGER PRIMARY KEY,
    history_id TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    synced_at INTEGER NOT NULL
);

-- A lesser copy joined to another copy's message although their bytes
-- differ (a Takeout copy of a Gmail API copy); both stay stored.
CREATE TABLE IF NOT EXISTS fold (
    artifact_id INTEGER PRIMARY KEY,
    copy_of INTEGER NOT NULL,
    -- e.g. 'takeout final-crlf'
    reason TEXT NOT NULL
);

-- Messages removed (their copies joined another), still to drop from data/text/.
CREATE TABLE IF NOT EXISTS text_forget (msg_id INTEGER PRIMARY KEY);

-- a copy last seen at path in last_seen_run and not there in gone_run
CREATE TABLE IF NOT EXISTS removal (
    artifact_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    last_seen_run INTEGER NOT NULL,
    gone_run INTEGER NOT NULL,
    -- path it turned up at in gone_run; NULL if found nowhere new
    moved_to TEXT,
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (artifact_id, path, last_seen_run, gone_run)
);
CREATE INDEX IF NOT EXISTS removal_gone ON removal (gone_run);
";

/// Whether `stmt` is `ALTER TABLE t ADD COLUMN c` for a column `t` already
/// has: `SCHEMA` creates tables new to an old database in their latest shape.
async fn adds_existing_column(conn: &Connection, stmt: &str) -> Result<bool> {
    let words: Vec<&str> = stmt.split_whitespace().collect();
    let [alter, table, t, add, column, c, ..] = words[..] else {
        return Ok(false);
    };
    let is_add = [alter, table, add, column]
        .iter()
        .zip(["alter", "table", "add", "column"])
        .all(|(w, k)| w.eq_ignore_ascii_case(k));
    if !is_add {
        return Ok(false);
    }
    let mut rows = conn.query(&format!("PRAGMA table_info({t})"), ()).await?;
    while let Some(r) = rows.next().await? {
        if r.get::<String>(1)?.eq_ignore_ascii_case(c) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub struct Store {
    _db: Database,
    pub conn: Connection,
    pub data_dir: PathBuf,
    /// External objects written since the last commit, not yet synced.
    unsynced: Cell<bool>,
}

impl Store {
    pub async fn open(data_dir: &Path) -> Result<Store> {
        fs::create_dir_all(data_dir.join("content"))
            .with_context(|| format!("creating {}", data_dir.display()))?;
        let path = data_dir.join("email.sqlite");
        let db = Builder::new_local(path.to_str().context("non-UTF-8 data path")?)
            .build()
            .await
            .map_err(|e| {
                let held = e.to_string().contains("locked by another process");
                let e = anyhow::Error::from(e);
                if held {
                    e.context(format!(
                        "another munchymail process (import, index, ocr, or a long query) holds {}; wait for it, or see which with `fuser -v {}`",
                        path.display(),
                        path.display()
                    ))
                } else {
                    e
                }
            })?;
        let conn = db.connect()?;
        conn.execute("BEGIN", ()).await?;
        conn.execute_batch(SCHEMA).await?;
        let mut rows = conn
            .query("SELECT value FROM meta WHERE key = 'schema_version'", ())
            .await?;
        match rows.next().await? {
            Some(r) => {
                let v: String = r.get(0)?;
                drop(rows);
                let found = v
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (1..=SCHEMA_VERSION).contains(n));
                let Some(found) = found else {
                    bail!(
                        "schema version {v} in {}, expected {SCHEMA_VERSION}",
                        path.display()
                    );
                };
                for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize - 1) {
                    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                        if !adds_existing_column(&conn, stmt).await? {
                            conn.execute(stmt, ()).await?;
                        }
                    }
                    info!(to = i + 2, "migrated schema");
                }
                if found != SCHEMA_VERSION {
                    conn.execute(
                        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                        (SCHEMA_VERSION.to_string(),),
                    )
                    .await?;
                }
            }
            None => {
                drop(rows);
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    (SCHEMA_VERSION.to_string(),),
                )
                .await?;
            }
        }
        conn.execute_batch(LATE_SCHEMA).await?;
        conn.execute("COMMIT", ()).await?;
        Ok(Store {
            _db: db,
            conn,
            data_dir: data_dir.to_path_buf(),
            unsynced: Cell::new(false),
        })
    }

    pub fn content_path(&self, hash: &[u8]) -> PathBuf {
        let h = hex(hash);
        self.data_dir.join("content").join(&h[..2]).join(h)
    }

    /// Write an external object if absent. `commit` syncs the filesystem first,
    /// so committed rows never point at a missing or partial file.
    pub fn put_external(&self, hash: &[u8], bytes: &[u8]) -> Result<()> {
        let dest = self.content_path(hash);
        // A length mismatch is a leftover from a crash before the sync; rewrite it.
        if fs::metadata(&dest).is_ok_and(|m| m.len() == bytes.len() as u64) {
            return Ok(());
        }
        let dir = dest.parent().context("content path has no parent")?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".tmp.{}", std::process::id()));
        fs::File::create(&tmp)?.write_all(bytes)?;
        fs::rename(&tmp, &dest)?;
        self.unsynced.set(true);
        Ok(())
    }

    fn sync_content(&self) -> Result<()> {
        if self.unsynced.replace(false) {
            let dir = fs::File::open(self.data_dir.join("content"))?;
            // SAFETY: valid fd for the duration of the call.
            if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
                self.unsynced.set(true);
                return Err(std::io::Error::last_os_error()).context("syncfs on content store");
            }
        }
        Ok(())
    }

    /// Decoded value of a message's first `name` header, or "" if it has none.
    pub async fn first_header(&self, artifact_id: i64, name: &str) -> Result<String> {
        let mut rows = self
            .conn
            .query(
                "SELECT value FROM header WHERE artifact_id = ?1 AND nth = 0 AND name = ?2",
                (artifact_id, name),
            )
            .await?;
        Ok(match rows.next().await? {
            Some(r) => r.get(0)?,
            None => String::new(),
        })
    }

    pub async fn object(&self, hash: &[u8]) -> Result<Vec<u8>> {
        let mut rows = self
            .conn
            .query(
                "SELECT inline_bytes FROM object WHERE hash = ?1",
                (hash.to_vec(),),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .with_context(|| format!("object {} missing", hex(hash)))?;
        match row.get_value(0)? {
            Value::Blob(b) => Ok(b),
            Value::Null => Ok(fs::read(self.content_path(hash))?),
            v => bail!("object {} has unexpected value {v:?}", hex(hash)),
        }
    }

    pub async fn begin(&self) -> Result<()> {
        self.conn.execute("BEGIN", ()).await?;
        Ok(())
    }

    pub async fn commit(&self) -> Result<()> {
        self.sync_content()?;
        self.conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    /// Fold the WAL back into the database; large imports otherwise leave it to slow every open.
    pub async fn checkpoint(&self) -> Result<()> {
        self.conn
            .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
            .await?
            .next()
            .await?;
        Ok(())
    }

    pub async fn rollback(&self) -> Result<()> {
        self.conn.execute("ROLLBACK", ()).await?;
        Ok(())
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
