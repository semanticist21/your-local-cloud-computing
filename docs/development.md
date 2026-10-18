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

CI builds/tests on native Linux x86_64, Linux arm64, Intel macOS, Apple Silicon macOS and Windows x86_64. Windows installer checks run under Windows PowerShell 5.1. Windows container CI uses a Server 2022 Docker engine, initializes a disposable named volume, runs/fetches a real ContainerUser job, cancels a descendant process and checks persistence after recreation. Release automation creates four Unix archives plus a Windows ZIP, checksums, a Linux multi-platform image, a separate Windows LTSC 2022 image and a draft GitHub Release for final real-machine validation. Published release availability and successful local hardware tests are separate checks.
