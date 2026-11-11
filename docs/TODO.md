# TODO

Roughly in priority order within each section. Phases refer to
`docs/design.md`.

## Agent search and read (phase 1)

- [ ] OCR into `extract` (tesseract, `ocrmypdf` for PDFs with no text
      layer; English) as a background job, sandboxed like the other tools:
      86 PDFs here have no text layer, plus ~2,200 images (most are logos
      and photos; maybe only OCR attachments, not inline parts). (Done:
      bodies, text parts, attached messages, PDF text layer, office files;
      Tantivy in `data/text/` because turso's `fts` index type is
      experimental and would make the database unreadable to `sqlite3`.)
- [ ] Text inside zips (documents in a zip part); the `.docm`/`.xlsm` here
      are macro-only droppers with no text.
- [ ] `--text` ranking (`--sort relevance`); today results stay newest first.
- [ ] `read mN --attachment K`: extracted text (untrusted-wrapped) and a way
      to save the original bytes.
- [ ] 7z/rar member names (only zip is listed today); nested zips.
- [ ] Move search/read behind a service (one process owns the database;
      the CLI over a Unix socket, later the web UI). `src/api.rs` is the
      layer it will wrap.

Agent-friendly CLI gaps (against an agent-friendly CLI checklist):

- [ ] `describe`: commands, arguments and effects generated from the clap
      definitions, so it can't drift; the service API reuses it.
- [ ] Name each command's effect: read-only, or writes `data/` (`import`,
      `export`, `origin move/retire`), visible in `describe` and `--help`.
- [ ] Render text and JSON from one value, so `search --long` and `--json`
      carry the same facts (text lacks the snippet today).
- [ ] Every error names a fix: unknown ID or origin points at the command
      that lists them; the database-locked error says another munchymail
      process holds `data/email.sqlite`.

## Sieve rule work (current use)

- [ ] Negated filters (`--not-header`, `--not-from`, …) so a profile can be
      "subscribed minus the repos I keep".
- [ ] Run a Sieve script over the corpus and report which action each message
      gets (via Pigeonhole `sieve-test` or `sieve-filter`), so a whole rule
      set can be checked against a date range before deploying it.
- [ ] `report` keys over any occurrence for plain values (today plain `NAME`
      keys use the first occurrence; tests already use all).
- [ ] An `address` table filled at import so `--from/--to` and `:addr` keys
      don't parse addresses at query time.

## mbox-sort

- [ ] Merge mode: sort several mboxes into one new file (e.g. a backfill
      folder plus the live one) instead of sorting after appending.
- [ ] Zone-aware `--key received` (Dovecot writes local time without a zone;
      the DST fall-back hour can tie or swap).
- [ ] Run it for real on `lists/github-project` (procedure in docs/mbox-sort.md).

## Import speed

Cold import of the 3.3 GB inbox: lock held 0.89 s, split 1.0 s, hash/parse
0.18 s, **inserts 30 s**. Already one transaction per file and prepared
statements reused per run; the cost is ~3M executions, mostly 2.7M header
rows each updating four B-trees (table, PK, `header_name_nth_value`,
`header_artifact_nth_name`). Unprofiled; measure each against the inbox:

- [ ] Profile (`perf`) to confirm where the 30 s goes before tuning.
- [ ] Drop the two header indexes before a large import and rebuild them
      after (one sorted build instead of random-order maintenance).
- [ ] Multi-row `INSERT` for headers (e.g. 50 rows per statement).
- [ ] Avoid copying hashes/inline bytes into owned `Value`s per execute
      where turso allows borrowing.

## Corpus (phase 1 remainder)

- [ ] Record lock wait/hold per `file_sighting` (schema migration), not just in logs.
- [ ] Maildir import.
- [ ] Read only the appended tail when a file extends its `parent_content_id`.
- [ ] Detect lineage for files under 64 KiB and across differing heads.
- [ ] Prune old runs (sightings, observations of contents no run needs,
      then unreferenced artifacts and objects), keeping recorded removals.
- [ ] Garbage-collect unreferenced `data/content` objects and `.tmp.*` leftovers.
- [ ] Measure the 16 KiB inline threshold against real size distributions.

## Surgery (phase 2)

- [ ] Fuzzier logical-message grouping beyond `message` (body, Message-ID,
      Date): the same mail with a list footer or a rewritten body, a resend
      hours later (seen: carriers, device vendors). Use DKIM-signed headers;
      explicit and reversible; prefer false negatives.
- [ ] Rule engine with versioned rules and recorded classifications (rule
      language still open: SQL, DSL, or Sieve itself).
- [ ] Previewable views (e.g. `Sent/YYYY/MM`) and anomaly reports
      (unclassified mail, classification vs historical folder conflicts).
- [ ] Export views to Maildir and mbox. Before writing any mail format,
      study Dovecot's storage drivers for how they write safely (done for
      mbox UID headers in docs/mbox-sort.md; `mbox-sort`'s writer is the
      starting point). Any write to a live mailbox outside a Dovecot-stopped
      window must take Dovecot's write locks: `dotlock fcntl flock`.

## Search and MIME (phase 3)

- [ ] Threading.
- [ ] Duplicate-attachment reports (`part.hash` is indexed).
- [ ] Optional reversible MIME extraction (bodies as extents of literals and
      objects), declining anything malformed, signed or unusual.
- [ ] IMAP import and export.

## Live (phase 4, optional)

- [ ] Investigate Dovecot `lib-storage`/`lib-index` seams for a
      corpus-backed storage driver, or an IMAP layer over the corpus.
