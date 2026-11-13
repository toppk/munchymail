# Install

As agreed with the machine's admin.

| What | Where |
|---|---|
| Releases | `~/.local/lib/munchymail/releases/<version>-<shortsha>/{munchymail,mbox-sort}` |
| Current, previous | `~/.local/lib/munchymail/{current,previous} -> releases/…` (relative symlinks) |
| CLI | `~/.local/bin/{munchymail,mbox-sort} -> ~/.local/lib/munchymail/current/…` |
| Config, credentials | `~/.config/munchymail/{config.toml,creds/}` |
| Data root | `~/.local/share/munchymail/` (`--data`, `MUNCHYMAIL_DATA`) |
| Socket | `$XDG_RUNTIME_DIR/munchymail/serve.sock` (`MUNCHYMAIL_SOCKET`) |
| Unit | `~/.config/systemd/user/munchymail.service`, from `contrib/` |

The CLI is a symlink so it is always the same build as the service it talks
to. `~/.local/bin` is on `PATH` only in login shells and the systemd user
manager; cron, `ssh host cmd` and sandboxes should call
`~/.local/bin/munchymail` by its full path.

## Installing a release

```
just install                      # needs a clean tree: releases are named by commit
systemctl --user daemon-reload
systemctl --user restart munchymail
```

`just install` copies the binaries into a new release, moves `previous` to
the old `current`, points `current` at the new one, creates the `~/.local/bin`
links if missing and copies the unit. It never enables or restarts the unit
and never deletes old releases. Roll back by pointing `current` at
`previous`'s target and restarting.

The service stops at once on SIGTERM; writes are transactional, so an import
or OCR run loses only the file or chunk in progress. Restart it when idle.

## The unit

Hardened like the other user units, minus what bwrap (which runs the OCR
and document tools) can't work under: `RestrictNamespaces` (it makes
namespaces), `ProtectKernelTunables` and `ProtectKernelLogs` (they overmount
parts of `/proc`, so the kernel refuses bwrap's fresh `/proc`), and with
`AF_NETLINK` allowed (it sets up loopback in its network namespace).
`MemoryDenyWriteExecute` is left out as untested with tesseract and
ocrmypdf. Before each OCR or extraction run munchymail starts the sandbox
once with `/usr/bin/true`; if that fails the run stops with an error and
records nothing, so a broken sandbox never marks attachments as failed. It may
read anything and writes only the data root. If `serve` ever refreshes Gmail
tokens itself, `~/.config/munchymail/creds` joins `ReadWritePaths`.

## Moving the data from the checkout (once, when the owner asks)

After the home filesystem's snapshots have run at least once. Start no `serve`
(and no unit) before step 4: the unit's `ReadWritePaths` and `serve` itself
would create an empty `~/.local/share/munchymail` first.

1. `just install` (installs the unit, doesn't start it).
2. Nothing holds the databases: `systemctl --user is-active munchymail` says
   `inactive`, and `fuser -v ~/src/munchymail/data/*/email.sqlite`
   lists nothing.
3. `rm -f ~/src/munchymail/data/serve.sock`
4. `mv -T ~/src/munchymail/data ~/.local/share/munchymail` (same btrfs
   filesystem: a rename; `-T` never moves it *into* an existing directory,
   and fails if one with contents is there).
5. `munchymail account list` and `munchymail --account me.example@gmail.com
   stats` match the counts from before (19,516 messages).
6. Check bwrap inside the unit's sandbox, with no munchymail or data
   (prints `bwrap-ok`; checked 2026-11-12):

   ```
   systemd-run --user --wait --pipe --collect -q \
     -p NoNewPrivileges=yes -p PrivateTmp=yes -p ProtectSystem=strict \
     -p ProtectHome=read-only -p ProtectKernelModules=yes -p ProtectControlGroups=yes \
     -p "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK" \
     -p LockPersonality=yes -p SystemCallArchitectures=native \
     bwrap --ro-bind /usr /usr --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
       --symlink usr/bin /bin --symlink usr/sbin /sbin --proc /proc --dev /dev \
       --tmpfs /tmp --unshare-all --die-with-parent --new-session --clearenv \
       --setenv PATH /usr/bin /usr/bin/true && echo bwrap-ok
   ```
7. `systemctl --user daemon-reload && systemctl --user enable --now
   munchymail`; `munchymail search -n 3` is then answered by it
   (`journalctl --user -u munchymail`).
8. At the first OCR or extraction run (the first import or sync bringing
   a PDF or image), `journalctl --user -u munchymail` shows it finished;
   a sandbox failure would stop it with an error, recording nothing.

Done 2026-11-13 (`0.1.0-dbd86bb`); step 8 waits for the first such run.
Roll back: stop the unit, `mv -T` the directory back.

## Development

`just run` and `cargo run` use the checkout's own `data/`, never the live
data root: a dev build may change the schema, and the live databases are the
service's. That data root differs from the service's, so those commands run
directly rather than through it.
