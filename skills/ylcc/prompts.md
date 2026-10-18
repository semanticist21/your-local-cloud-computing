# Natural-language prompts that the skill should route well

These are examples of agent requests and the command flow ylcc is built to handle. Assert that each request still runs the shown command styles after a CLI change.

1. "Are any workers online, and what can they actually do? Don't run anything." 
   - `ylcc --json workers`
   - Report OS, architecture, toolchains, GPU and `online` status verbatim; don't infer a GPU/CUDA build from CPU tooling.

2. "Only run this check on a Windows x86_64 worker that is powered on."
   - `ylcc --json workers`
   - `ylcc run --os windows --arch x86_64 --no-input --json -- powershell -NoProfile -Command "..."`

3. "Build this on Linux x86_64 and bring back the binary."
   - `ylcc --json manifest --project` when choosing inputs is unclear
   - `ylcc run --input Cargo.toml --input Cargo.lock --input src --os linux --arch x86_64 --output target/release/my-tool --detach -- cargo build --locked --release`
   - `ylcc jobs wait <job-id>`

4. "Run the project's reviewed build task, then fetch its output even if it takes a while."
   - `ylcc --json task list`
   - `ylcc task run build --detach`
   - `ylcc --json jobs wait <job-id>`

5. "Verify this environment is prepared before trusting it."
   - `ylcc --json env status`
   - `ylcc env prepare my-tools --worker WORKER_ID` if it is not ready
   - Re-run the environment's `verify` commands; a successful preparation never proves credentials.

6. "Queue this low-priority smoke test after the current production job:"
   - `ylcc run --no-input --os linux --arch x86_64 --priority -100 --detach -- true`
   - To change a queued job: `ylcc jobs priority JOB_ID 20`

7. "Use the dashboard without giving you the token."
   - `ylcc dashboard --open`
   - Read the token value only from the displayed token-file location; never paste it in another command or output.

8. "Set up the monorepo's tests on the pool so I can rerun them."
   - `ylcc env presets` to choose a toolchain preset, or write a `setup`/`verify` recipe
   - First run: `ylcc run --input ... --os linux --cache node_modules --save-task web-test -- sh -c 'bun install && bun test'`
   - Then: `ylcc task run web-test`; for the same task on several OS/arch, add `targets` in `ylcc.yaml`
