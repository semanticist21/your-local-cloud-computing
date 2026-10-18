English | [한국어](ko/tasks.md)

# Project tasks

Keep repeatable commands in `ylcc.yaml` in your application repository. The requester uses its existing user-level Hub/token configuration; do not put credentials in this file. Commands and selected paths are relative to the YAML file's directory, even when invoked elsewhere.

```yaml
version: 1
tasks:
  test:
    command: [cargo, test, --locked]
    inputs: [Cargo.toml, Cargo.lock, src, tests]
    requirements:
      os: linux
      arch: x86_64
      tools:
        cargo: "*"
    timeout_secs: 1800
    priority: 0
```

Use your project's actual inputs and command. Add `outputs` for files to download, `environment` for a tool environment, or use `project: true` instead of `inputs` to select the whole project. A command without source inputs must explicitly set `no_input: true`. Project files define executable commands; inspect a new file before running it.

```bash
ylcc task list
ylcc task run test
ylcc task run test --file /path/to/application/ylcc.yaml --detach
ylcc task delete test
```

`task delete` removes the task block and the comment lines directly above it, keeps the rest of the file, and succeeds when the task is already gone. It refuses a removal that would leave the file invalid, such as deleting a task whose YAML anchor other tasks use. To change a saved task, edit it in place, or delete it and save it again.

## Repeatable workflows

A project file can carry everything a run needs, like a CI workflow: shared defaults, the toolchain, per-OS targets and worker-side caches.

```yaml
version: 1
environments:
  bun: {uses: bun@1.3.14}          # Built-in preset; `environment: bun@1.3.14` also works inline.
  tools:                           # Or a full recipe (setup/verify/env, or an image digest).
    setup: ['curl -fsSL https://example.com/tool.tar.gz | tar -xz -C "$YLCC_ENV_DIR/bin"']
    verify: [tool --version]
defaults:
  requirements: {os: linux, arch: x86_64}
  environment: bun
  timeout_secs: 1800
  cache: [node_modules]
tasks:
  test:
    command: [sh, -c, 'bun install --frozen-lockfile && bun test']
    inputs: [package.json, bun.lock, src]
  cargo-test:
    command: [cargo, test, --locked]
    inputs: [Cargo.toml, Cargo.lock, src]
    environment: rust-native       # A name not declared above refers to a Hub-registered environment.
    cache: [target]
    targets:                       # One job per target; each overrides the requirements.
      - {os: linux, arch: x86_64}
      - {os: macos, arch: aarch64}
```

- **Environments** declared in the file are registered when `task run` uses them. Each worker installs one on first use and keeps it, so later runs start immediately. The revision is a hash of the recipe: editing it prepares a new revision, while an unchanged recipe is reused. A custom recipe's Hub id gets a project suffix (for example `tools-3fa2…`), so projects reusing a name never replace each other's recipe or a separately registered environment.
- **Presets** (`bun`, `node`, `rust`, `go`, `uv`) install into the worker's environment directory: a shell recipe on Linux/macOS, a PowerShell recipe on Windows x86_64 (id suffix `-windows`). A preset needs an explicit `os` in the requirements or target to pick its recipe. Rust on Windows links MSVC targets only with Visual Studio Build Tools installed on the host.
- **Defaults** apply to every task that does not set the field itself; `env` maps merge.
- **Targets** submit one job per entry. The snapshot uploads once. `task run` prints one object for one target or an array for several, and fails if any job fails.
- **Cache** directories are moved out of the checkout after the job and back into the next one on the same worker with the same project, environment, container and path. The project is its `name` when set (to share caches between requesters), otherwise the YAML directory's absolute path. Caches are kept after failures too. If another attempt is using a cache, the job runs without it rather than waiting. A task with caches checks out into a fixed `workspaces/` directory for that project, environment and container (still fresh each time), because build outputs such as Cargo's `target` embed absolute paths; a concurrent attempt of the same task uses its own directory. Caches live under the worker's `caches/` data directory and are evicted as described in [Worker disk cleanup](services.md#worker-disk-cleanup). A path already present in the inputs is never replaced.

Turn a working ad-hoc run into a task with `--save-task NAME`. It appends to `./ylcc.yaml` (creating it if needed, keeping existing comments) after the job succeeds, or at submission with `--detach`. Inputs and commands stay relative to the current directory. The run is refused before submission if the file's `defaults` would make the saved task run differently:

```bash
ylcc run --input package.json --input bun.lock --input src --os linux --environment bun@1.3.14 \
  --cache node_modules --save-task test -- sh -c 'bun install && bun test'
ylcc task run test
```

YAML version 1 rejects unknown fields and invalid selections. CPU/memory percentage limits are not supported by this schema; configure actual resource caps in the container runtime or operating system. `max_jobs` limits simultaneous jobs, not their CPU or memory consumption.

## Control queued work

The Hub persists the queue and worker controls. Higher numeric priority runs first among compatible queued jobs; equal priorities retain submission order. Priority defaults to `0` and accepts `-100` through `100`. Changing priority never interrupts an assigned or running job.

```bash
ylcc run --no-input --os linux --priority 10 --detach -- true
ylcc jobs priority JOB_ID 20
ylcc workers pause WORKER_ID
ylcc workers resume WORKER_ID
ylcc workers limit WORKER_ID --max-jobs 1
```

Pausing stops new assignments and new environment preparation while existing work finishes. Resume allows scheduling again. A live concurrency limit may be raised only up to the worker's startup `--max-jobs` capacity; lowering it lets active work finish. Set the startup capacity when installing the worker service. Controls survive Hub restarts and worker reconnects. To update safely, pause a worker, wait until its active attempts finish, update it, then resume it. Cancellation remains an explicit per-job operation.

Priority submission requires a Hub advertising priority support; older Hubs are rejected instead of silently ignoring a nonzero priority.

## Setup and fast task templates

Run first-run discovery without printing secrets:

```bash
ylcc setup
ylcc examples
```

Create a project task file in the current application repository. Runtime templates declare a preset environment and cache:

```bash
ylcc task init --runtime bun --name test --file ylcc.yaml
ylcc task init --runtime rust --name test --file ylcc.yaml
ylcc task list
ylcc task run test --detach
```

Reuse a prepared environment before trusting it:

```bash
ylcc env status
ylcc env ensure my-tools --worker WORKER_ID
```
