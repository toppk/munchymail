# Index internals

Schema in `src/store.rs`; everything lives in `data/` (`MUNCHYMAIL_DATA` or
`--data`). Command usage is in `README.md`.

## Tables

| Table | Holds |
|---|---|
| `object` | SHA-256 → bytes. `inline_bytes` NULL means `data/content/aa/<hash>` (objects > 16 KiB). |
| `artifact` | One exact message: `header_hash`, `body_hash`, plus `date`, `sender`, `subject`, `message_id` for listing. |
| `header` | Derived: per artifact, ordered fields; `name` lowercased, `value` unfolded and RFC 2047-decoded; `nth` = occurrence of that name in the message (0 = first, i.e. Sieve `:index` − 1). |
| `origin` | Registered roots (directory tree or single mbox file). |
| `import_run` | One per origin per `import`. |
| `file_content` | Whole-file SHA-256, `head_hash` (first 64 KiB), message count, and `parent_content_id` when an earlier content is a byte prefix (a growing mbox across backups). |
| `file_sighting` | Content seen at `path` (relative to origin root) in a run, with dev/ino/mtime/ctime. |
| `observation` | Artifact at byte `offset` in a content, with its `From ` line and separator bytes. |

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
