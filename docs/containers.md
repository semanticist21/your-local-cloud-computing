English | [한국어](ko/containers.md)

# Docker and Podman

The worker image supports Linux amd64 and arm64. It includes YLCC, CA certificates, Git, Bash and curl; workload tools belong in reusable environments or your own image. The plain image has no runtime socket or nested container daemon.

Use your existing Docker or Podman runtime. On macOS, an existing [OrbStack](https://docs.orbstack.dev/docker/) or Docker Desktop installation can run the Linux worker; YLCC requires no second VM. Select the intended Docker context explicitly for each command instead of changing the global context. Keep the runtime's image cache on its configured disk, and bind-mount worker data from an external ownership-enforced volume when desired. Confirm the selected runtime can access that mount and the protected token as the intended user.

Using the [support checkout](install.md#download-examples-and-agent-support-files), publish the Compose hub only on host loopback and bind persistent directories owned by your current user. The worker shares the hub's network namespace and connects over its loopback, keeping plaintext HTTP local. Preserve the token's owner-only mode:

```bash
docker context ls
export YLCC_DOCKER_CONTEXT="$(docker context show)"
# To select an existing OrbStack context explicitly: export YLCC_DOCKER_CONTEXT=orbstack
docker --context "$YLCC_DOCKER_CONTEXT" info
export YLCC_UID="$(id -u)" YLCC_GID="$(id -g)"
export YLCC_DATA_DIR="$HOME/.local/share/ylcc/containers"
# For external work data, choose an already mounted ownership-enforced volume:
# export YLCC_DATA_DIR=/Volumes/YourOwnedVolume/ylcc/containers
export YLCC_TOKEN_FILE="$HOME/.config/ylcc/token"
mkdir -p "$YLCC_DATA_DIR/hub" "$YLCC_DATA_DIR/worker"
docker --context "$YLCC_DOCKER_CONTEXT" compose -f "$YLCC_SUPPORT_DIR/compose.yaml" up -d
```

The worker connects to `http://127.0.0.1:7443` inside the shared network namespace. To join an existing private hub instead, run only a worker with a persistent data mount:

```bash
mkdir -p "$YLCC_DATA_DIR/worker"
docker --context "$YLCC_DOCKER_CONTEXT" run -d --init --name ylcc-worker --restart unless-stopped \
  --user "$YLCC_UID:$YLCC_GID" \
  --mount "type=bind,src=$YLCC_DATA_DIR/worker,dst=/data" \
  --mount "type=bind,src=$YLCC_TOKEN_FILE,dst=/run/secrets/token,readonly" \
  ghcr.io/semanticist21/your-local-cloud-computing:0.5.15 \
  --hub https://HUB.example.ts.net:10000 --token-file /run/secrets/token \
  worker --id container-worker --data-dir /data --max-jobs 1
```

For rootless Podman service management, copy `$YLCC_SUPPORT_DIR/examples/quadlet/ylcc-worker.container` ([example](../examples/quadlet/ylcc-worker.container)) to `~/.config/containers/systemd/`, replace the hub URL and worker ID, then `systemctl --user daemon-reload && systemctl --user start ylcc-worker`. Its keep-id mapping lets the service read your mode-600 token. See [Podman Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html).

## Optional sibling-container execution

Native workers can use an already configured Docker or Podman runtime; verify its context and socket access under the worker service account's `HOME`. To request a workload image, pass `--container IMAGE`, or use an image environment. A containerized worker needs an explicitly installed runtime client and access to the host runtime. Build the optional client target from a matching release binary:

```bash
mkdir -p "$YLCC_SUPPORT_DIR/dist"
# Extract the verified Linux release archive for the runtime CPU; arm64 example:
tar -xzf /path/to/ylcc-linux-aarch64.tar.gz -C "$YLCC_SUPPORT_DIR/dist"
docker --context "$YLCC_DOCKER_CONTEXT" build --target runtime-client -t ylcc-runtime-client:0.5.15 \
  -f "$YLCC_SUPPORT_DIR/Dockerfile" "$YLCC_SUPPORT_DIR"
```

Mount the daemon socket explicitly and add the socket's group to the worker. **This grants the worker control of that host runtime.** Bind-mount the worker data directory at the **same absolute path** on host and worker, and pass that path as `--data-dir`; sibling container mounts are resolved by the host daemon, not the worker container. This profile uses a client only, never Docker-in-Docker. Docker Desktop path sharing and rootless Podman socket permissions must also allow the chosen paths/user.

For a Docker socket, set `YLCC_RUNTIME_SOCKET` to its absolute path and `YLCC_RUNTIME_GID` to its numeric group ID, then:

```bash
: "${YLCC_RUNTIME_SOCKET:?Set the Docker socket path}" "${YLCC_RUNTIME_GID:?Set its numeric group ID}"
export YLCC_RUNTIME_DATA="$HOME/.local/share/ylcc/runtime-worker"
mkdir -p "$YLCC_RUNTIME_DATA"
docker --context "$YLCC_DOCKER_CONTEXT" run -d --init --name ylcc-runtime-worker --restart unless-stopped \
  --user "$(id -u):$(id -g)" --group-add "$YLCC_RUNTIME_GID" \
  --mount "type=bind,src=$YLCC_RUNTIME_SOCKET,dst=/var/run/docker.sock" \
  --mount "type=bind,src=$YLCC_RUNTIME_DATA,dst=$YLCC_RUNTIME_DATA" \
  --mount "type=bind,src=$YLCC_TOKEN_FILE,dst=/run/secrets/token,readonly" \
  ylcc-runtime-client:0.5.15 \
  --hub https://HUB.example.ts.net:10000 --token-file /run/secrets/token \
  worker --id runtime-worker --data-dir "$YLCC_RUNTIME_DATA" --max-jobs 1
```

Build custom workload images outside YLCC, push them to GHCR/Forgejo or another registry, and register their immutable digest. Runtime credentials stay with the service user; they do not belong in the environment catalog.
