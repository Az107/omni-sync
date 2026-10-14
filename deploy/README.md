# Installing the omni-sync client daemon as a service

`omni-sync daemon install` writes a per-user service definition pointing at
**this binary** and the resolved config, then hands it to the platform's
service manager. It has to be the installed binary, not a `target/` build:

```sh
omni-sync daemon install            # write, enable, start
omni-sync daemon install --dry-run  # print the unit/plist and change nothing
omni-sync daemon status             # installed? enabled? running?
omni-sync daemon restart            # pick up a new binary
omni-sync daemon uninstall          # stop and remove it
```

Common flags: `--no-start` (enable but do not start now), `--force`
(overwrite a service file omni-sync did not write), and `--system` (Linux
only; see below).

Everything is resolved to **absolute paths** first — service managers never
expand `~` — and every generated file carries a `Managed by omni-sync`
marker, so re-running `install` overwrites our own file and only asks for
`--force` when it finds someone else's.

## Linux — systemd

- Default is a **user** unit at `~/.config/systemd/user/omni-sync.service`
  (honours `XDG_CONFIG_HOME`). `install` runs `systemctl --user
  daemon-reload` and `systemctl --user enable --now omni-sync`.
- A user unit only starts at boot, and keeps running after logout, if
  **lingering** is enabled. `install` detects this and prints the fix:
  `loginctl enable-linger "$USER"`. `uninstall` deliberately leaves linger
  alone.
- `--system` writes `/etc/systemd/system/omni-sync.service` (needs root)
  with `User=` / `HOME=` set to the invoking user. It runs as that user, not
  root, so the `0600` `<stem>.daemon.ctl` stays user-owned and
  `omni-sync flush` can still reach the daemon.
- Logs go to the journal: `journalctl --user -u omni-sync -f`.

`deploy/systemd/omni-sync.service` is a hand-written example; the generator
writes absolute paths rather than `%h`.

## macOS — launchd

- A LaunchAgent at `~/Library/LaunchAgents/dev.albruiz.omni-sync.plist`,
  with `RunAtLoad`, `KeepAlive`/`SuccessfulExit=false`, and stdout/stderr
  redirected to `~/Library/Logs/omni-sync.log`.
- `install` runs `launchctl bootout gui/$UID/<label>` (ignoring its
  absence) then `launchctl bootstrap gui/$UID <plist>`.
- `deploy/launchd/dev.albruiz.omni-sync.plist` is a hand-written example.

## Windows — Task Scheduler

- A per-user task named `omni-sync`, triggered at logon, run with
  `LeastPrivilege` and `RestartOnFailure`. It is a task rather than an SCM
  service on purpose: a service runs in session 0 as the service account
  with no user profile, so it cannot see `%APPDATA%`'s config, mapped
  drives, or the user's folders.
- `install` writes the UTF-16 task XML to a temp file and runs `schtasks
  /Create /TN omni-sync /XML … /F`, then `schtasks /Run`.
- `status` is `schtasks /Query /TN omni-sync`; `uninstall` is `schtasks
  /Delete /TN omni-sync /F`.

## Notes

- Stops are graceful: the daemon handles SIGTERM like Ctrl-C, so a service
  stop removes `<stem>.daemon.ctl` and aborts the per-folder engines instead
  of killing them mid-write.
- `--system` is rejected on macOS and Windows (it is a systemd concept).
- After `omni-sync update` replaces the binary, restart the service to pick
  it up. `omni-sync update` offers to do this on a terminal, or takes
  `--restart-daemon` / `--no-restart`; `omni-sync daemon restart` does it on
  demand. Only a *running* service is restarted — one you stopped stays
  stopped and uses the new binary when next started.
