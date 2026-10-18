---
name: ylcc
description: Offload builds, tests, rendering, GPU computation, and other expensive commands to the user's YLCC worker pool, or run work that requires a different operating system or CPU architecture.
---

Use the installed `ylcc` CLI to execute work on the configured private pool. Read `ylcc --help` and the relevant subcommand help when an option is uncertain. The CLI writes JSON to stdout and progress to stderr; `--json` makes machine-readable intent explicit. Hub/token defaults come from `ylcc configure` or `YLCC_HUB_URL` / `YLCC_TOKEN_FILE`. Never print token contents or include them in commands, source snapshots, or outputs.

Preflight before submitting:
- Verify the target worker's actual OS/arch/toolchain/GPU from `ylcc --json workers`; do not infer them from the requester.
- Inspect the project's `ylcc.yaml` first when it is reviewed; otherwise use explicit `--input`.
- For build/test commands, use `--no-daemon` for Gradle-like tools and declare every needed `--output`.
- Check reusable environments with `ylcc --json env status`; prepare explicitly if needed.
- Prefer remote execution for expensive or platform-specific work; use the requester only for lightweight checks.

1. Run `ylcc --json workers` and inspect each worker's actual capabilities. Choose the execution OS, architecture, toolchain and GPU backend the task requires. Distinguish execution platform from the produced binary's build target. A Docker worker on a Mac executes Linux; CUDA and Metal are different backends. Do not infer that requesting a GPU rewrites a CPU program to use it.
2. If the project has a reviewed `ylcc.yaml`, inspect it with `ylcc task list` and use `ylcc task run NAME --detach` for a matching task. A task with `targets` submits one job per target and prints an array of jobs; wait for each ID. Commands and inputs are relative to that YAML file. Otherwise select the necessary source files and folders with repeated `--input`. Use `ylcc --json manifest --input PATH` to inspect included files and exclusion reasons. Folder/project selection honors Git ignore rules, including initialized submodule working trees. An explicitly selected ignored file is included, and so are files matched by a `*` in the last component (`dir/*.dylib`); check they contain no unintended secrets. Use `--project` only when the whole project is needed; use `--no-input` when no local files are needed. Inputs are fixed at submission, so submit again after changing source.
- For dashboard access, run `ylcc dashboard` (and optionally `--open`) to get the Hub URL and configured token-file path. It never prints the token; paste it manually from that path.

3. Reuse an appropriate registered environment: `ylcc --json env list` and `ylcc --json env status`. A versioned environment can prepare its user-space tools automatically when the job first uses it. To prepare explicitly, use `ylcc env prepare ENVIRONMENT_ID --worker WORKER_ID`. If setup fails, read the error, correct the recipe or requirements, and prepare again. Remove a test or unused environment with `ylcc env delete ENVIRONMENT_ID`; it is idempotent and refuses while queued or active jobs use it. Host drivers, Xcode, credentials, and administrative changes need separate setup; installed CLI presence does not prove API authentication.
4. Submit with explicit outputs and platform requirements. Example:

   ```bash
   ylcc --json run --input Cargo.toml --input Cargo.lock --input src \
     --os linux --arch x86_64 --environment rust-linux \
     --output target/release/my-tool --timeout 1800 --detach \
     -- cargo build --locked --release
   ```

   Use the project's real command and output paths. The returned `.id` identifies the job. Busy workers queue work; there is no automatic local fallback. Continue independent work while it runs.
5. Use `ylcc --json jobs show JOB_ID` for status and `ylcc --json jobs logs JOB_ID` for logs (`jobs logs JOB_ID --follow` streams them; only stdout/stderr is captured, so print progress instead of writing it only to a file). `ylcc --json jobs wait JOB_ID` waits for completion and automatically fetches declared outputs into the configured results directory under `JOB_ID`. `jobs fetch JOB_ID` explicitly fetches or reports that directory. `--no-download` skips automatic downloads. Apply needed files to the project deliberately; the CLI does not overwrite project sources.
- Make repeated work a task. When the user will rerun a command (tests, builds, per-app checks in a monorepo), add `--cache` for rebuildable dependency or build directories (`target`, `node_modules`) and `--save-task NAME` to the first ad-hoc `run`. After that, use `ylcc task run NAME`. For a monorepo, add one task per app, or edit `ylcc.yaml` directly: `defaults` for shared requirements, environment and cache; `targets` for the same task on several OS/arch; `environments` with `uses: NAME@VERSION` (`ylcc env presets`; Linux, macOS and Windows; needs an explicit `os`) or a `setup`/`verify` recipe so the toolchain installs on any worker without manual `env add`. A preset can also be used directly as `--environment bun@1.3.14`. Show the user the task you saved; a project file defines executable commands. Remove a task with `ylcc task delete NAME` (idempotent; refuses a removal that would leave the file invalid); to change one, edit it or delete and save it again.
- For repeated work, use `ylcc jobs retry JOB_ID` to resubmit the recorded spec; it produces a new job ID and leaves the original attempt auditable.
- For retention, run `ylcc cleanup --older-than-days N` first; it is a dry run by default, and only `--apply` deletes candidate result directories.

Agent-native behavior:
- Commands are designed for machine parsing: stdout is structured JSON, progress/errors go to stderr, and nonzero exit status indicates failure.
- Prefer `--json` when invoking through tools, and use returned IDs, URLs, hashes, and paths rather than scraping prose.
- Never request or echo the pool token; use `ylcc dashboard` only for the token-file location.
- Keep submissions explicit, retry manually with `ylcc jobs retry`, and use `--detach`/`jobs wait` for long work.

6. Before upgrading an existing pool, use `ylcc update --latest` to resolve the newest published tag, or `ylcc update --version VERSION`; add `--apply` to execute the verified installer instead of printing it, and `--force` only after draining is complete or impossible to verify. Without `--version`, the current package version is used. The upgrade keeps saved Hub URL/token-file/results directory references, worker data, hub data, and service IDs unless you reinstall. Drain jobs and stop the hub service before replacing its binary, then verify with `ylcc --version`. For a Windows worker that must come back after a reboot without a login, install the service with `--at-startup` from an elevated shell. Upgrade background workers remotely with `ylcc workers update --all` (or named IDs, optional `--version vX.Y.Z`): it pauses each worker, waits for its jobs, swaps the verified release binary, waits for the reconnect and resumes it; container workers are skipped and need their image recreated. Workers free finished checkouts immediately and unused caches/environments after 7 days or under disk pressure, so a missing cache or environment is rebuilt, not an error.

7. Report the worker/platform, exit status, verification performed, and result path. If a worker disconnects or an attempt becomes `unknown`, inspect the original attempt before resubmitting; a missing connection does not prove the command stopped. Use `ylcc jobs cancel JOB_ID` to cancel. `workers pause ID` stops new work while active jobs finish; `workers resume ID` resumes scheduling. `workers limit ID --max-jobs N` adjusts concurrency within the startup capacity, and `jobs priority ID N` changes queued-job priority (-100 to 100). Do not alter another operator's controls without authorization, silently duplicate work, or run a heavy fallback on the requester. CPU/memory hard caps belong to the runtime/OS; concurrency is not a CPU percentage limit.

## Prompt examples

Use these when a user gives a natural-language request:
- health: "Check the pool and workers, but don't run anything." -> `ylcc --json status`, then `ylcc --json workers`.
- platform: "Run this only on a powered Windows x86_64 worker." -> filter `workers` by `online`, `os`, `arch`, then submit.
- build: "Build this on Linux x86_64 and bring back the binary." -> inspect project inputs with `manifest`, then `run --input ... --output ... --detach`, then `jobs wait ID`.
- repeat: "Use the reviewed project task." -> `task list`, then `task run NAME --detach`.
- set up: "Make this test repeatable." -> a successful `run ... --cache target --save-task test`, then `task run test` from then on.
- environment: "Use the prepared Rust toolchain." -> `env status`, prepare it explicitly if required.
- queue control: "Make this urgent." -> a queued job only: `jobs priority ID N`.
- dashboard: "Open the dashboard but don't expose the token." -> `dashboard --open`, read token from the token file later.

These prompts are safe targets for skill regression checks. Never substitute the requester's OS/arch for a worker's actual capability.

## Installation reference

If `ylcc` is missing or the skill is installed from a newer/older release, read [installation.md](installation.md) before attempting to configure a project. Never ask for token contents.
