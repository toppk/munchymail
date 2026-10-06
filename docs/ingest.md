# Ingest: reading sources safely

Addendum to `mail-corpus-design.docx`.

## Writes

munchymail writes only under `data/` (gitignored):

```
data/email.sqlite
data/content/aa/aa23…   large immutable objects, by SHA-256
```

Sources are never written: no dotlocks, temp files, renames, or atime updates.

## Reading an mbox (`src/source.rs`)

Matches a Dovecot reader with the default `mbox_read_locks = fcntl`:

1. `open(O_RDONLY|O_NOATIME|O_CLOEXEC)`; drop `O_NOATIME` on `EPERM` (file not ours).
2. Whole-file `fcntl(F_SETLK, F_RDLCK)`, retrying every 100–200 ms (Dovecot's
   jitter) up to a timeout. Dovecot writers (`mbox_write_locks`, here `fcntl`)
   take `F_WRLCK`, so we never read a half-written append or rewrite.
3. If the path's dev/ino no longer match the fd, reopen (as `mbox_file_open_latest`).
4. Read the whole file into memory, `fstat` again, close (releases the lock).
   Any size/mtime/ctime change is reported as `changed_during_read`.

No mmap, so a truncating rewrite can't SIGBUS us. POSIX locks drop when *any*
fd for the file closes in this process, so a file is opened exactly once per read.

Verified with strace: the only syscalls touching the source are the `openat`
and one `fcntl(F_SETLK, F_RDLCK)`; a Python `lockf(LOCK_EX)` holder makes the
reader wait until release.

## Splitting (`src/mbox.rs`)

Follows `istream-raw-mbox.c` / `mbox-from.c`:

- A separator is `\nFrom ` (or `\r\nFrom `) followed by a line `mbox_from_parse`
  accepts; no blank line is required. The preceding newline belongs to the
  separator, not the message. At EOF a final newline after an empty line is
  likewise the separator.
- `Content-Length:` is honoured only when it points exactly at a valid
  separator or at a lone trailing newline at EOF, and only to extend a message
  past a `From ` line in its body.
- Nothing is unescaped: `>From` stays as written. mboxrd/mboxo
  interpretation is a later, derived step.
- Header object = bytes through the empty line ending the headers;
  body = the rest. Header ‖ body is the exact message.
- The file is `preamble ‖ (from_line ‖ header ‖ body ‖ separator)*` with no
  gaps (`Mbox::covers`), so every byte stays attributable.

Known Dovecot divergence: `+HHMM` in `From ` lines is parsed as hours and
minutes; Dovecot reads the last two digits as seconds.

Dovecot-written mboxes carry its own headers (`X-UID`, `Status`, `X-Keywords`,
`Content-Length`, and an `X-IMAP` pseudo-message first). They stay in the raw
header object; copies from other sources will share the body hash but not the
header hash.

## Origins (next)

Many backups will hold the same files, so provenance is tracked in layers:

- **origin**: a backup set or live location you register (label, host, date, notes).
- **file sighting**: a path within an origin plus dev/ino/size/mtime/ctime,
  pointing at a **file content** row keyed by SHA-256 of the whole file.
  Identical files are parsed once; a file whose bytes are a prefix of
  another's records a lineage edge (older snapshot of a growing mbox) and lets
  re-imports read only the appended tail.
- **observation**: an artifact at a byte offset within a file content, with
  its `From ` line and separator.
