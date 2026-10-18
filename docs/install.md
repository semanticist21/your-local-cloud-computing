English | [한국어](ko/install.md)

# Installation

Install the binary as shown in the [README](../README.md#install-the-cli).

Use `--prefix /your/prefix` to change the install location. You can also download and check the [release archives](https://github.com/semanticist21/your-local-cloud-computing/releases) yourself. macOS builds are unsigned command-line programs; they are not notarized application bundles. Linux release binaries require glibc 2.35 or newer so native workers can load GPU driver libraries. On musl-based systems such as Alpine, use the container image or build from source for your system; GPU probing additionally requires dynamic library loading. Workload binaries keep their own platform/ABI requirements.

## Download examples and agent support files

The binary installer installs the executable and license notices. Before using the example jobs, environment recipes, Compose/Quadlet files or agent installer in these docs, fetch the matching versioned support checkout outside your application project:

```bash
export YLCC_SUPPORT_DIR="$HOME/.local/share/ylcc/support/v0.5.0"
mkdir -p "$(dirname "$YLCC_SUPPORT_DIR")"
git clone --depth 1 --branch v0.5.0 \
  https://github.com/semanticist21/your-local-cloud-computing.git "$YLCC_SUPPORT_DIR"
```

Keep this directory for the examples in these docs. Reuse it if already cloned; it contains public support files, never your pool token. Commands reference this directory explicitly or run examples in a subshell, preserving your application's working directory.

To build the CLI from this checkout instead of downloading a binary:

```bash
npm ci --prefix "$YLCC_SUPPORT_DIR/dashboard"
npm run build --prefix "$YLCC_SUPPORT_DIR/dashboard"
cargo build --manifest-path "$YLCC_SUPPORT_DIR/Cargo.toml" --locked --release
mkdir -p "$HOME/.local/bin"
install -m 755 "$YLCC_SUPPORT_DIR/target/release/ylcc" "$HOME/.local/bin/ylcc"
```

## Updating an existing installation

Run installer with desired version; config/token/data are preserved. Update background workers with [`ylcc workers update`](services.md#updating-workers).
