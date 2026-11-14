# Keeping mail current

The service (`mmadm serve`) keeps live origins current itself: Gmail and
IMAP every 3 minutes, an IMAP INBOX at once through IDLE, `munchymail sync`
on request, and failure handling. A live mbox tree is imported only by
`sync` or `mmadm import`; serve its mail over IMAP instead and keep the tree
as a snapshot.

## Which origins are synced

Every origin gets a role, stored in the database (schema 10, built):

| Role | Meaning | Synced |
|---|---|---|
| `live` | a mailbox still in use (`mailhost-dovecot`, `gmail`) | yes |
| `snapshot` | a copy of a mailbox as it was (a backup, a found file) | never |
| `export` | a service's export (a Takeout) | never |

An origin with no role is never synced, and neither is a retired one.
`mmadm origin role LABEL ROLE` sets it; the upgrade makes the Gmail origin
live and leaves the rest unset.

## Gmail

- The service polls `history.list` every 3 minutes, which needs only the
  read-only scope (Gmail IMAP IDLE would need full access). When nothing
  changed it only moves its checkpoint and records no import run.
- `munchymail sync` checks every live origin at once and waits, e.g. before
  an agent starts work, and reports what arrived. Its effect is `sync`:
  agents may run it. An origin checked in the last 15 s (Gmail) or 60 s
  (an mbox tree) is skipped, so repeated syncs queue and then do nothing.

## IMAP

An IMAP origin's root is the server's URL; how to log in is in
`config.toml`:

```
mmadm origin add mailhost-imap imaps://imap.example.org --role live
```
```toml
[accounts."me@example.org".origins."mailhost-imap"]
imap = "imaps://imap.example.org"
user = "me"
# Run at each login; the password stays in memory. A failure (e.g. a keyring
# still locked after a reboot) is retried, not a reason to stop the origin.
password_command = ["secret-tool", "lookup", "service", "imap", "user", "me"]
# For mutual TLS, and a server certificate from a private CA:
client_cert = "~/.config/munchymail/creds/imap.cert.pem"
client_key = "~/.config/munchymail/creds/imap.key.pem"
server_cert = "/etc/pki/my-ca.pem"
# Optional: only these folders; a trailing * matches the rest of a name.
# folders = ["INBOX", "lists/*"]
```

- Read-only: folders are opened with EXAMINE and messages fetched with
  BODY.PEEK, so nothing on the server changes, not even \Seen.
- Each check runs STATUS on every folder and opens only the ones whose
  UIDVALIDITY, UIDNEXT or HIGHESTMODSEQ moved. With QRESYNC the server
  reports what was expunged and which flags changed since the last check;
  new UIDs are then fetched in batches of 100.
- The first sync fetches every message once. Measured against Dovecot's
  mbox files: an IMAP copy is the file's message with CRLF line endings
  (IMAP requires them; Dovecot stores LF unless `mail_save_crlf`) and
  without Dovecot's own fields (`Status`, `X-Status`, `X-Keywords`,
  `X-UID`, `Content-Length`, `X-IMAPbase`); each file's "FOLDER INTERNAL
  DATA" message isn't served at all. Copies are stored exactly as received,
  and a message's identity reads CRLF as LF, so both copies are one
  message.
- A folder's progress is saved only after its messages are; an interrupted
  sync redoes the folder and skips what it already stored.
- Like Gmail, a check that finds nothing new records no import run; each
  run's sightings are the folders, so `--folder`, `--current`, `changes`
  and removals work as for mbox.
- The service holds IDLE on the INBOX on a second connection and checks the
  origin about 5 s after any change there. Mail filed into other folders
  (by Sieve, say) is found by the 3-minute check. IMAP NOTIFY would cover
  every folder at once; it needs support in imap-codec and io-imap first.

## Why not watch mbox trees directly

A rescan re-reads every file under Dovecot's shared lock. Measured on a
5 GB tree: 13 s with nothing new, and the lock on a multi-gigabyte INBOX held
for 8–9 s, during which Dovecot can't deliver to it. Reading only what was
appended would shorten the lock but miss Dovecot's in-place header
rewrites. IMAP gives the same changes exactly, through Dovecot's own
locking.

## Failures

| Kind | Example | What the service does |
|---|---|---|
| transient | network down, Google 5xx, quota | retries with backoff, up to an hour apart |
| one origin | token revoked, mailbox path gone | stops syncing that origin and says so in `overview`; `mmadm origin resume LABEL` restarts it |
| systemic | database corrupt or unwritable, disk full | exits with status 78 and stays down |

The unit must then not restart it forever:

```ini
[Unit]
StartLimitIntervalSec=10min
StartLimitBurst=5

[Service]
RestartPreventExitStatus=78
```

## Unit changes

- Only the limits above, in `contrib/munchymail.service`. Gmail's refreshed access tokens stay in memory, so
  the sign-in in `~/.config/munchymail/creds` is only read, and the live
  mbox trees are already readable (`ProtectHome=read-only`).
