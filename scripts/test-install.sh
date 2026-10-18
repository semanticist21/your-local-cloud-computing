#!/usr/bin/env bash
# Exercise actual installation, corruption refusal, and skill overwrite protection without network access.
set -euo pipefail
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
task_tmp="$(mktemp -d)"
trap 'rm -rf "$task_tmp"' EXIT
mkdir -p "$task_tmp/mock" "$task_tmp/release" "$task_tmp/payload" "$task_tmp/user"
printf '#!/bin/sh\nprintf "ylcc test\\n"\n' > "$task_tmp/payload/ylcc"
chmod +x "$task_tmp/payload/ylcc"
cp "$repo/LICENSE" "$task_tmp/payload/LICENSE"
printf 'Third-party fixture notices\n' > "$task_tmp/payload/THIRD_PARTY_NOTICES.txt"
cat > "$task_tmp/mock/uname" <<'MOCK'
#!/bin/sh
case "$1" in -s) printf '%s\n' "$TEST_OS" ;; -m) printf '%s\n' "$TEST_ARCH" ;; *) exit 1 ;; esac
MOCK
cat > "$task_tmp/mock/getconf" <<'MOCK'
#!/bin/sh
[ "$1" = GNU_LIBC_VERSION ] || exit 1
case "$TEST_LIBC" in
  glibc*) printf '%s\n' "$TEST_LIBC" ;;
  *) exit 1 ;;
esac
MOCK
cat > "$task_tmp/mock/ldd" <<'MOCK'
#!/bin/sh
case "$TEST_LIBC" in
  musl) printf 'musl libc (aarch64)\nVersion 1.2.5\n' >&2; exit 1 ;;
  glibc*) printf 'ldd (GNU libc) %s\n' "${TEST_LIBC#glibc }" ;;
  *) exit 1 ;;
esac
MOCK
cat > "$task_tmp/mock/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
url='' destination=''
while (($#)); do
  case "$1" in
    -o) destination="$2"; shift 2 ;;
    https://*) url="$1"; shift ;;
    *) shift ;;
  esac
done
case "$url" in
  https://github.com/semanticist21/your-local-cloud-computing/releases/download/v0.4.9/*) ;;
  *) printf 'Unexpected release URL: %s\n' "$url" >&2; exit 1 ;;
esac
cp "$TEST_RELEASE/${url##*/}" "$destination"
MOCK
chmod +x "$task_tmp/mock/"*
export PATH="$task_tmp/mock:$PATH" TEST_RELEASE="$task_tmp/release" TEST_LIBC='glibc 2.35'
for platform in 'Linux x86_64 linux x86_64' 'Linux aarch64 linux aarch64' 'Darwin x86_64 macos x86_64' 'Darwin arm64 macos aarch64'; do
  read -r TEST_OS TEST_ARCH os arch <<< "$platform"
  export TEST_OS TEST_ARCH
  archive="ylcc-$os-$arch.tar.gz"
  tar -czf "$TEST_RELEASE/$archive" -C "$task_tmp/payload" ylcc LICENSE THIRD_PARTY_NOTICES.txt
  if command -v sha256sum >/dev/null; then hash="$(sha256sum "$TEST_RELEASE/$archive")"; else hash="$(shasum -a 256 "$TEST_RELEASE/$archive")"; fi
  printf '%s  %s\n' "${hash%% *}" "$archive" > "$TEST_RELEASE/SHA256SUMS"
  bash "$repo/scripts/install.sh" --prefix "$task_tmp/prefix with spaces" >/dev/null
  [[ "$("$task_tmp/prefix with spaces/bin/ylcc")" == 'ylcc test' ]]
  cmp "$task_tmp/payload/LICENSE" "$task_tmp/prefix with spaces/share/licenses/ylcc/LICENSE"
  cmp "$task_tmp/payload/THIRD_PARTY_NOTICES.txt" "$task_tmp/prefix with spaces/share/licenses/ylcc/THIRD_PARTY_NOTICES.txt"
done
for unsupported_libc in musl 'glibc 2.34' unknown; do
  if TEST_OS=Linux TEST_ARCH=x86_64 TEST_LIBC="$unsupported_libc" bash "$repo/scripts/install.sh" --prefix "$task_tmp/unsupported" >"$task_tmp/libc-error" 2>&1; then
    printf 'Unsupported libc was accepted: %s\n' "$unsupported_libc" >&2; exit 1
  fi
  [[ ! -e "$task_tmp/unsupported/bin/ylcc" ]]
  if ! grep -q 'glibc 2.35+.*container image or build from source' "$task_tmp/libc-error"; then
    printf 'Missing actionable libc guidance for %s\n' "$unsupported_libc" >&2; exit 1
  fi
done
printf '\ncorrupt\n' >> "$TEST_RELEASE/$archive"
if bash "$repo/scripts/install.sh" --prefix "$task_tmp/rejected" >/dev/null 2>&1; then
  printf 'Corrupt download was accepted\n' >&2; exit 1
fi
[[ ! -e "$task_tmp/rejected/bin/ylcc" ]]
# Recalculate checksums so these cases test archive validation, not checksum failure.
for unsafe in extra duplicate traversal symlink hardlink directory; do
  python3 - "$TEST_RELEASE/$archive" "$task_tmp/payload" "$unsafe" <<'PYARCHIVE'
import io
from pathlib import Path
import sys
import tarfile

archive, directory, variant = sys.argv[1:]
with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as output:
    for name in ("ylcc", "LICENSE", "THIRD_PARTY_NOTICES.txt"):
        member = tarfile.TarInfo(name)
        if name == "ylcc" and variant in ("symlink", "hardlink", "directory"):
            member.type = {"symlink": tarfile.SYMTYPE, "hardlink": tarfile.LNKTYPE,
                           "directory": tarfile.DIRTYPE}[variant]
            member.linkname = "LICENSE" if variant != "directory" else ""
            output.addfile(member)
        else:
            data = (Path(directory) / name).read_bytes()
            member.size = len(data)
            output.addfile(member, io.BytesIO(data))
    if variant in ("extra", "duplicate", "traversal"):
        name = {"extra": "unexpected", "duplicate": "ylcc", "traversal": "../escaped"}[variant]
        output.addfile(tarfile.TarInfo(name), io.BytesIO())
PYARCHIVE
  if command -v sha256sum >/dev/null; then hash="$(sha256sum "$TEST_RELEASE/$archive")"; else hash="$(shasum -a 256 "$TEST_RELEASE/$archive")"; fi
  printf '%s  %s\n' "${hash%% *}" "$archive" > "$TEST_RELEASE/SHA256SUMS"
  if bash "$repo/scripts/install.sh" --prefix "$task_tmp/prefix with spaces" >/dev/null 2>&1; then
    printf 'Unsafe archive was accepted: %s\n' "$unsafe" >&2; exit 1
  fi
  [[ "$("$task_tmp/prefix with spaces/bin/ylcc")" == 'ylcc test' ]]
  [[ ! -e "$task_tmp/escaped" ]]
  cmp "$task_tmp/payload/LICENSE" "$task_tmp/prefix with spaces/share/licenses/ylcc/LICENSE"
done
(cd "$task_tmp" && HOME="$task_tmp/user" bash "$repo/scripts/install-skill.sh") >/dev/null
HOME="$task_tmp/user" bash "$repo/scripts/install-skill.sh" >/dev/null
printf '\nlocal customization\n' >> "$task_tmp/user/.agents/skills/ylcc/SKILL.md"
if HOME="$task_tmp/user" bash "$repo/scripts/install-skill.sh" >/dev/null 2>&1; then
  printf 'Existing customized skill was overwritten\n' >&2; exit 1
fi
bash "$repo/scripts/install-skill.sh" --project "$task_tmp/project with spaces" >/dev/null
cmp "$repo/skills/ylcc/SKILL.md" "$task_tmp/project with spaces/.agents/skills/ylcc/SKILL.md"
# Config rendering needs no daemon and catches security-incompatible service URLs.
if command -v docker >/dev/null && docker compose version >/dev/null 2>&1; then
  python3 - "$repo/compose.yaml" "$task_tmp" <<'PYCOMPOSE'
import json
import os
from pathlib import Path
import subprocess
import sys

compose, directory = sys.argv[1:]
root = Path(directory)
(root / "compose-data/hub").mkdir(parents=True)
(root / "compose-data/worker").mkdir()
(root / "compose-token").write_text("0123456789abcdef0123456789abcdef\n")
(root / "compose-token").chmod(0o600)
config = dict(os.environ, YLCC_UID=str(os.getuid()), YLCC_GID=str(os.getgid()),
              YLCC_DATA_DIR=str(root / "compose-data"), YLCC_TOKEN_FILE=str(root / "compose-token"))
rendered = json.loads(subprocess.check_output(
    ["docker", "compose", "-f", compose, "config", "--format", "json"], env=config))
services = rendered["services"]
worker = services["worker"]
assert worker["network_mode"] == "service:hub", "HTTP worker must share the hub network namespace"
command = worker["command"]
assert command[command.index("--hub") + 1] == "http://127.0.0.1:7443", "plaintext HTTP must use loopback"
assert not worker.get("ports"), "only the hub publishes a loopback port"
ports = services["hub"]["ports"]
assert len(ports) == 1 and ports[0]["host_ip"] == "127.0.0.1", "hub must not publish beyond host loopback"
assert str(ports[0]["published"]) == "7443" and ports[0]["target"] == 7443
print("Compose rendered config passed: shared namespace, loopback HTTP, loopback-only publication.")
PYCOMPOSE
else
  printf 'Compose rendered-config check skipped: Docker Compose plugin is unavailable.\n'
fi
printf 'Installer checks passed: four platforms, licenses, libc compatibility, spaces, corrupt/unsafe archives, skill idempotence and overwrite protection.\n'
