English | [한국어](ko/services.md)

# Background services

Install under the account that should run workloads. On shared managed machines, use a dedicated account without administrative rights; keep management credentials out of that account. Its `HOME`, token, executable/support checkout, tool caches and worker data must reside on filesystems that enforce ownership and permissions. Use mode `600` for tokens and `700` for private directories, and keep executable paths unwritable by unrelated accounts.

```bash
ylcc --hub https://HUB.example.ts.net:10000 --token-file "$HOME/.config/ylcc/token" \
  service install --role worker --id linux-build --data-dir "$HOME/.local/share/ylcc/worker"

# On the hub machine, using its own local listen address:
ylcc service install --role hub --listen 127.0.0.1:7443 --data-dir "$HOME/.local/share/ylcc/hub"
```

Linux uses `systemd --user`: inspect `systemctl --user status ylcc-worker` and `journalctl --user -u ylcc-worker`. An administrator must enable linger with `loginctl enable-linger SERVICE_USER` for startup without login. Installing from a shell requires a working user manager/session bus.

macOS uses a login LaunchAgent by default: `launchctl print gui/$(id -u)/org.ylcc.worker`. Its stdout/stderr logs are `$HOME/Library/Logs/ylcc/service-ROLE.log` and `service-ROLE.err.log`. The user must log in and any external data volume must be mounted. For a dedicated headless account, an administrator can use `service install --system-user SERVICE_USER ...` to install a LaunchDaemon running as that user. System-user logs use `/private/var/log/ylcc/SERVICE_USER/`, with root-owned directories and service-user-owned mode-600 regular files. Prepare an existing user-owned data directory and readable protected token first:

```bash
sudo /absolute/path/to/ylcc --hub https://HUB.example.ts.net:10000 \
  --token-file /absolute/service-home/.config/ylcc/token service install \
  --system-user SERVICE_USER --role worker --id mac-build \
  --data-dir /absolute/persistent-volume/ylcc/worker
```

Service installation captures the current executable location and native user's `PATH`; keep the binary there. System-user macOS installs set a user-specific `HOME` and a standard tool path. After changing tool paths, reinstall the service and verify capabilities under the service account.

To restrict what a worker runs, add `--policy /absolute/path/policy.yaml` to `service install --role worker`. The file is validated at install time and the service runs with its absolute path. Keep it owned by root or an administrator and not writable by the service account; with macOS `--system-user`, that account must still be able to read it. See [worker command policy](security.md#worker-command-policy).

Keep launchd stdout/stderr logs on the internal system volume, separate from external job data. macOS can reject opening log paths on removable volumes before starting the executable, producing `posix_spawn: Operation not permitted` or `EX_CONFIG` (78) without a worker log. Check the plist's `StandardOutPath`/`StandardErrorPath` and `launchctl print` when diagnosing this failure; readable external tokens and a valid working directory do not establish that launchd can open those log paths.

An external Mac volume with “Ignore ownership” enabled (`noowners`) cannot enforce this account isolation. A plaintext sparsebundle on that volume does not fix it: other local accounts can read or modify the backing bands regardless of permissions inside the mounted image.

On macOS, an administrator can [add a separate APFS volume](https://support.apple.com/guide/disk-utility/add-delete-or-erase-apfs-volumes-dskua9e6a110/mac) to an existing APFS container, optionally with a quota, and enable ownership on that volume. This permits separate storage without changing a neighboring volume's ownership setting. Use OS tools to prepare and mount the chosen volume, verify that ownership is enforced, then place the service account's files there. Start or restart the worker only after its data volume is available.

macOS [file-access privacy controls](https://support.apple.com/en-nz/guide/security/secddd1d86a6/web) also cover removable volumes. A background LaunchDaemon can be denied access even when the service user owns the directory with mode `700`. An installed CLI, readable token or successful interactive/SSH access does not establish privacy approval for the background service. A denied `worker.lock` create/open is one possible symptom; inspect the worker error and macOS privacy logs before changing permissions.

For a standalone native worker denied removable-volume access, a Mac administrator can open **System Settings → Privacy & Security → Full Disk Access**, click **+**, add the installed `ylcc` executable at the fixed path used by the service plist (for example `/usr/local/bin/ylcc`), and enable it. Restart the worker and verify external-data writes and a real job through that LaunchDaemon. Use the macOS approval UI; leave TCC databases and SIP settings untouched. After updating an unsigned or ad-hoc-signed executable, re-check access in the actual service context.

Remove the user service with `ylcc service uninstall --role worker` (or `hub`). This retains tokens and workload data; remove those explicitly only when no longer needed. System-user LaunchDaemon removal uses `service uninstall --role worker --system-user SERVICE_USER` as an administrator.

## Updating workers

```bash
ylcc workers update --all                      # newest published release, one worker at a time
ylcc workers update mac-build --version v0.4.6
```

Each worker is paused, finishes its running jobs, downloads the release archive for its OS/architecture, checks it against the release's `SHA256SUMS` and the new binary's `--version`, swaps it in place of its own executable and exits with status 75. systemd (`Restart=on-failure`), launchd (`KeepAlive`) and the Windows task runner restart it on the new binary; the command then resumes it (unless it was already paused) and moves to the next worker, stopping at the first failure. The executable's directory must be writable by the worker account, so install the worker binary under that account (for example `~/.local/bin`), not in a root-owned directory. Container workers are skipped: recreate them from the new release image. Update the Hub first with `ylcc update --apply` on its host; `workers` shows each worker's `version`.

## Worker disk cleanup

Workers follow the usual CI/cluster retention rules, without configuration:

- An attempt's checkout, `TMPDIR`/`TMP`/`TEMP` directory and private environment are removed as soon as it finishes and its outputs are uploaded. Its journal and logs stay for 3 days; `unknown` attempts stay until reconciled.
- Input blobs, task caches and environment revisions unused for 7 days are removed, like GitHub Actions cache eviction.
- When the data filesystem is over 85% full, the least recently used of those go first until it is under 80%, as the kubelet does for images. Anything used within the last hour is never evicted, so a disk filled by something else does not make every job rebuild.
- Deleting an environment from the catalogue removes its files from connected workers immediately; offline workers drop them by age.

Collection runs hourly, after each attempt and at startup, and only while the worker is idle. An evicted environment is prepared again by the next job that needs it.
