# munchymail

A self-hosted mail archive that keeps every copy of every message, read-only
from its sources, searchable by people and AI agents. It imports mail from
live mailboxes, old backups and Gmail byte for byte, remembers where each copy
was found, and searches the whole history: headers (Sieve-style, handy for
writing filter rules), full text, attachments and what changed between
imports.

Documentation: <https://toppk.github.io/munchymail/>

Sources are only ever read, under the same shared `fcntl` lock a Dovecot
reader takes. Everything munchymail writes lives in its data root,
`~/.local/share/munchymail` (`$XDG_DATA_HOME/munchymail`; `--data` or
`MUNCHYMAIL_DATA` to use another), called `data/` below.

- `docs/tutorial.md`: a walk through setting up, importing, searching and reading
- `docs/install.md`: where it is installed and how it runs as a service
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

Use the release build; debug builds are much slower. The `justfile` has
`build`, `check`, `run` (`just run search --count` = `munchymail search
--count`), `adm` (the same for `mmadm`) and `install`.

The binary has two names. `munchymail` uses the archive (search, read,
study, export); `mmadm`, a link to it, sets it up and keeps it (accounts,
origins, imports, Gmail, the service, `sql`). Each refuses the other's
commands and names the right one. Installed, they are
in `$MUNCHYMAIL_BINDIR` (default `~/.local/bin`); use those absolute paths
in scripts, cron and agents (see `docs/install.md`).

## Getting mail in

Each mail account is separate, with its own directory under `data/`
holding its database, content store and text index (`data/me@example.org/`,
`data/me.example@gmail.com/`). Commands act on one account: `--account
ADDRESS` (or `MUNCHYMAIL_ACCOUNT`) picks it, and may be left out while there
is only one. Aliases are the account's own addresses, as addresses or Sieve
patterns:

```
mmadm account add me@example.org      # creates data/me@example.org/
mmadm alias add '*@example.org' me@old-isp.example
mmadm account list                     # accounts and their aliases
```

Gmail accounts sign in to Google with read-only access to mail and
contacts. Secrets stay out of `data/`: the
OAuth client JSON (a Desktop app client) goes in
`~/.config/munchymail/creds/google-client.json` and each account's refresh
token next to it, all readable only by you:

```
mmadm account add me.example@gmail.com
mmadm --account me.example@gmail.com gmail auth   # prints a link; approve in the browser
mmadm --account me.example@gmail.com gmail status # address, message count, history id, labels
mmadm --account me.example@gmail.com gmail sync   # new mail and label changes, then index
```

`gmail auth` waits for Google's redirect on 127.0.0.1; with the browser on
another machine, paste the address the browser ended on instead. It refuses
a sign-in as a different Google account than `--account`. Keep the Google
Cloud consent screen "In production": in Testing, refresh tokens can expire
after 7 days.

`gmail sync` keeps a Gmail origin (`gmail`) in the account. The first sync
lists every message (Spam and Trash included) and fetches each one's exact
bytes (`format=raw`); later syncs ask Gmail's history for what changed since
the last one, and fall back to a full listing if that history has expired.
Each sync is an import run in which every label is a path (its name:
`INBOX`, `SENT`, `CATEGORY_PROMOTIONS`, your own labels) and mail in neither
Spam nor Trash is also under `All Mail`; `UNREAD` is state, not a place, and
is left out. So `--folder INBOX --origin gmail`, `--current`, `read`'s
locations, `changes --origin gmail` and removals all work as for mbox
folders. Gmail's record (message id, thread, labels, deletion) is kept in
`gmail_message` beside, not instead of, the archive: a message Gmail
deletes stays, marked gone. The checkpoint advances only with a run whose
changes are all stored, and a full listing marks messages gone only once it
is complete, so an interrupted sync just repeats. `import` leaves the Gmail
origin to `gmail sync`.

A Google Takeout of the account is an mboxrd origin marked `source =
"gmail-takeout"` in the config (below). Its copies differ from the API's:
Takeout adds `X-GM-THRID`/`X-Gmail-*` headers, ends every message with CRLF
(Gmail-internal mail has none) and writes U+FFFD over 8-bit text in other
charsets. Each Takeout copy joins its API copy's message when it pairs by
thread id and Message-ID and differs only in those ways; both copies are
kept, the API one is shown and indexed, and `fold` records the reason.
`import` does this as it reads a Takeout, `gmail sync` for each new API
copy, so the order doesn't matter; `mmadm reconcile` redoes it for
everything and lists the copies that stay separate.

Within an account:

```
mmadm origin add mailhost-dovecot /var/mail/me --host mailhost --note "live dovecot tree: INBOX + folders"
mmadm origin list
mmadm import                     # all origins; or: mmadm import mailhost-dovecot
munchymail stats
```

Before importing, say how the origin's files quote body lines starting
`From `, in `~/.config/munchymail/config.toml` (`$XDG_CONFIG_HOME`). A file
doesn't record it, so there is no default and `import` refuses an origin
without one:

```toml
[accounts."me@example.org".origins."mailhost-dovecot"]
mbox = "dovecot"   # as Dovecot reads it: Content-Length honoured, >From kept

[accounts."me.example@gmail.com".origins."2026-11-12-takeout"]
mbox = "mboxrd"    # one > dropped from each >…>From line (Google Takeout)
source = "gmail-takeout"   # a Takeout of this account's Gmail (see above)
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
mmadm origin add mailhost-dovecot /var/mail/me --host mailhost --note "INBOX + folders"
mmadm origin retire inbox          # was /var/mail/me, a single file
mmadm origin retire home-mail      # was ~/mail
mmadm origin retire --undo inbox   # if needed
```

A retired origin is skipped by `import`, refused when named, and ignored by
`--current` unless selected with `--origin`. Folder paths inside a tree
(`lists/github-project`) stay the same across such a move, so `--folder`
matches old and new runs; the old single-file INBOX was recorded as `me`.

When the same files only change place (e.g. a backup filed under
`~/mail-backups/`), keep the origin and repoint it instead:

```
mmadm origin move 2021-07-07-mailhost-spool ~/mail-backups/2021-07-07-mailhost-spool/extracted/me
```

Each origin has a role: `live` (a mailbox in use), `snapshot` (a copy as it
was) or `export` (e.g. a Takeout). Only live origins are kept in sync by the
service (`docs/sync.md`); the Gmail origin is live from the start. A label can
change too, and then the origin's `config.toml` section must be renamed
with it, as `rename` says:

```
mmadm origin role 2021-07-07-mailhost-spool snapshot
mmadm origin rename spool-2021 2021-07-07-mailhost-spool
```

Global options: `--data DIR` (or `MUNCHYMAIL_DATA`, default `~/.local/share/munchymail`; `just run` and
`cargo run` use the checkout's `data/`), the
directory of accounts, and `--account ADDRESS`. Paths below such as
`data/exports/` are within the account's directory.
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

`munchymail describe` (or `mmadm describe`) lists that program's commands with their arguments, defaults and
effect, generated from the same definitions that parse them (`--json` for
one object; `describe search` for one command). The effect is `read` (reads
`data/` only, or for `diff`/`scan` only the files named) or `write` (says
what in `data/` it changes, or the `-o` path for `export`), and leads each
command's line in `--help`. No flag changes a command's effect: recording
removals is its own command, `record-removals`. Errors name the command
that fixes them or lists valid values, e.g. an unknown `--origin` points
at `origin list`, and a database held by another munchymail process says
so.

### serve: one process holding the databases

A database can be open in only one process at a time, so one long command
(an import, `ocr`) otherwise makes every other one wait or fail.
`mmadm serve` opens every account and listens on
`$XDG_RUNTIME_DIR/munchymail/serve.sock` (or `$MUNCHYMAIL_SOCKET`; mode
0600, and only the same user's clients are answered). Installed, it runs as
the systemd user unit `munchymail` (`docs/install.md`). While it runs, the
other commands send their command line there and print what comes back
(output, log lines, exit status), so reads run alongside a write and
writes take turns. With no service running, commands open the database
themselves as before, as they do when the service holds a different data
root or there is no `XDG_RUNTIME_DIR` (cron, ssh without a session).
`diff`, `scan`, `describe` and `gmail auth`/`status` never use it.

```
mmadm serve &                      # until killed; logs to stderr
munchymail search --text invoice -n 5   # answered by the service
mmadm status                       # this program's version and the service's
```

`mmadm status` prints its own version, then asks the service for its
version, process id and uptime, whether a write is running, and per live
origin when it was last checked, whether it is waiting to retry or stopped,
and the INBOX IDLE watch; `--json` for scripts. It says so when no service
is running (exit 1), and when the service is a different version.

The service uses its own `MUNCHYMAIL_LOG`, not the client's. It keeps live
origins current (Gmail and IMAP every 3 minutes, an IMAP INBOX at once through
IDLE; `docs/sync.md`); `munchymail sync` checks them now. The protocol (one JSON request line, then framed
replies) is described at the top of `src/serve.rs`.

### search: list messages

```
munchymail search --origin inbox --since 2026-10-01 -n 20
munchymail search --from '*@isp.example' --subject invoice --count
munchymail search -R 'list-id:^widgets/' --headers -n 3     # raw header blocks
munchymail search --has list-unsubscribe --raw -n 1        # whole raw messages
munchymail search --participant '*@corp.example' --long     # folder, From, To, size, attachments
munchymail search --participant '*@corp.example' --tsv -n 1000 > corp.tsv
munchymail search --participant '*@corp.example' --unique --json -n 20   # for agents
munchymail search --folder SENT --columns date,to,cc,subject              # pick columns
```

Output is `id  date  who  subject`, newest first, fitted to the terminal's
width. `who` is the other side: `→` and the recipients (To and Cc; the
first name and `+N` for the rest) when From is one of the account's
aliases, `→ (self)` for mail to itself, else `←` and the sender. The id is
a stored copy's, what `show` takes; see [Messages and copies](#messages-and-copies).

`--columns` picks the columns: `id`, `message` (mN), `date`, `time`, `who`,
`subject`, `folder`, `size`, `attachments`, `message-id`, `snippet`, or any
header name (`to`, `cc`, `list-id`, ...: its first value; address headers
show names, or addresses with `--addresses`). The default can be set in
`~/.config/munchymail/config.toml`, for all accounts or one:

```toml
[listing]
columns = "id,date,who,subject"

[accounts."me@gmail.com".listing]
columns = "id,date,who,cc,subject"
```

`--tsv` prints the columns tab-separated and untruncated with a header row;
its default is id, message, time, folder, from, to, subject, size,
attachments, message-id and snippet. `--long` adds, per message, the folders it is in now (or "gone; last seen
in PATH DATE"), date and time (UTC), the message id and Message-ID, From,
To, Cc, body size in bytes as stored (MIME-encoded), the number of attachments
(parts with a file name or `Content-Disposition: attachment`), the snippet
(with `--text`, where it matched, matched words in «»), and `problem` if it
didn't decode. Folder is per stored copy: a copy whose headers Dovecot later
rewrote shows as gone, next to its current copy; `--unique` lists each
message once (its newest copy, with the places of all its copies), as
`export` counts them, and `--current` leaves out copies no longer in any
folder.

Without `--account` and with several accounts, `search` searches each in
turn, the account with the newest match first (`default_account` in
`config.toml` breaks ties), so the latest mail from anyone is on page 1. Each
account's rows come under `== ADDRESS (N matches)`, `--tsv` gains an
`account` column, `--json` records an `account` field (after a first
`matches` record), and `--count` prints each account's count. The cursor
names the account it is in and those still to come
(`ADDRESS/DATE:ID/NEXT,...`), so paging runs through one account and on into
the next in the same order, even if new mail arrives meanwhile. Ids are the account's own: read one with
`--account`. An `--origin` that only one account has just matches nothing
in the others.

Results come in pages of `-n` (1 to 1000, default 50), newest first by
`Date:` with ties broken by id, so pages are stable. When there are more,
the next cursor is logged to stderr after the page (`more results: add
--after CURSOR`); `--json` carries it in its `page` record instead.
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
   2537  13.7%  45.8%  match                      acme/rocket <rocket.acme.github.com>
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
munchymail read m1234 --text > m1234.txt   # just the headers and text, to file; no untrusted wrapper
munchymail attachment m1234 2 > file.pdf   # attachment 2's decoded bytes; refuses a terminal
munchymail show m1234 --raw    # show takes a message id too: the copy read shows
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
mmadm sql "SELECT tool, outcome, count(*) FROM extract GROUP BY 1, 2"
```

Images and scanned PDFs need OCR (English), which is slow, so it is a
command of its own rather than part of `index`:

```
mmadm ocr              # everything queued, half the CPUs at a time
mmadm ocr -n 200 -j 8  # a bounded run
```

It queues PDFs whose text layer came out empty (`ocrmypdf --force-ocr`,
sidecar text) and image parts of 10 KiB or more (`tesseract`, keeping
words with confidence 40 or more; smaller images are icons and logos),
each once per distinct file, sandboxed as above with 2 GiB and 120 s per
image or 900 s per PDF. It takes the database only to load each chunk of
64 and to record it and update the text index, so other commands can run
in between (they fail with "File is locked" only during those moments;
under `serve` they just run);
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
mmadm diff BEFORE AFTER --mbox VARIANT [--moved-to MBOX ...] [--by KEY ...] [-n N] [--list]
```

`--mbox` (`dovecot` or `mboxrd`, as in the config above) is required here
and for `scan`.

Reads both files (shared lock, read-only), splits them and pairs every
message: **unchanged** (identical bytes), **headers changed** (same body and
Message-ID, e.g. Dovecot rewrote `Status`; shows which headers), **removed,
moved** (found in a `--moved-to` file, matched by body and Message-ID since
the new copy gets new Dovecot headers), **removed, NOT FOUND**, and
**added** (new mail since). Each section is broken down by `--by` keys as in
`report`; `--list` prints every message.

```
$ mmadm diff ~/inbox-before-backfill-2026-10-06.mbox /var/mail/me/INBOX --mbox dovecot \
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
munchymail changes --origin LABEL [--by KEY ...] [-n N] [--list]
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
     7584  me -> lists/github-project  owner/repo-a <repo-a.owner.github.com>
     5724  me -> lists/github-project  owner/repo-b <repo-b.owner.github.com>
   ...
```

### removals: what left where, and when

Every `import` compares the new run with that origin's previous one and
records each removed or moved message in the `removal` table: the path, the
last run that saw it there and the first that didn't (so the time is known
to within two imports), and where it turned up. `record-removals` (taking
the same runs as `changes`) does the same for any pair of snapshots, e.g. across origins. Moves between origins
are only seen that way, since an import compares one origin at a time.

```
munchymail removals [--folder PATTERN] [--moved true|false] [-n N]
```

### marks: what munchymail noticed about a copy

Each copy is checked as it is stored: a `Content-Length` its body doesn't
match (`content-length`), a multipart body never closed
(`unclosed-boundary`), or a body that is the start of another copy of the
same message (`prefix-of`, saying whether it stops mid-line). `read` lists
them; `search --mark NAME` (or `--mark '*'`) finds them, and
`--mark NAME:PATTERN` matches the detail too, e.g. `--mark prefix-of:mid-line`.

### copy: what the owner says about stored copies

```
mmadm copy damaged ID... --reason TEXT
mmadm copy undamaged ID... --reason TEXT
mmadm copy link REPAIRED --repaired-from ID... [--note TEXT] [--remove]
mmadm copy list [--json]
```

IDs are copy ids, or `mN` for every copy of a message. A damaged copy is
kept as found and readable by id, but left out of search, counts, reports
and indexes, like a copy that isn't mail; its marks stay, and every change
is kept with its reason. `link` records that a copy was built to replace
damaged ones; `read` shows it on both sides.

### verify-files: rebuild an origin's mbox files

```
mmadm verify-files ORIGIN [--path PATTERN] [--out DIR]
```

Rebuilds every mbox file the origin was read from, using only the stored
copies, their `From ` lines and separators (re-quoted for mboxrd), and
checks each against the SHA-256 recorded at import. The source files are
not read. `--path` picks files (e.g. `INBOX`); `--out` also writes the
rebuilt files into a directory, for trying something on a copy of a
mailbox without touching the mailbox.

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

Writes `data/exports/NAME.tgz`, or `-o FILE` (relative to where you ran
it; under the service the CLI writes it, kept only if the export succeeds),
which unpacks to `NAME/index.md` (a table
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
mmadm sql "SELECT name, count(*) FROM header WHERE nth = 0 GROUP BY name ORDER BY 2 DESC LIMIT 10"
mmadm scan --mbox dovecot --list /var/mail/me/Drafts   # split only, verify byte-exact, no database
```

`sql` takes one query (`SELECT`, `WITH`, `VALUES` or `EXPLAIN`) and runs it
in a transaction that is rolled back, so it changes nothing; columns of a
table: `SELECT * FROM pragma_table_info('artifact')`.

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
mmadm import mailhost-dovecot
munchymail changes --origin mailhost-dovecot --by list-id               # moved/removed/added since the last import
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

## License

MIT; see `LICENSE`.
