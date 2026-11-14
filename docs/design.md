# munchymail design

A notmuch-like local corpus for a lifetime of mail scattered across live
mailboxes and backups: ingest every surviving copy, keep exact bytes and
where each copy was found, then query, classify and re-export without the
historical folder tree being the source of truth.

`README.md` is the command reference; `docs/architecture.md` is the original
design paper (with `docs/mime-extraction.md` and `docs/surgery.md`);
`docs/ingest.md` covers how sources are read and split; `docs/index.md` covers the schema and performance;
`docs/TODO.md` lists what is not built.

## Thesis

Exact mail bytes are immutable evidence. Everything else (parsed headers,
folders, labels, threads, duplicates) is a reproducible interpretation of
those bytes, and can be thrown away and rebuilt.

## Principles

1. Preserve first, interpret second. Never rewrite or normalise the only copy.
2. Hash exact bytes. Identity is SHA-256 of header bytes and of body bytes.
3. Dedupe storage, never history: one stored object, every sighting recorded.
4. Parsed fields, full-text terms and MIME structure are rebuildable indexes.
5. Classification is replayable rules, not destructive moves; every result
   names the rule (and version) that produced it.
6. MIME-aware dedupe is an optimisation that may decline any input; failing
   to understand a message never means failing to keep it.
7. Whole-object garbage collection, no packed-container compaction.
8. Useful offline, independent of any live mail server.
9. Sources are never written. Only `data/` is.

## Model

| Layer | What it is |
|---|---|
| account | One mail account (`me@example.org`, `me.example@gmail.com`) with its own `data/ADDRESS/`: database, objects, text index. Aliases are its own addresses. Everything below is per account. |
| object | Immutable bytes by SHA-256. Small ones inline in SQLite, large ones in `data/content/aa/<hash>`. |
| artifact | One exact message: (header object, body object). Header ‖ body is the message. |
| origin | A place mail was found: a live mailbox tree, a backup set, an export. |
| file content | A whole source file by SHA-256. Identical files across backups are parsed once; a file that extends another is linked to it. |
| file sighting | A file content seen at a path in an origin during an import run. The path is the historical folder. |
| observation | An artifact at a byte offset in a file content, with its mbox `From ` line and separator. |
| header (derived) | Ordered, unfolded, RFC 2047-decoded header fields per artifact. |
| logical message (later) | Artifacts believed to be the same email (e.g. differing only in `Received:` or Dovecot's `X-UID`). Grouping is explicit and reversible; prefer false negatives. |
| classification (later) | Rule-derived facts and views. |

Why headers stay bytes: RFC 5322 headers are an ordered list with legal
duplicates, folding, and case that signatures and forensics care about. A
name/value table cannot regenerate them, so it is only an index.

Why SQLite plus a content store: Maildir pays an inode per message; mbox and
Dovecot's mdbox pay rewrite/compaction costs because they mutate shared
containers. Immutable objects are either referenced or garbage, and SQLite
handles its own free space.

## Facts, inferences, views

- **Fact**: observed directly. Raw `From:`, a sighting path, a body hash.
- **Inference**: produced by a rule. "sent", "family", "mailing list X".
- **View**: a presentation choice. Put sent mail in `Sent/YYYY/MM`.

Keeping these apart is what lets the old folder tree stay as provenance
while new rules produce a cleaner mailbox.

## Workflow ("repository surgeon")

1. Register origins; never modify them.
2. Read each source under a Dovecot-compatible lock, split byte-exactly.
3. Hash header and body; store objects; record artifact and observation.
4. Build derived indexes.
5. Exact dedupe now; logical grouping later.
6. Run rules over the whole corpus; review anomalies and conflicts.
7. Materialise views; export to Maildir, mbox or IMAP.
8. Repeat at will; exports are disposable.

The first concrete use is developing Sieve rules for the live inbox: search
by decoded header values with Sieve-style wildcard matching, see which
values dominate, and inspect full header blocks.

## Phases

1. **Corpus**: schema, mbox import with origins, exact dedupe, search CLI.
2. **Surgery**: rule engine, previewable views, Maildir/mbox export, anomaly
   reports, logical grouping. Maildir import.
3. **Search and MIME**: full-text, threading, attachment indexing, optional
   reversible MIME extraction (body as extents of literals and objects).
4. **Live (optional)**: investigate Dovecot `lib-storage`/`lib-index` seams,
   or an IMAP layer over the corpus. Any writing of mail formats starts by
   studying Dovecot's storage drivers.

## Status

Phase 1 is built for mbox: origins, Dovecot-compatible locked reads,
byte-exact splitting, file- and message-level dedupe with provenance, a
decoded header index, and the Sieve-oriented query tools (`search`,
`report`, `profile`, `top`, `names`, `show`), `diff` for verifying mbox
surgery, `read` and `search --json` (decoded text, attachments, locations, JSON
Lines with stable paging) and `describe` (commands, arguments and
effects from the parser's definitions) for agents, message ids (`m123`: same body,
Message-ID and Date) that survive Dovecot's header rewrites, a MIME part
index (declared vs detected type, zip member names; `attachments`,
`--attachment`), full-text search over bodies, text attachments,
attached messages, documents and OCR of images and scanned PDFs
(`--text`, Tantivy; external tools sandboxed with bwrap), `changes` and
recorded removals for comparing import runs, `export` (a `.tgz` of matching
messages with an index), and a separate `mbox-sort` binary that writes
date-sorted copies (the first code that writes a mail format; never in
place). Not yet: Maildir, fuzzier logical grouping, and everything from
phase 2 on (see `docs/TODO.md`). Accounts: each mail account has its own
data directory and aliases; Gmail comes in through its API. `serve` holds
the databases and runs the CLI's commands sent over a Unix socket.

## Open questions

- Inline threshold (currently 16 KiB) — measure on real corpora.
- Rule language: SQL, a small DSL, or Sieve itself?
- Logical grouping scoring: Message-ID, DKIM-signed headers, body identity,
  transport chronology.
- Representing mutable user state (Seen, Flagged, keywords) offline.
- Which MIME encodings are predictable enough for reversible extraction.
- Versioning derived indexes so they can be rebuilt after parser changes.
- One object namespace for headers and bodies, or two.
- With IMAP sync, telling corpus-derived classifications from the user's
  own mailbox changes.
- Per-account object stores for good, or later a shared one.
- The hash and collision policy to fix in the on-disk format.
