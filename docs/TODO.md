# TODO

## Import speed

Cold import of the 3.3 GB inbox: lock held 0.89 s, split 1.0 s, hash/parse
0.18 s, **inserts 30 s**. Already one transaction per file and prepared
statements reused per run; the cost is ~3M executions, mostly 2.7M header
rows each updating four B-trees (table, PK, `header_name_nth_value`,
`header_artifact_nth_name`). Unprofiled; measure each against the inbox:

- [ ] Drop the two header indexes before a large import and rebuild them
      after (one sorted build instead of random-order maintenance).
- [ ] Multi-row `INSERT` for headers (e.g. 50 rows per statement) to cut
      per-execute overhead.
- [ ] Avoid copying hashes/inline bytes into owned `Value`s per execute
      where turso allows borrowing.
- [ ] Profile (`perf`) to confirm where the 30 s goes before tuning further.

## Corpus

- [ ] Record lock wait/hold per `file_sighting` (schema migration), not just in logs.
- [ ] Read only the appended tail when a file extends its `parent_content_id`.
- [ ] Maildir import.
- [ ] Garbage-collect unreferenced `data/content` objects and `.tmp.*` leftovers.
- [ ] `--folder` matches any historical sighting; add "current location only".
- [ ] Logical-message grouping (ignore Dovecot `X-UID`/`Status`/`X-Keywords`
      etc. when comparing copies).

## Search

- [ ] Full-text search over bodies.
- [ ] An `address` table at import so `--from/--to` and `:addr` keys don't parse at query time.
