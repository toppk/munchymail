# TODO

Roughly in priority order within each section. Phases refer to
`docs/design.md`.

## Agent search and read (phase 1)

- [ ] OCR: keep tesseract's text when it exits 1 over a truncated JPEG
      (libjpeg warnings; output may be fine). Run `ocr` after `import`
      automatically, e.g. a systemd timer, once the service exists.
      (Text search is Tantivy in `data/text/` because turso's `fts` index
      type is experimental and would make the database unreadable to
      `sqlite3`.)
- [ ] Text inside zips (documents in a zip part); the `.docm`/`.xlsm` here
      are macro-only droppers with no text.
- [ ] `--text` ranking (`--sort relevance`); today results stay newest first.
- [ ] `read mN --attachment K`: extracted text (untrusted-wrapped); the
      original bytes are `attachment mN K`.
- [ ] 7z/rar member names (only zip is listed today); nested zips.
- [x] `serve`: one process owns the databases; the CLI forwards its
      command line over `$XDG_RUNTIME_DIR/munchymail/serve.sock` (reads
      concurrent, writes in turn). Installed per `docs/install.md`.
- [ ] The service's own work (Gmail sync, mbox scans, OCR after import):
      designed in `docs/sync.md`; origin roles are done.
- [ ] Web UI and Android over HTTP with auth, through `src/api.rs`; the
      socket is same-user only and runs any command.

## Accounts and Gmail (phase 5)

- [x] Cross-account search: without `--account`, `search` covers every
      account in turn, newest match first, with an account-naming cursor.
- [ ] Account-numbered ids (`2:m16596`) so `read`/`show`/`attachment` work
      across accounts without `--account`; numbers stored, never reused.
- [x] `gmail auth`/`gmail status`: read-only sign-in, secrets in
      `~/.config/munchymail/creds/`.
- [x] `gmail sync` through the Gmail API, read-only: `format=raw` bytes,
      labels as paths of a run, incremental via history, full listing when
      history has expired.
- [ ] The service runs Gmail sync itself, woken by new mail: poll
      history.list (2 units) every ~30 s, or users.watch with a Pub/Sub
      pull subscription. Gmail IMAP IDLE needs the full-access
      `https://mail.google.com/` scope (no read-only IMAP scope exists).
- [x] First full sync of a 19.5k-message mailbox: 3 h, held to ~100
      messages a minute by "Units per minute per user" errors (a Google
      project shared with other tools); incremental syncs are cheap, so a
      project of munchymail's own is needed only if that bites.
- [ ] Gmail's batch endpoint (fewer HTTP round trips; same quota cost).
  - Gmail labels and presence are source records, kept apart from the
    archive's own tags and rules; a deletion or label removal in Gmail
    updates the source record and never removes archived mail.
  - Advance the history checkpoint only after every change it covers is
    committed. A full rescan marks messages or labels absent only once
    the scan has completed.
  - Before relying on dedupe, compare Takeout and API copies of the same
    messages (8-bit, long lines, attachments, odd encodings): both raw
    copies and their origins are kept even when they fold into one message.
  - OAuth: the consent screen is In production (in Testing, refresh
    tokens expire after 7 days and unattended sync would break).
- [x] Takeout: no importer of its own; its `.mbox` imports as an origin of
      the Gmail account (`X-Gmail-Labels` is then just a header).
- [x] Takeout vs API (2026-11-12, 19,509 shared): the API copy is the
      original each time. Takeout copies join their API copy's message
      (`fold`) by thread id and Message-ID when they differ only by Takeout's
      headers, an added final CRLF (2,652) or U+FFFD for 8-bit bytes (76);
      one, a raw binary attachment, stays separate. Same result whichever
      of import and sync comes first (checked on the real data).
- [ ] Around 2026-12-15, before the next Takeout: send test mail (Gmail web
      to itself and to me@example.org; from example.org a Latin-1 8-bit
      message with `From ` and `>From ` lines) to confirm the above.
- [ ] Optional `--detect` for the mbox variant (writer fingerprints such as
      Takeout's `X-GM-THRID`); the configured variant stays required.
- [ ] Sending (later): through the originating account, with the sent copy
      visible there; verify each provider's Sent behaviour (Gmail files API
      sends itself; Dovecot needs an IMAP APPEND or a locked mbox write).

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

Cold import of a 3.3 GB inbox: lock held 0.89 s, split 1.0 s, hash/parse
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
      hours later (seen from carriers and device vendors). Use DKIM-signed headers;
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
