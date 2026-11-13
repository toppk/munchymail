# Keeping mail current (planned)

Not built yet. Today mail arrives only when someone runs `mmadm import` or
`mmadm gmail sync`, so `munchymail overview` can be hours or days behind.
This is the agreed design for the service (`mmadm serve`) to do that work
itself.

## Which origins are synced

Every origin gets a role, stored in the database (schema 7):

| Role | Meaning | Synced |
|---|---|---|
| `live` | a mailbox still in use (`mailhost-dovecot`, `gmail`) | yes |
| `snapshot` | a copy of a mailbox as it was (a backup, a found file) | never |
| `export` | a service's export (a Takeout) | never |

An origin with no role is never synced, and neither is a retired one.
`mmadm origin role LABEL ROLE` sets it.

## Gmail

- Poll `history.list` every 2–5 minutes; it is cheap when nothing changed,
  and needs only the read-only scope (Gmail IMAP IDLE would need full
  access).
- `munchymail sync` asks the service to poll now, e.g. before an agent
  starts work; it waits for the run and reports what arrived.

## mbox trees

- inotify on each live origin's files. A change starts a scan once the
  files have been quiet for 15 seconds, or 2 minutes after the first change
  at the latest, so a burst of ten new messages is one scan.
- These scans read only the changed files and never close a presence
  interval: a message counts as gone only after a complete scan.
- A full scan of every live origin runs nightly, and is what closes
  intervals.

## Failures

| Kind | Example | What the service does |
|---|---|---|
| transient | network down, Google 5xx, quota | retries with backoff, up to an hour apart |
| one origin | token revoked, mailbox path gone | stops syncing that origin and says so in `overview`; `mmadm sync resume LABEL` restarts it |
| systemic | database corrupt or unwritable, disk full | exits with status 78 and stays down |

The unit must then not restart it forever:

```ini
[Unit]
StartLimitIntervalSec=10min
StartLimitBurst=5

[Service]
RestartPreventExitStatus=78
```

## Unit changes this needs

- Only the limits above. Gmail's refreshed access tokens stay in memory, so
  the sign-in in `~/.config/munchymail/creds` is only read, and the live
  mbox trees are already readable (`ProtectHome=read-only`).
