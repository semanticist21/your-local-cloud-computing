English | [한국어](ko/environments.md)

# Reusable environments

Register and prepare Rust, Node or Python image environments from the [support checkout](install.md#download-examples-and-agent-support-files):

```bash
ylcc env add "$YLCC_SUPPORT_DIR/examples/environments/rust.json"
ylcc env prepare rust-linux --worker linux-build
ylcc env status
(
  cd "$YLCC_SUPPORT_DIR"
  ylcc run --input examples/jobs/hello.rs --os linux --arch x86_64 \
    --environment rust-linux --output result.json \
    -- sh -c 'rustc examples/jobs/hello.rs -o hello && ./hello'
)
```

The example image references are pinned multi-platform digests. Use [Node](../examples/environments/node.json) or [Python](../examples/environments/python.json) similarly. Image environments execute on a worker with a usable container runtime; Docker on macOS still runs Linux. Images are pulled and verified in the actual execution environment, and resolved digests/readiness are stored per worker.

For native Linux/macOS tools, [the rustup recipe](../examples/environments/rust-native.json) installs a pinned toolchain into the environment's user-owned directory. Project-specific recipes can use existing version managers such as mise, a release binary, or Git clone/build commands. Commands receive `YLCC_ENV_DIR`, with its `bin` directory prepended to `PATH`; environment values expand `${YLCC_ENV_DIR}` and `${PATH}`. Keep install steps repeatable and versioned, and provide `verify` commands that fail when the expected tools are unusable.

Use a new `revision` when changing an environment, then `env add` and `env prepare` explicitly. `ylcc env delete ID` removes an environment from the catalogue with its statuses and preparation logs; it succeeds when the id is already gone and refuses while queued or active jobs use it. Files already prepared on workers stay in their data directories; past jobs keep their recorded environment, and retrying one needs the id registered again. Preparation/update is serialized and drains active work on the worker. Jobs can prepare their requested native environment when first used. Image environments are immutable: put setup in the image Dockerfile, not a disposable setup command. Persistent environment/cache data lives under the worker data directory and must be mounted for a container worker. Updating a worker image is an explicit pull/recreate after jobs drain, with these mounts retained.

Installing an API CLI does not authenticate it. Supply required credentials through the service user's existing secure configuration. Drivers, system SDKs and Xcode remain host setup; the tool does not change them automatically.
