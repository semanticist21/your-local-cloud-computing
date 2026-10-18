English | [한국어](ko/security.md)

# Security

YLCC is a pool for trusted people and trusted code. Role tokens, the audit log and worker command policies limit what each machine can do and record who did it; they do not turn the pool into multi-tenant CI. See [What is not protected](#what-is-not-protected).

## Roles

The token file the Hub was started with (`ylcc init`) is the admin **pool** token. Existing single-token pools keep working unchanged. Issue other machines their own tokens with one of three roles:

| Action | admin | requester | worker |
| --- | --- | --- | --- |
| Submit, list, cancel and reprioritize jobs (any job); read logs | yes | yes | no |
| Upload and download blobs | yes | yes | yes |
| `env add`, `prepare`, `status`, `logs`; list workers | yes | yes | no |
| Dashboard | yes, plus Audit log and Tokens pages | yes | no |
| Connect as a worker | any worker ID | no | only its own `--worker-id` |
| Worker delete, pause, resume, limit, update; `env delete` | yes | no | no |
| `ylcc tokens`, `ylcc audit` | yes | no | no |

An unknown or revoked token gets `401`; a role that may not perform the action gets `403`. A worker token cannot pause its own worker. Cancelling or reprioritizing another person's job is allowed for requesters and recorded in the audit log.

## Tokens

Upgrade the Hub first: an older Hub rejects scoped tokens with `401`. `tokens` and `audit` check the Hub's `/health` capabilities and say "upgrade the Hub first" when it lacks them.

```bash
ylcc tokens create --name laptop --role requester --out laptop.token
ylcc tokens create --name linux-build --role worker --worker-id linux-build --out linux-build.token
ylcc tokens list
ylcc tokens revoke TOKEN_ID
```

`create` writes the secret (`ylcc_` plus 64 hex characters) to a new owner-only file, never prints it and never replaces an existing file. The Hub stores only SHA-256 hashes. `--worker-id` is required for worker tokens. The name `pool` is reserved. Revoking disconnects workers using the token; their in-flight requests finish.

### Moving off the shared pool token

Until this is done, every machine holding the pool token is effectively an admin.

1. Upgrade the Hub.
2. Create one worker token per worker with `--worker-id`, and one requester token per laptop or agent.
3. Transfer each file over your secure administration channel; keep it at mode `600`.
4. Restart each worker and requester with its own `--token-file` (or reinstall its service, or re-run `configure`).
5. Replace the pool token so only the admin holds it: stop the Hub, remove its token file, run `ylcc init`, start the Hub.

Rotate a machine's token by creating a new one, switching the machine over, then revoking the old ID.

## Audit log

The Hub records `token.create`/`revoke`, `job.submit`/`cancel`/`priority`, `worker.control`/`update`/`delete`, `environment.upsert` (new revision only)/`delete`/`prepare`, and `worker.connect` (only when the token or version changes) and `worker.rejected`. The last two are limited to 20 per token per hour, followed by one `worker.audit_suppressed`. Reads and `401`/`403` responses are not recorded. The newest 10,000 events are kept.

```bash
ylcc audit --limit 50
```

Each job records `submitted_by`, the token name set by the Hub. The dashboard accepts a pool or requester token and shows the account name and role; jobs show **Submitted by**. Admins also see read-only **Audit log** and **Tokens** pages; manage tokens with the CLI. A worker token gets an explanatory screen instead.

## Worker command policy

A worker can optionally restrict what it runs:

```bash
ylcc worker --id linux-build --data-dir "$HOME/.local/share/ylcc/worker" --policy /etc/ylcc/policy.yaml
```

The same `--policy FILE` works with `service install --role worker`. Without a policy file, the worker runs everything it is assigned, as before.

### Format

```yaml
version: 1
commands:            # argv prefixes; each element exact, "*" any, or "prefix*"
  - [cargo, build]
  - [make, "test-*"]
containers: ["ghcr.io/me/*"]  # exact image, "*" or "prefix*" (end prefixes at / : or @)
environments:                 # id exact/"*"/"prefix*"; sha256 = content digest or "*"
  - {id: rust-1.96.0, sha256: <64 hex>}
env: [RUST_LOG]               # allowed job env keys; PATH, LD_*, DYLD_* only via "*"
self_update: false            # default true
```

- An omitted category is unrestricted; `[]` denies it. An empty category (`env:` with nothing after it), a misspelled key or a `*` anywhere but alone or at the end is a load error.
- **Commands** match argv as exact strings, with no PATH resolution: `cargo` does not match `/usr/bin/cargo`, and on Windows `cargo` does not match `cargo.exe`. A rule matches when it is a prefix of the job's argv. Commands apply to container jobs too.
- Wildcards never match a value containing a `..` path component.
- **Environments** are pinned by a SHA-256 of the environment definition (revision label excluded), computed by the worker from what it actually received. Editing `setup`, `verify`, `env` or `image` breaks the pin. `sha256: "*"` trusts any content for matching IDs, so any requester can then run setup shell commands on the worker. A denial prints the exact `{id, sha256}` to paste. Preset `uses: rust@1.96.0` becomes ID `rust-1.96.0`; preset digests can change with a new ylcc release. An inline environment in a project without `name:` gets an ID that depends on the checkout path.
- `self_update: false` refuses `ylcc workers update`.

### Enforcement

The worker checks each job before preparing environments, pulling images or downloading inputs; a refused job fails with the reason. Environments prepared before the policy existed are re-checked at startup and marked failed if not allowed. The Hub reads the policy each worker advertises, routes jobs only to workers that allow them, and rejects a submission no registered worker allows. A job refused by a worker (for example behind an older Hub) is not rerouted. Requirement probes still run `<tool> --version`.

### Protecting the policy file

Jobs run as the worker account, so the policy must not be writable by it. Keep the file owned by root or an administrator. On Unix the worker refuses a file or directory that is group- or world-writable and warns when the file is owned by its own account. Windows ACLs are not checked.

### Check and suggest

```bash
ylcc policy check /etc/ylcc/policy.yaml
ylcc policy suggest --task build --task test
ylcc policy suggest --environment rust@1.96.0 --os linux
```

`check` parses the file exactly as the worker does and prints each category as unrestricted, deny all or restricted, plus warnings. `suggest` reads `ylcc.yaml` (`--file` for another path) and prints a policy allowing exactly those tasks (all by default) and presets, listing every category explicitly. Path-dependent environment IDs need `--allow-path-scoped`. Re-run it after upgrading ylcc.

### What a policy does not do

An allowlist limits which program starts, not what it does. `cargo build`, `make`, `npm` and `pytest` run code from the inputs (`build.rs`, Makefiles, package scripts, `conftest.py`). Allowing `sh`, `python` or another interpreter allows everything. Job env keys such as `NODE_OPTIONS`, `PYTHONPATH`, `RUSTC_WRAPPER` and `BASH_ENV` can run code. A policy is defence in depth among trusted operators, not a sandbox; use container images for isolation. The worker warns (without refusing) when commands are restricted but `env` is not, environments are not pinned, containers are unrestricted, or a rule starts an interpreter. The dashboard marks such workers **Restricted (bypassable)** instead of **Restricted**.

## What is not protected

- There is no tenant isolation. Requesters see every job, log, output and blob; worker tokens can read any blob by hash; caches are shared between jobs on a worker.
- A requester can run arbitrary code on every worker without a policy, and on any worker whose policy allows a build tool.
- The Hub has no TLS of its own; keep it behind a private HTTPS endpoint.
- Native jobs run with the worker account's permissions; use a dedicated non-admin account ([Services](services.md)).
