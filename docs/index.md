# Index and tools

Schema in `src/store.rs`; everything lives in `data/` (`MUNCHYMAIL_DATA` or `--data`).

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

Measured on this machine (176k messages, 5.2 GB of mbox, 5.8M header rows):
full import 79 s; re-import of unchanged files ~1 s; searches 0.04–0.3 s;
`names`/`top` 0.1–0.6 s.

## Commands

```
munchymail origin add LABEL ROOT [--host H] [--note N]
munchymail origin list
munchymail import LABEL...
munchymail search [FILTERS] [--headers | --raw | --count] [-n N]
munchymail report --by KEY... [FILTERS] [-n N] [--tsv]
                                            grouped counts, % of scope and of matches
munchymail top HEADER [FILTERS] [-n N]      most common values
munchymail names [--like PAT] [FILTERS] [-n N]
                                            header names, by messages carrying them
munchymail show ID [--raw | --where]
munchymail stats
munchymail sql 'SELECT ...'
munchymail scan [--list] MBOX...            split only, no database
```

Filters (all must match):

```
-H, --header NAME:PATTERN   decoded header value (Sieve `header`); repeatable
--from/--to PATTERN         any address in From/To (Sieve `address :all`)
--subject PATTERN
--has NAME                  header present
--folder PATTERN            sighting path, e.g. 'lists/*'
--origin LABEL
--since/--before YYYY-MM-DD Date: header, UTC
```

Report keys are `NAME`, `NAME:addr` (first address, lowercased) or
`NAME:domain`; a message lacking the header groups under `(none)`. `%scope`
is against every message in the `--origin/--folder/--since/--before` scope;
`%match` is against those also passing the header/address filters.

Patterns follow Sieve `:matches` with `i;ascii-casemap`: `*`, `?`, `\`
escapes, ASCII case-insensitive. A pattern with no wildcard means `:contains`.

## Developing Sieve rules

The `justfile` wraps the common reports (`just` lists them; `ORIGIN=` for
all origins, `""` leaves a date bound open):

```
just senders 2026-01-01                       # who sends most, % of all mail
just sender-headers '*@substack.com' 2026-01-01   # which X-headers they carry
just sender-by '*@github.com' x-github-reason 2026-01-01
just by list-id 2026-09-01 2026-10-01
```

```
munchymail names --origin inbox                    # what headers exist
munchymail top list-id --origin inbox              # who dominates
munchymail search -H 'list-id:*gear*' --count     # size of a candidate rule
munchymail search -H 'list-id:*gear*' --headers -n 3
munchymail show ID --raw > m.eml && sieve-test rules.sieve m.eml
```

`show --raw` emits the exact stored bytes, so `sieve-test` sees what
Dovecot delivered (including Dovecot's own `X-UID`/`Status` headers when the
copy came from a Dovecot mbox).
