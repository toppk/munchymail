# TODO

Roughly in priority order within each section. Phases refer to
`docs/design.md`.

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
- [ ] `munchymail diff` between two import snapshots in the index (today:
      SQL in docs/index.md, or `diff` on mbox files).
- [ ] Garbage-collect unreferenced `data/content` objects and `.tmp.*` leftovers.
- [ ] Measure the 16 KiB inline threshold against real size distributions.

## Surgery (phase 2)

- [ ] Logical-message grouping: link artifacts that are the same email
      (ignore Dovecot `X-UID`/`Status`/`X-Keywords`/`Content-Length`,
      transport headers; use Message-ID, body hash, DKIM-signed headers).
      Explicit and reversible; prefer false negatives.
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

- [ ] Full-text search over bodies.
- [ ] Threading.
- [ ] MIME structure and attachment indexing; duplicate-attachment reports.
- [ ] Optional reversible MIME extraction (bodies as extents of literals and
      objects), declining anything malformed, signed or unusual.
- [ ] IMAP import and export.

## Live (phase 4, optional)

- [ ] Investigate Dovecot `lib-storage`/`lib-index` seams for a
      corpus-backed storage driver, or an IMAP layer over the corpus.
