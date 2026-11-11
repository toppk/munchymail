# munchymail

An offline, notmuch-like corpus for mail scattered across live mailboxes and
backups. It imports every copy of every message byte-for-byte, remembers
where each copy was found, and lets you slice the result by any header,
which is mainly for working out Sieve rules against real mail.

Sources are only ever read, under the same shared `fcntl` lock a Dovecot
reader takes. Everything munchymail writes lives in `data/`.

- `docs/design.md`: goals, data model and phases
- `docs/ingest.md`: how mbox files are read, locked and split
- `docs/index.md`: schema and performance notes
- `docs/mbox-sort.md`: the `mbox-sort` binary and how to install its output
- `docs/TODO.md`: what is not built yet

## Build

```
cargo build --release          # binary: target/release/munchymail
just check                     # fmt, clippy -D warnings, tests
```

Use the release build; debug builds are much slower. The `justfile` only
has `build`, `check` and `run` (`just run search --count` = `munchymail
search --count`). The examples below call the binary directly.

## Getting mail in

```
munchymail origin add mail /var/mail/me --host mailhost --note "live dovecot tree: INBOX + folders"
munchymail origin list
munchymail import                     # all origins; or: munchymail import mail
munchymail stats
```

An origin is a directory tree (walked; dot-entries like Dovecot's `.imap`
are skipped) or a single mbox file. Files that don't start with a valid
`From ` line are skipped. Import is idempotent: unchanged files are
recognised by hash and only get a new sighting, so re-running is cheap.
Import ends by indexing new messages' MIME parts and text (`munchymail
index` does only that step; the whole archive takes about 20 s for parts
and 40 s for text, plus about 8 s to extract documents).

Every import is kept as a run, so each re-import adds a generation:
earlier snapshots of a file stay queryable (`--current` selects only the
latest run per origin; `changes` compares runs). Only new
messages cost space; a header-only change (Dovecot writing `X-UID` or
`Status`) shares the stored body.

Origins are keyed by root path. When mail moves, add a new origin for the
new root and retire the old ones; their runs stay as history:

```
munchymail origin add mail /var/mail/me --host mailhost --note "INBOX + folders"
munchymail origin retire inbox          # was /var/mail/me, a single file
munchymail origin retire home-mail      # was ~/mail
munchymail origin retire --undo inbox   # if needed
```

A retired origin is skipped by `import`, refused when named, and ignored by
`--current` unless selected with `--origin`. Folder paths inside a tree
(`lists/github-project`) stay the same across such a move, so `--folder`
matches old and new runs; the old single-file INBOX was recorded as `me`.

When the same files only change place (e.g. a backup filed under
`~/mail-backups/`), keep the origin and repoint it instead:

```
munchymail origin move spool-2021-07 ~/mail-backups/spool-2021-07-07/me
```

Global options: `--data DIR` (or `MUNCHYMAIL_DATA`, default `data`).
Logs go to stderr; `MUNCHYMAIL_LOG=munchymail=debug` for more. Each file
read logs how long its lock was waited for and held:

```
INFO munchymail::source: read under fcntl read lock path=/var/mail/me/INBOX
     bytes=3330473224 lock_wait=4.598µs lock_held=888.296303ms ...
```

## Filters

`search`, `report`, `profile`, `top`, `names`, `attachments` and `export`
share these; all given must match:

| Option | Matches |
|---|---|
| `-H, --header NAME:PATTERN` | decoded header value, like Sieve `header` (repeatable) |
| `-R, --regex NAME:REGEX` | decoded header value, case-insensitive regex, like Sieve `header :regex` (repeatable) |
| `--from PATTERN`, `--to PATTERN` | any address in From/To, like Sieve `address :all` |
| `--participant PATTERN` | any address in From, Sender, Reply-To, To, Cc or Bcc: mail sent or received by someone (repeatable) |
| `--subject PATTERN` | same as `-H subject:PATTERN` |
| `--has NAME` | header present (repeatable) |
| `--attachment TYPE` | a part other than the message text of this type: file extension, MIME type detected from its bytes or declared by the sender, or either half of one, exact and case-insensitive: `pdf`, `zip`, `image`, `application/pdf` (repeatable) |
| `--attachment-name PATTERN` | a part's file name, or a file name inside a zip part, e.g. `'*.xlsx'` (repeatable) |
| `--text QUERY` | words in the Subject or text; see [Text search](#text-search) |
| `--origin LABEL` | found in that origin |
| `--folder PATTERN` | found at a matching path inside an origin, ever, e.g. `'lists/*'` |
| `--current` | restrict `--origin`/`--folder` to each origin's latest import (where mail is now), not any past sighting; retired origins are left out unless named by `--origin` |
| `--since DATE`, `--before DATE` | `Date:` header, `YYYY-MM-DD`, UTC; `--before` is exclusive |

PATTERN follows Sieve `:matches` with `i;ascii-casemap`: `*` and `?`
wildcards, `\` escapes, ASCII case-insensitive, anchored at both ends. A
pattern with no wildcard means "contains". Header values are matched after
unfolding and RFC 2047 decoding, which is what Sieve compares.

`--from` matches addresses, not the raw header: `--from '*@substack.com'`
finds `News <news@substack.com>`, while `-H 'from:*@substack.com'` does not
(the value ends in `>`), exactly as in Sieve.

There is no negation yet (see TODO), except within `--text`.

## Commands

### search: list messages

```
munchymail search --origin inbox --since 2026-10-01 -n 20
munchymail search --from '*@isp.example' --subject invoice --count
munchymail search -R 'list-id:^widgets/' --headers -n 3     # raw header blocks
munchymail search --has list-unsubscribe --raw -n 1        # whole raw messages
munchymail search --participant '*@corp.example' --long     # folder, From, To, size, attachments
munchymail search --participant '*@corp.example' --tsv -n 1000 > corp.tsv
munchymail search --participant '*@corp.example' --unique --json -n 20   # for agents
```

Output is `id  date  from  subject`, newest first. The id is a stored
copy's, what `show` takes; see [Messages and copies](#messages-and-copies).
`--long` adds, per message, the folders it is in now (or "gone; last seen
in PATH DATE"), date and time (UTC), the message id, From, To, body size in
bytes as stored (MIME-encoded), and the number of attachments (parts with a
file name or `Content-Disposition: attachment`); `--tsv` gives the same as
columns. Folder is per stored copy: a copy whose headers Dovecot later
rewrote shows as gone, next to its current copy; `--unique` lists each
message once (its newest copy, with the places of all its copies), as
`export` counts them, and `--current` leaves out copies no longer in any
folder.

Results come in pages of `-n` (1 to 1000, default 50), newest first by
`Date:` with ties broken by id, so pages are stable. When there are more,
the next cursor is logged to stderr (`more results: add --after CURSOR`).
`--json` prints JSON Lines: one `{"type":"message",...}` record per message
(copy `id`, `message` id, date, Message-ID, From, To, subject, a
160-character snippet of the
text body, body size, attachment count, `locations`, `problem` if it didn't
decode, `"untrusted": true`), then `{"type":"page","returned":N,"next":CURSOR
or null}`. Each location has `origin`, `path`, `current`, `first_seen`,
`last_seen` and `origin_last_import`, so a reader can tell how fresh the
answer is and that a copy has left a folder. Text from mail has control
characters and bidi overrides replaced in every output form except
`--headers`/`--raw`.

### report: grouped counts with percentages

```
munchymail report --by KEY [--by KEY ...] [FILTERS] [-n N] [--tsv]
```

A KEY is one of:

| KEY | Groups by |
|---|---|
| `NAME` | first value of the header |
| `NAME:addr` | first address in it, lowercased |
| `NAME:domain` | domain of that address |
| `NAME~REGEX` | `match` / `no match` (Sieve `:regex`, any occurrence) |
| `NAME=PATTERN` | `match` / `no match` (Sieve `:matches`, any occurrence) |

A message without the header groups as `(none)` (Sieve treats that as no
match). `%scope` is the share of all mail in the `--origin/--folder/--since/
--before` scope; `%match` is the share of mail that also passes the other
filters. Groups past `-n` are summed on one line.

```
$ munchymail report --origin inbox --since 2026-01-01 --by from:addr -n 3
scope 18549 messages, matched 18549 (100.0%), 1230 groups
  count %scope %match  from:addr
   7503  40.4%  40.4%  notifications@github.com
   2041  11.0%  11.0%  updates@video.example
   1040   5.6%   5.6%  newsletter@letters.example
   7965  42.9%  42.9%  (1227 more groups)

$ munchymail report --origin inbox --since 2026-01-01 --from '*@github.com' \
    -H 'x-github-reason:subscribed' --by 'list-id~^(acme|widgets)/' --by list-id
scope 18549 messages, matched 5542 (29.9%), 7 groups
  count %scope %match  list-id~^(acme|widgets)/  list-id
   2537  13.7%  45.8%  match                      acme/anvil <anvil.acme.github.com>
   ...
     27   0.1%   0.5%  no match                   example/website <website.example.github.com>
```

### profile: what every header looks like

```
munchymail profile [--like NAME-PATTERN] [FILTERS] [-k N]
```

For each header among the matching messages (first occurrence per
message): how many messages carry it, how many distinct values it has, and
its top `-k` values. Headers are sorted so useful ones come first:
categorical (2–50 values), then constant, then identifiers (all-unique or
more than 50 values). This finds the header to write a rule on without
opening sample mail.

```
munchymail profile --origin inbox --from '*@github.com' --since 2026-01-01 --like 'x-*'
munchymail profile --origin inbox --from '*@github.com' -H 'x-github-reason:subscribed'
```

### top, names: quick single-column counts

```
munchymail top list-id --origin inbox              # values of one header (any occurrence)
munchymail names --origin inbox --like 'list-*'    # header names, by messages carrying them
```

Both print counts and percentages of the matching messages.

### read: one message, decoded

```
munchymail read m1234          # a message: locations, attachments, then headers and text body
munchymail read 227617         # one stored copy, and only its locations
munchymail read m1234 --json   # one object: all decoded headers, text, attachments, locations
```

A message id shows the copy seen most recently and the places of every
copy; `copies` lists the copy ids.

The text body is the first `text/plain` part, decoded (transfer encoding
and charset); without one, `text/html` converted to plain text
(`text_source` says which). Every other part is listed: `attachment` (has a
file name or `Content-Disposition: attachment`) or `inline` (e.g. an image
the HTML shows), with decoded size, the type found in its bytes when it
differs from the declared one, and the file names inside a zip. A message whose MIME
structure doesn't parse is still shown, from its raw body, with a
`problem`. The text form puts the mail between `<<<untrusted mail N>>>` and
`<<<end untrusted mail N>>>`, where N comes from the SHA-256 of the whole
message (including headers our server added), so a sender can't forge the
end marker. A message that left every folder stays readable; its
locations say `was`. `show --raw` gives the exact bytes.

### attachments: MIME parts by type

```
munchymail attachments                       # most common types of non-text parts
munchymail attachments --all-parts           # including text/plain and text/html bodies
munchymail attachments --by mismatch         # declared type -> type the bytes are
munchymail attachments --by ext --since 2024-01-01
munchymail search --attachment pdf --unique  # messages with a PDF, however declared
munchymail search --attachment-name '*.js'   # e.g. scripts inside zips
```

Counts parts, messages and decoded MiB per key: `type` (detected, else
declared; the default), `declared`, `detected`, `ext`, or `mismatch`. Types
are detected by magic numbers (`infer`), except that PEM text and DER data
are only called certificates when they are (PGP armour is named as such).
The index is in `part` and `archive_member`; until `index` has run on a
message, `--attachment*` filters miss it and a warning says so.

### Text search

```
munchymail search --text invoice --unique --long        # invoices too (stemmed)
munchymail search --text '"tracking number"' --since 2026-01-01
munchymail search --text 'sieve -spam' --folder 'lists/*' --count
munchymail search --text 'subject:passport OR subject:visa' --json
```

`--text` searches the Subject and the text of each message: the text/plain
body (or the text/html one converted, when there is no plain one), every
other text/* part (text attachments, calendars, patches, logs) and attached
messages (`message/rfc822`, their Subject and text, nested up to 3 deep)
and documents (below), at most 1 MiB per part. Words are English-stemmed and case-insensitive, and
all must match; `"a phrase"`, `a OR b`, `-word` and `subject:word` /
`text:word` work as in Tantivy's query syntax. It combines with every
other filter. `--long` adds where it matched (`body`, `attachment NAME`,
`inline`, `subject`) and a fragment around the match; `--json` puts the
fragment in `snippet` and adds `match` (`source`, `part`, `filename`,
`highlighted` byte ranges of the snippet).

Documents are found by detected type (else declared type and extension)
and extracted once per distinct file:

| Format | Tool |
|---|---|
| PDF | `pdftotext` (text layer only; scans come out empty) |
| docx, xlsx, pptx, odt, ods, odp | read in-process from the XML inside the zip (every sheet's strings) |
| doc | `catdoc`, else `antiword` |
| xls | `xls2csv` |
| rtf | `pandoc` |

External tools run under `bwrap`: read-only `/usr`, a private `/tmp`, no
network or other namespaces, one file at `/in`, 60 s, 1 GiB data and 1 MiB
of output; the file is written to `data/tmp/` for the run and removed.
Without `bwrap` only the in-process formats are extracted. What each tool
made of each file is in the `extract` table (`ok`, `empty`, `truncated`,
`timeout`, `failed: …`), e.g.:

```
munchymail sql "SELECT tool, outcome, count(*) FROM extract GROUP BY 1, 2"
```

Images and scanned PDFs need OCR (English), which is slow, so it is a
command of its own rather than part of `index`:

```
munchymail ocr              # everything queued, half the CPUs at a time
munchymail ocr -n 200 -j 8  # a bounded run
```

It queues PDFs whose text layer came out empty (`ocrmypdf --force-ocr`,
sidecar text) and image parts of 10 KiB or more (`tesseract`, keeping
words with confidence 40 or more; smaller images are icons and logos),
each once per distinct file, sandboxed as above with 2 GiB and 120 s per
image or 900 s per PDF. It takes the database only to load each chunk of
64 and to record it and update the text index, so other commands can run
in between (they fail with "File is locked" only during those moments);
`ocr` itself waits up to 10 minutes for a lock another process holds. The
whole archive here (86 scanned PDFs, about 1,000 images) took under 2
minutes on 16 processes. Results go in
`extract` like the other tools, so `ocr` again only does what's new.

The index is Tantivy in `data/text/`, one document per message, built by
`index` (and `import`); deleting the directory makes the next `index`
rebuild it. Until a message is indexed `--text` misses it and a warning
says so.

### Messages and copies

A **copy** (plain number, e.g. `227617`) is one exact stored message, byte
for byte. Dovecot rewrites `Status`/`X-Keywords` in the file when mail is
read or flagged, and the next import stores that as a new copy with a new
id; the old copy then reads as gone from the folder.

A **message** (`m` and a number, e.g. `m1234`) is every copy with the same
body, Message-ID and `Date:`, so it keeps its id across those rewrites, a
SpamAssassin `[SPAM]` subject tag, re-quoted addresses, and the same mail in
INBOX and in a backup. `Date:` is part of it because the archive has
different mail sharing body and Message-ID (a reused Exchange id), and mail
without a Message-ID sharing a body (daily reminders). `search --unique`,
`export` and `read mN` use messages; `show` and `changes` work on copies.
Ids of both kinds are never reused within one `data/` directory.

### show: one stored copy

```
munchymail show ID              # raw header block
munchymail show ID --raw        # exact stored bytes
munchymail show ID --where      # every origin/path/offset it was found at, and recorded removals
```

### diff: before/after check of one mbox (no database)

```
munchymail diff BEFORE AFTER [--moved-to MBOX ...] [--by KEY ...] [-n N] [--list]
```

Reads both files (shared lock, read-only), splits them and pairs every
message: **unchanged** (identical bytes), **headers changed** (same body and
Message-ID, e.g. Dovecot rewrote `Status`; shows which headers), **removed,
moved** (found in a `--moved-to` file, matched by body and Message-ID since
the new copy gets new Dovecot headers), **removed, NOT FOUND**, and
**added** (new mail since). Each section is broken down by `--by` keys as in
`report`; `--list` prints every message.

```
$ munchymail diff ~/inbox-before-backfill-2026-10-06.mbox /var/mail/me/INBOX \
    --moved-to /var/mail/me/lists/github-project --by from:addr --by list-id
before   81835  ~/inbox-before-backfill-2026-10-06.mbox
after    62655  /var/mail/me/INBOX
  unchanged          62638
  headers changed       16
  removed, moved     19181
  removed, NOT FOUND     0
  added                  1
  in --moved-to but not from before: 3
...
```

### changes: what changed between import runs

```
munchymail changes --origin mail [--by KEY ...] [-n N] [--list] [--record]
munchymail changes BEFORE AFTER [--rename OLD=NEW ...] ...
```

The `diff` comparison, run on the index without reading any mail.
`--origin` compares that origin's last two runs; otherwise BEFORE and AFTER
are run ids, comma-separated when a snapshot spans origins (e.g. `6,7` for
INBOX and the folders before the move to `/var/mail/me/`). Files
identical on both sides are counted and skipped. In the rest, messages pair
up per path as **unchanged**, **headers changed** (same body and
Message-ID), **moved** (gone from one path and new at another), **removed**
(gone and new nowhere) and **added**. `--rename me=INBOX` compares a
recorded path under another name.

```
$ munchymail changes 1,2 3,4 --by list-id -n 2
before runs 1,2  after runs 3,4
  files unchanged       62  (94351 messages)
  in changed files:
  unchanged          56556
  headers changed       12
  moved              25264
  removed                0
  added                 11

== moved (25264) by path, list-id ==
     7584  me -> lists/github-project  widgets/gear <gear.widgets.github.com>
     5724  me -> lists/github-project  widgets/rocket <rocket.widgets.github.com>
   ...
```

### removals: what left where, and when

Every `import` compares the new run with that origin's previous one and
records each removed or moved message in the `removal` table: the path, the
last run that saw it there and the first that didn't (so the time is known
to within two imports), and where it turned up. `changes --record` does the
same for any pair of snapshots, e.g. across origins. Moves between origins
are only seen that way, since an import compares one origin at a time.

```
munchymail removals [--folder PATTERN] [--moved true|false] [-n N]
```

```
$ munchymail removals --folder Sent -n 1
  25689  2026-10-07..2026-10-10  home-mail:Sent -> Sent-sep-2026  2026-10-04  Me Example ...  Re: test message
```

Messages that arrived and left between two imports are never seen.
Header-only changes (Dovecot rewriting `Status`) are not removals.

### export: a copy for someone to keep

```
munchymail export NAME [filters] [--title TEXT] [-o FILE]
```

```
$ munchymail export corp --participant '*@corp.example' --title 'Mail with corp.example'
wrote data/exports/corp.tgz: 32 messages (5 duplicate copies left out)
```

Writes `data/exports/NAME.tgz`, which unpacks to `NAME/index.md` (a table
of date, From, To, subject and current folder, the command that made it,
and SHA-256 sums) and `NAME/maildir/001`, `002`, ... in date order. Each
file is one whole message (an `.eml`), byte for byte as stored, without the
mbox `From ` line. Bytes come from the index, so import first for the
latest mail. Copies of one message (e.g. one in INBOX, one in an archive,
or Dovecot's `Status` rewrite; see [Messages and copies](#messages-and-copies))
are exported once, using the copy seen most recently. Without `--current`, messages that have since
left the mailbox are included and marked "gone" in the index.

### sql, scan

```
munchymail sql "SELECT name, count(*) FROM header WHERE nth = 0 GROUP BY name ORDER BY 2 DESC LIMIT 10"
munchymail scan --list /var/mail/me/Drafts       # split only, verify byte-exact, no database
```

## mbox-sort: date-sorted copy of an mbox

A separate binary for fixing arrival order after mail surgery. It reads SRC
(shared lock), writes a new OUT (never overwrites), drops only Dovecot's
`X-UID`/`X-IMAPbase` headers and internal first message so Dovecot
renumbers the folder, and verifies OUT before reporting success.

```
target/release/mbox-sort SRC --dry-run
target/release/mbox-sort SRC OUT [--key received|date]
```

Installing OUT over a live folder needs Dovecot stopped and the folder's
index removed; see `docs/mbox-sort.md`.

## Checking surgery from the index

Re-import after moving mail; earlier imports stay as older runs, so the
index can compare where mail was with where it is now:

```
munchymail import mail
munchymail changes --origin mail --by list-id               # moved/removed/added since the last import
munchymail removals --moved false                           # anything found nowhere
munchymail report --folder lists/github-project --current \
  --by 'x-github-reason~^(subscribed|push|review_requested|team_mention)$' \
  --by 'list-id~<[^.]+\.(acme|widgets)\.github\.com>$'      # every message should be match/match
```

## Developing a Sieve rule

1. Who sends the most:
   `munchymail report --origin inbox --since 2026-01-01 --by from:addr`
2. Which header separates what you want:
   `munchymail profile --origin inbox --from '*@github.com' --since 2026-01-01`
3. Size the candidate rule, with the test as a key:
   `munchymail report --origin inbox --since 2026-01-01 --from '*@github.com' --by 'list-id~^(acme|widgets)/' --by x-github-reason`
4. Check one message with Pigeonhole:
   `munchymail show ID --raw > m.eml && sieve-test rules.sieve m.eml`

`show --raw` emits the stored bytes, so `sieve-test` sees what Dovecot
delivered, including Dovecot's own `X-UID`/`Status` headers when the copy
came from a Dovecot mbox. Rust regex syntax is close to, but not the same
as, the POSIX regex Pigeonhole uses; confirm anything elaborate with
`sieve-test`.
