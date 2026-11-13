# mbox-sort

A separate binary (`target/release/mbox-sort`) that writes a date-sorted copy
of an mbox. It exists because moving mail into an existing folder (e.g. a
backfill into `lists/github-project` after live delivery already started)
appends old messages after new ones, so arrival order is wrong.

```
mbox-sort SRC --dry-run                 # how out of order is it?
mbox-sort SRC OUT [--key received|date] # write the sorted copy
```

- SRC is only read, under the same shared `fcntl` lock as `munchymail`.
- OUT must not exist. It is written to a temp file next to OUT, fsynced,
  hard-linked into place (so an existing file is never replaced), given
  SRC's permission bits, then re-read and verified: it must split into the
  same number of messages with identical header/body bytes apart from the
  dropped UID headers, and be in order.
- `--key received` (default) sorts by the `From ` line time, i.e. IMAP
  INTERNALDATE, what clients call arrival. `--key date` sorts by the `Date:`
  header, falling back to the `From ` line. Ties keep their file order.

## What changes in the messages

Only Dovecot's UID bookkeeping, which is meaningless once the order changes:

- `X-UID` headers are removed (with any continuation lines).
- `X-IMAPbase` (UIDVALIDITY and next UID, in the first real message) is removed.
- Dovecot's internal first message (`X-IMAP`, "DON'T DELETE THIS MESSAGE --
  FOLDER INTERNAL DATA") is dropped; Dovecot recreates it.

Everything else is byte-exact: `From ` lines, `Status`/`X-Status`/
`X-Keywords` (flags and keywords, including Dovecot's padding),
`Content-Length` (it counts body bytes, which are untouched), bodies, and
separators. A message that ended the source without a trailing separator
gets one newline if it is no longer last.

## Why that is enough (from Dovecot CE `src/lib-storage/index/mbox`)

- `mbox-sync-parse.c` `parse_x_uid`: X-UIDs must strictly increase through
  the file; a lower one marks the message `uid_broken`. Re-sorting a file
  with its X-UIDs would trip this.
- `mbox-sync.c` `mbox_sync_loop`: a message with a missing/broken X-UID gets
  `next_uid++`, index records from there on are expunged, and the header is
  rewritten. A missing `X-IMAPbase` makes Dovecot choose a UIDVALIDITY
  (`mbox_sync_parse_next_mail`): the index's if it has one, else the time.
- If an index still exists and a message has no X-UID, Dovecot tries to
  match it to an old record by header MD5 (`mbox_sync_find_index_md5`). With
  a reordered file that can resurrect old UIDs out of order, so **delete the
  folder's index** when installing; then Dovecot starts clean with a new
  UIDVALIDITY and numbers messages in file order.
- `mbox-save.c` writes `From ` lines with `mbox_from_create`, in local time
  with no zone. `--key received` compares those times as written, so
  messages in the hour repeated at a DST change may tie or swap.

## Installing a sorted copy over a Dovecot folder

Not automated on purpose. Mail lives in `/var/mail/me/` (`INBOX` and
folders, `me:mail 0700`). For `lists/github-project`:

```
cd /var/mail/me/lists
doveadm mailbox path -u me -t index lists/github-project   # confirm the index dir below
sudo systemctl stop dovecot          # no IMAP clients, no LMTP delivery (mail queues in postfix)
mbox-sort github-project github-project.sorted
mmadm diff --mbox dovecot github-project github-project.sorted
#   expect: headers changed = every message (x-uid; one also x-imapbase),
#           removed NOT FOUND = 1 (the internal message), added = 0
mv github-project github-project.before-sort
mv github-project.sorted github-project
rm -r .imap/github-project            # that folder's Dovecot index
sudo systemctl start dovecot
```

With Dovecot stopped nothing else writes, so the swap needs no locks. Dovecot
now takes `dotlock fcntl flock` for writes; anything that writes these files
while it runs must take all three.

On first access Dovecot assigns UIDs 1..n in the new order, writes X-UID
headers (a one-time rewrite of the file) and a new UIDVALIDITY; IMAP clients
(Roundcube, K-9) see the new UIDVALIDITY and re-download that folder's
listing. Flags survive because they live in `Status`/`X-Status`/
`X-Keywords`. Keep `.before-sort` until you're satisfied.

mbox-sort copies the mode bits but not ownership: OUT gets your primary
group, while files in `/var/mail/me/` (`INBOX` especially) are group
`mail`. Check with `ls -l` after the move and `chgrp mail` if needed.
