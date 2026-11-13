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

Hardened like the other user units, minus `RestrictNamespaces` (OCR and
document extraction run tools in bwrap, which makes namespaces) and
`MemoryDenyWriteExecute` (untested with tesseract and ocrmypdf). It may
read anything and writes only the data root. If `serve` ever refreshes Gmail
tokens itself, `~/.config/munchymail/creds` joins `ReadWritePaths`.

## Moving the data from the checkout (once, when the owner asks)

After the home filesystem's snapshots have run at least once:

1. `just install`; run one OCR job and one document extraction through the
   service to check bwrap works inside the unit.
2. Nothing holds the databases: `fuser -v ~/src/munchymail/data/*/email.sqlite`.
3. `rm -f ~/src/munchymail/data/serve.sock`
4. `mv ~/src/munchymail/data ~/.local/share/munchymail` (same btrfs
   filesystem: a rename).
5. `munchymail account list` and `munchymail --account me.example@gmail.com
   stats` match the counts from before (19,516 messages).
6. `systemctl --user enable --now munchymail`; `munchymail search -n 3` is
   then answered by it (`journalctl --user -u munchymail`).
7. Optionally `ln -s ~/.local/share/munchymail ~/src/munchymail/data`.

Roll back: stop the unit, `mv` the directory back, run with `--data data`.
Development runs from the checkout use the live data by default; point
`MUNCHYMAIL_DATA` at a scratch copy for anything experimental.
