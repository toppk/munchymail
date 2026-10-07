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
munchymail origin add home-mail ~/mail --host mailhost --note "live dovecot tree"
munchymail origin add inbox /var/mail/me --host mailhost --note "live INBOX"
munchymail origin list
munchymail import                     # all origins; or: munchymail import inbox
munchymail stats
```

An origin is a directory tree (walked; dot-entries like Dovecot's `.imap`
are skipped) or a single mbox file. Files that don't start with a valid
`From ` line are skipped. Import is idempotent: unchanged files are
recognised by hash and only get a new sighting, so re-running is cheap.

Global options: `--data DIR` (or `MUNCHYMAIL_DATA`, default `data`).
Logs go to stderr; `MUNCHYMAIL_LOG=munchymail=debug` for more. Each file
read logs how long its lock was waited for and held:

```
INFO munchymail::source: read under fcntl read lock path=/var/spool/mail/me
     bytes=3330473224 lock_wait=4.598µs lock_held=888.296303ms ...
```

## Filters

`search`, `report`, `profile`, `top` and `names` share these; all given must
match:

| Option | Matches |
|---|---|
| `-H, --header NAME:PATTERN` | decoded header value, like Sieve `header` (repeatable) |
| `-R, --regex NAME:REGEX` | decoded header value, case-insensitive regex, like Sieve `header :regex` (repeatable) |
| `--from PATTERN`, `--to PATTERN` | any address in From/To, like Sieve `address :all` |
| `--subject PATTERN` | same as `-H subject:PATTERN` |
| `--has NAME` | header present (repeatable) |
| `--origin LABEL` | found in that origin |
| `--folder PATTERN` | found at a matching path inside an origin, ever, e.g. `'lists/*'` |
| `--current` | restrict `--origin`/`--folder` to each origin's latest import (where mail is now), not any past sighting |
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
munchymail show ID --where      # every origin/path/offset it was found at
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
$ munchymail diff ~/inbox-before-backfill-2026-10-06.mbox /var/mail/me \
    --moved-to ~/mail/lists/github-project --by from:addr --by list-id
before   81835  ~/inbox-before-backfill-2026-10-06.mbox
after    62655  /var/mail/me
  unchanged          62638
  headers changed       16
  removed, moved     19181
  removed, NOT FOUND     0
  added                  1
  in --moved-to but not from before: 3
...
```

### sql, scan

```
munchymail sql "SELECT name, count(*) FROM header WHERE nth = 0 GROUP BY name ORDER BY 2 DESC LIMIT 10"
munchymail scan --list ~/mail/Drafts       # split only, verify byte-exact, no database
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
munchymail import inbox home-mail
munchymail search --origin inbox --current --count          # what's in INBOX now
munchymail report --folder lists/github-project --current \
  --by 'x-github-reason~^(subscribed|push|review_requested|team_mention)$' \
  --by 'list-id~<[^.]+\.(acme|widgets)\.github\.com>$'      # every message should be match/match
```

For a before/after count of individual messages across snapshots, see the
SQL in `docs/index.md`.

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
