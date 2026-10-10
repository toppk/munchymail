# Index internals

Schema in `src/store.rs`; everything lives in `data/` (`MUNCHYMAIL_DATA` or
`--data`). Command usage is in `README.md`.

## Tables

| Table | Holds |
|---|---|
| `object` | SHA-256 → bytes. `inline_bytes` NULL means `data/content/aa/<hash>` (objects > 16 KiB). |
| `artifact` | One exact message: `header_hash`, `body_hash`, plus `date`, `sender`, `subject`, `message_id` for listing. |
| `header` | Derived: per artifact, ordered fields; `name` lowercased, `value` unfolded and RFC 2047-decoded; `nth` = occurrence of that name in the message (0 = first, i.e. Sieve `:index` − 1). |
| `origin` | Registered roots (directory tree or single mbox file); `retired_at` set by `origin retire`. |
| `import_run` | One per origin per `import`. |
| `file_content` | Whole-file SHA-256, `head_hash` (first 64 KiB), message count, and `parent_content_id` when an earlier content is a byte prefix (a growing mbox across backups). |
| `file_sighting` | Content seen at `path` (relative to origin root) in a run, with dev/ino/mtime/ctime. |
| `observation` | Artifact at byte `offset` in a content, with its `From ` line and separator bytes. |

`meta.schema_version` is 2. Opening an older database upgrades it in the
opening transaction (`MIGRATIONS` in `store.rs`, version 1 → 2 adds
`origin.retired_at`); a newer or unknown version refuses to open.

## Performance rules

- Every write is inside an explicit transaction; one per source file.
- All import statements are prepared once per run and reused.
- External objects are written then renamed without per-file fsync; `commit`
  calls `syncfs` on the content store once before `COMMIT`.
- `import` ends with `PRAGMA wal_checkpoint(TRUNCATE)`; a large WAL makes
  every later open slow.
- turso scans `IN (subquery with joins)` linearly and is very slow at
  correlated `EXISTS` and `DISTINCT` subqueries. Filters are therefore
  materialised into `TEMP` tables keyed by `artifact_id` and intersected by
  primary-key joins (`query.rs`). `CROSS JOIN` forces join order where the
  planner, lacking statistics, picks the wrong side.
- Address (`--from/--to`) and regex (`-R`) filters can't be expressed in
  SQL: candidates are narrowed by header name (and a LIKE on the longest
  literal run of the pattern), then decided in Rust.
- `report` and `profile` fetch per-message header values and aggregate in
  Rust rather than relying on `count(DISTINCT)`.

Measured on this machine (176k messages, 5.2 GB of mbox, 5.8M header rows):
full import 76–79 s (30 s of it inserting the inbox, see TODO); re-import of
unchanged files ~1 s; `search` 0.04–0.3 s; `report`/`profile` 0.1–0.35 s;
`names`/`top` 0.1–0.6 s.

## Comparing snapshots

Each `import` adds a `file_sighting` per file per run, so earlier states
stay queryable. Find the contents, then count each message (body hash +
Message-ID, which survives Dovecot rewriting `X-UID`) across them:

```
munchymail sql "SELECT s.run_id, s.path, s.content_id, c.message_count
  FROM file_sighting s JOIN file_content c ON c.content_id = s.content_id ORDER BY s.run_id"

munchymail sql "SELECT o, i, d, count(*) FROM (
    SELECT b, m, sum(src = 58) AS o, sum(src = 59) AS i, sum(src = 60) AS d FROM (
      SELECT ob.content_id AS src, a.body_hash AS b, coalesce(a.message_id, '') AS m
      FROM observation ob JOIN artifact a ON a.artifact_id = ob.artifact_id
      WHERE ob.content_id IN (58, 59, 60))
    GROUP BY b, m)
  GROUP BY o, i, d ORDER BY 4 DESC"
```

(58 = INBOX before the GitHub surgery, 59 = INBOX after, 60 =
`lists/github-project`.) A row with copies before and none after anywhere
is lost mail.

