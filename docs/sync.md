# Keeping mail current

The service (`mmadm serve`) keeps live origins current itself. Built: Gmail
every 3 minutes, `munchymail sync`, failure handling. Not yet: watching mbox
trees; until then they are imported by `sync` or `mmadm import`.

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
  agents may run it.

## mbox trees (not built)

A rescan re-reads every file under Dovecot's shared lock. Measured on a
5 GB tree: 13 s with nothing new, and the lock on a multi-gigabyte INBOX held
for 8–9 s, during which Dovecot can't deliver to it. Too slow to run on
every change, so live mbox origins aren't on the timer. Two ways forward:

- **Tail reads:** inotify with a 15 s settle (2 minutes at most); a file
  that only grew is read from where it ended, under a short lock; unchanged
  files aren't opened. These partial scans never record removals; a nightly
  full scan catches Dovecot's in-place header rewrites and does.
- **IMAP as the live source**, as the Gmail API is for Gmail: NOTIFY or IDLE
  says when mail arrives, and only new messages are fetched. The mbox tree
  then becomes a periodic snapshot, as a Takeout is for Gmail.

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
