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

Global options: `--data DIR` (or `MUNCHYMAIL_DATA`, default `data`).
Logs go to stderr; `MUNCHYMAIL_LOG=munchymail=debug` for more. Each file
read logs how long its lock was waited for and held:

```
INFO munchymail::source: read under fcntl read lock path=/var/mail/me/INBOX
     bytes=3330473224 lock_wait=4.598µs lock_held=888.296303ms ...
```

## Filters

`search`, `report`, `profile`, `top`, `names` and `export` share these; all
given must match:

| Option | Matches |
|---|---|
| `-H, --header NAME:PATTERN` | decoded header value, like Sieve `header` (repeatable) |
| `-R, --regex NAME:REGEX` | decoded header value, case-insensitive regex, like Sieve `header :regex` (repeatable) |
| `--from PATTERN`, `--to PATTERN` | any address in From/To, like Sieve `address :all` |
| `--participant PATTERN` | any address in From, Sender, Reply-To, To, Cc or Bcc: mail sent or received by someone (repeatable) |
| `--subject PATTERN` | same as `-H subject:PATTERN` |
| `--has NAME` | header present (repeatable) |
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

There is no negation yet (see TODO).

## Commands

### search: list messages

```
munchymail search --origin inbox --since 2026-10-01 -n 20
munchymail search --from '*@isp.example' --subject invoice --count
munchymail search -R 'list-id:^widgets/' --headers -n 3     # raw header blocks
munchymail search --has list-unsubscribe --raw -n 1        # whole raw messages
```

Output is `id  date  from  subject`, newest first. The id is what `show`
takes.

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

### show: one message

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
latest mail. Copies with the same body and Message-ID (e.g. one in INBOX,
one in an archive, or Dovecot's `Status` rewrite) are exported once, using
the copy seen most recently. Without `--current`, messages that have since
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
