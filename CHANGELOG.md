# Changelog

Every deployment is a new version: at least a new patch, and a new minor
when the database schema changes. Upgrading to a new schema is one-way;
keep a copy of the data directory to roll back (`docs/install.md`).

## 0.4.1

- The service keeps each IMAP origin's sync connection open between
  checks instead of logging in every few minutes. One idle for 25 minutes
  is replaced (Dovecot logs out at 30), and one found gone is reconnected
  and the sync rerun.
- The INBOX watch sends its own ENABLE, so QRESYNC is listed in its reply.

## 0.4.0: the owner's judgements

- **Damaged copies.** `mmadm copy damaged ID... --reason TEXT` (copy ids, or
  `mN` for every copy of a message) says a copy's bytes are wrong. Like
  Dovecot's placeholder, it is kept as found and readable by id, and left
  out of search, counts, reports and indexes; unlike it, the owner set it,
  and each change is kept with its reason. Its marks stay as the evidence.
  `copy undamaged` takes it back. A message's preferred copy is a mail copy
  while it has one.
- **Repairs.** `mmadm copy link REPAIRED --repaired-from ID...` records which
  copy replaced which; `read` shows it on both sides, and `copy list` lists
  every damaged copy and link.
- **`mmadm status`**: this program's version and the running service's,
  its uptime, whether a write is running, and each live origin's last
  check, retry or stop, and INBOX IDLE watch. Says when the versions differ.
- `stats` counts copies that aren't mail per kind.

Upgrading: the schema moves from 11 to 12 (two new tables; instant). 0.3
can't open the database afterwards.

## 0.3.1

- `content-length` no longer marks a copy whose body matches the field
  once its line endings are read as LF: copies served with CRLF (the Gmail
  API, IMAP) keep the field their mbox wrote. On a real archive this
  removed 56 false marks and kept the 16 real ones.
- `search --mark NAME:PATTERN` matches a mark's detail too, e.g.
  `--mark prefix-of:mid-line` for copies cut short.
- `mmadm verify-files --path PATTERN --out DIR` rebuilds chosen files and
  writes them out (0600, never over a file), so a change to a mailbox can
  be tried on a copy rebuilt from the archive.

Upgrading: no schema change. The first `index`, import or sync re-marks
every copy under the new detector (about a minute for 460,000 copies).

## 0.3.0: damage noted, files proven

munchymail now says what it notices about each copy, and can prove that
the mbox files it read are recoverable from the archive alone.

- **Marks on copies** (`copy_mark`). Observations, never verdicts, and
  never a change to which message a copy is in or which copy is shown:
  - `content-length`: a `Content-Length` field that disagrees with the
    stored body, and whether the body is shorter or longer;
  - `unclosed-boundary`: a multipart body whose outer boundary is never
    closed;
  - `prefix-of`: a copy whose body is a strict prefix of another copy of
    the same Message-ID and Date, and how it ends: mid-line (a sign of
    truncation), where the longer goes on (often a mailing list's footer),
    or short only of newlines.

  `read` lists the marks on every copy of a message; `search --mark NAME`
  finds them. Copies are marked as they are stored, and once for copies
  stored earlier.
- **`mmadm verify-files ORIGIN`** rebuilds each mbox file the origin was
  read from, using only the stored copies and how they were framed
  (re-quoting mboxrd), and checks it against the SHA-256 recorded when it
  was read. Read-only.

On a real archive (460,000 copies; 153 mbox files, 38 GB): marking took
about a minute; every file rebuilt byte for byte. The marks found the known
damaged messages and showed that an older backup holds a complete copy of
one of them.

Upgrading: one new table, no schema version change; 0.2.0 can still open
the database. The first `index`, import or sync marks the existing copies.

## 0.2.0: live syncing

munchymail keeps itself current. Live mailboxes are checked as mail
arrives, and every arrival, move, deletion and flag change is kept as
history.

### Live syncing

- **IMAP origins.** An origin can be an IMAP server (`imaps://…`), with
  mutual TLS and a password from a command such as the desktop keyring.
  Read-only: folders are opened with EXAMINE and messages fetched with
  BODY.PEEK, so not even `\Seen` changes. Each check asks the server only
  what changed since the last one (CONDSTORE/QRESYNC); a check that finds
  nothing takes a fraction of a second, after a restart too.
- **Mail as it lands.** The service holds IDLE on the INBOX and checks
  about 5 seconds after activity there; other folders, and Gmail, are
  checked every 3 minutes.
- **`munchymail sync`** checks every live origin now and reports what
  arrived; agents may run it before a search that needs the latest mail.
  Repeated syncs within 15 s (Gmail, IMAP) or 60 s (mbox) do nothing.
- **Failures are handled by kind.** Network trouble and a locked keyring
  are retried, up to an hour apart. A refused login or revoked sign-in
  stops that origin alone, named in `munchymail overview`, until
  `mmadm origin resume`. A corrupt database or full disk stops the service
  with status 78, which the unit doesn't restart.
- **Origin roles**: `live`, `snapshot`, `export`. Only live origins are
  synced; a Dovecot mailbox moves to IMAP and its mbox tree stays as a
  snapshot.

### History that costs what changed

- **Presence intervals.** In a Gmail label or IMAP folder, each message is
  recorded from the check it appeared in until the check it was gone from.
  A new mail is one row, however big the folder; moves and deletions are
  recorded as removals (`removals`, `read` show where a copy went). Gmail
  listings recorded whole by 0.1 are converted once.
- **Flags and labels over time.** IMAP flags and Gmail labels, read state
  included, are kept as first seen and at every change, not only the latest.

### One message, however it arrived

- **The same email from any source is one message.** A copy from an mbox
  file, over IMAP, from the Gmail API and from a Takeout share one message:
  line endings (CRLF/LF), NUL served as `0x80`, and the final newline an
  mbox adds are read alike. Measured on a 180,000-message mailbox: every
  IMAP copy joined its mbox copy except one the server serves truncated.
- **A stricter key.** Message-ID is parsed as an identifier (no RFC 2047
  decoding, case kept); copies must agree on how the body is to be read
  (transfer encoding, media type, charset, boundary); and copies without a
  valid Message-ID must also share their delivery headers. Unrelated mail
  with the same body no longer merges.
- **Message ids are stable.** Messages are never deleted: a message whose
  copies joined another redirects to it (`read m4` says so), and no id is
  ever reused. Regrouping under a new rule can split as well as merge, in
  one transaction.
- **One preferred copy.** `read`, `show`, `search --unique`, indexing and
  `export` all use a message's earliest stored copy that isn't a lesser
  (folded) one, whichever copy matched.
- **Not mail is classified.** Dovecot's "FOLDER INTERNAL DATA" placeholder
  is kept byte for byte and readable by id, and left out of every listing,
  count, report and index (`artifact.kind`). `read` no longer lists
  Dovecot's bookkeeping fields (`Status`, `X-UID`, …).

### Fixes

- `export` writes the stored bytes by default; `--newlines lf|crlf`
  converts on request, and the export's index says which.
- `read --json` no longer lists the first body line as a header field.

### Upgrading

The schema moves from 10 to 11. The first `index`, `import` or sync
converts Gmail listings to presence and regroups messages under the new key
before storing anything (about 30 s for 280,000 copies). Stop the service,
copy the data directory, install, run `mmadm --account ADDRESS index` for
each account, then start the service.

## 0.1.0

The first installed release: an archive of mbox trees, backups, Google
Takeout and the Gmail API, every copy kept byte for byte with where it was
found; Sieve-style header search, full text including attachments and OCR,
history across imports (`changes`, `removals`), exports, and a service on a
Unix socket for people and agents.
