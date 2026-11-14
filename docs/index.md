# Index internals

Schema in `src/store.rs`, one database per account in `data/ADDRESS/`
(`~/.local/share/munchymail` unless `MUNCHYMAIL_DATA` or `--data` says
otherwise); `data/` below means that directory. Command usage is in `README.md`.

## Tables

| Table | Holds |
|---|---|
| `object` | SHA-256 → bytes. `inline_bytes` NULL means `data/content/aa/<hash>` (objects > 16 KiB). |
| `artifact` | One exact stored copy: `header_hash`, `body_hash`, plus `date`, `sender`, `subject`, `message_id` for listing, and `msg_id`. |
| `message` | One email however many copies were stored, keyed by `key` (`key::key`: the body's identity, parsed Message-ID, Date, MIME reading, and header evidence when there is no valid Message-ID; see `docs/identity.md`). `body_hash` is the body's identity (SHA-256 with CRLF read as LF, NUL as 0x80, a final newline added), `message_id` the parsed Message-ID (NULL if none is valid). Never deleted: a message whose copies joined another has `merged_into` set and `key` NULL; one begun by splitting another has `split_from`. Older databases are regrouped once (`rekey`, `meta` key `identity`). |
| `part` | Derived: every MIME leaf part of a message (`kind` body/inline/attachment, file name, `ext`, `declared_type`, `detected_type` from magic numbers, decoded size, SHA-256). `message.parts_version` records which `parts::VERSION` filled it (NULL: not yet), `parts_problem` why MIME didn't parse. |
| `message.text_version` | Which `text::VERSION` put the message in the Tantivy index in `data/text/` (fields `msg`, `subject`, `text`; one document per message). NULL: not yet; set to NULL for all when the directory is missing, so `index` rebuilds it. |
| `extract` | Derived: text a tool got from a document or image part (`index`, `ocr`), once per part `hash`: `version` (`extract::VERSION`), `tool` with its version, `outcome`, `text` (≤ 1 MiB). New rows set `text_version` NULL on the messages holding that part, so `index` re-indexes them. |
| `archive_member` | Derived: file names and sizes inside a zip part, keyed by the part's hash (at most 1000 per zip; read from the central directory, nothing decompressed). |
| `header` | Derived: per artifact, ordered fields; `name` lowercased, `value` unfolded and RFC 2047-decoded; `nth` = occurrence of that name in the message (0 = first, i.e. Sieve `:index` − 1). |
| `alias` | The account's own addresses as Sieve `:matches` patterns (`alias add`); `account add` adds the account's address. |
| `origin` | Registered roots (directory tree or single mbox file); `retired_at` set by `origin retire`; `role` (live, snapshot, export) by `origin role`. |
| `import_run` | One per origin per `import`, and per Gmail or IMAP check that found a change. |
| `file_content` | Whole-file SHA-256, `format` (`dovecot` or `mboxrd`: how its messages were unquoted), `head_hash` (first 64 KiB), message count, and `parent_content_id` when an earlier content is a byte prefix (a growing mbox across backups). |
| `file_sighting` | Content seen at `path` (relative to origin root) in a run, with dev/ino/mtime/ctime. |
| `observation` | Artifact at byte `offset` in a content, with its `From ` line and separator bytes. |
| `presence` | Where copies are in a live listing (a Gmail label, an IMAP folder): one row per `entry` (Gmail id, IMAP `uidvalidity:uid`) at `path`, from `since_run` until `until_run`, the first run it was gone from (NULL while there). A run writes only what came or went; listings recorded whole by older versions are converted once (`meta` key `presence`). |
| `imap_flags` | Each IMAP message's flags when first fetched and at every change, with `seen_at`. |
| `storage_record` | Copies that are a mail store's own records, not mail: Dovecot's "FOLDER INTERNAL DATA" placeholder (`kind` `folder-data`). Kept byte for byte; left out of search, counts and the overview. |
| `gmail_message` | Gmail's record of each message of the `gmail` origin: Gmail id, the artifact of its raw bytes, thread id, `internal_date` (receipt time), current label ids, and `gone_at` when Gmail deleted it (the artifact stays). |
| `gmail_label` | Gmail's label ids and names per origin. |
| `gmail_state` | The sync checkpoint: Gmail history id up to which every change is stored, and the run that stored it. |
| `fold` | A lesser copy (`artifact_id`) joined to `copy_of`'s message although their bytes differ, and why (`reason`, e.g. `takeout final-crlf`, `takeout utf8-damage`); the other copy is the one shown and indexed. Written by Takeout `import`, `gmail sync` and `reconcile`. |
| `text_forget` | Messages removed because their copies joined another; `index` drops them from `data/text/`. |
| `removal` | A copy last seen at `path` in `last_seen_run` and absent in `gone_run`; `moved_to` is where it turned up, NULL if nowhere new. Filled by `import` (per origin), by each Gmail or IMAP run (from the presence rows it closed), and `record-removals`. |

`meta.schema_version` is 11. Opening an older database upgrades it in the
opening transaction (`MIGRATIONS` in `store.rs`: 1 → 2 adds
`origin.retired_at`; 2 → 3 adds `removal` and `file_sighting_run`, which
`SCHEMA` creates; 3 → 4 adds `artifact.msg_id` and fills `message` from
existing artifacts, about 2 s for 262k; `LATE_SCHEMA` then indexes it;
4 → 5 adds `message.parts_version`/`parts_problem`, after which `import` or
`index` fills `part`; 5 → 6 adds `message.text_version`; 8 → 9 renames the `mbox` file format to `dovecot`; 9 → 10 adds `origin.role` and makes Gmail origins live); a newer or unknown version refuses to open.

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

`changes` (`src/changes.rs`) loads the files of each side's runs, skips
paths whose set of `content_id`s is identical on both sides, and loads
observations only for the rest. Within a path it pairs copies with
`diff::compare` (exact bytes, then body hash + Message-ID, which survives
Dovecot rewriting `X-UID`/`Status`). Copies gone from a path are then matched
against copies new at another path (moved); the remainder are removed or
added. A copy that also exists, unchanged, at another path does not count as
a destination, so deleting one of two duplicates is a removal. Comparing
the 3 GB INBOX between two runs takes about 5 s, most of it reading
observations.
