English | [한국어](ko/development.md)

# Diagnose and develop

`ylcc doctor` reports local capabilities and configured workers. Check service logs, actual service-account `PATH`, token permissions, private HTTPS reachability and container runtime access first. A worker connection loss is distinct from command failure. Persist the hub database/blob directory and worker data; do not delete an unknown attempt's files while its process may still be running.

```bash
(
  cd "$YLCC_SUPPORT_DIR"
  cargo test --locked
  bash scripts/test-install.sh
  cargo build --locked --release
)
```

CI builds/tests on native Linux x86_64, Linux arm64, Intel macOS, Apple Silicon macOS and Windows x86_64. Windows installer checks run under Windows PowerShell 5.1. Windows container CI uses a Server 2022 Docker engine, initializes a disposable named volume, runs/fetches a real ContainerUser job, cancels a descendant process and checks persistence after recreation. Release automation creates four Unix archives plus a Windows ZIP, checksums, a Linux multi-platform image, a separate Windows LTSC 2022 image and a published GitHub Release.

To release, run `scripts/release.py X.Y.Z` on a clean main checkout and push. It bumps `Cargo.toml`, `Cargo.lock` and every pinned install reference in one commit; the Release workflow tags `vX.Y.Z` on that commit and publishes it after every build passes. Never push a tag by hand. CI runs `scripts/release.py --check`, so a pin that disagrees with `Cargo.toml` fails the build. Only release builds report the bare version; any other build reports `X.Y.Z+g<commit>` (with `.dirty` for uncommitted changes), or `X.Y.Z+source` when built without Git.
