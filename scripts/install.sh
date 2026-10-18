#!/usr/bin/env bash
set -euo pipefail

version="${YLCC_VERSION:-v0.4.8}"
prefix="${YLCC_PREFIX:-${HOME:?HOME is required}/.local}"
while (($#)); do
  case "$1" in
    --version) version="${2:?--version requires a value}"; shift 2 ;;
    --prefix) prefix="${2:?--prefix requires a value}"; shift 2 ;;
    -h|--help) printf 'Usage: install.sh [--version v0.4.8] [--prefix ~/.local]\n'; exit 0 ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; exit 1 ;;
  esac
done
case "$version" in v[0-9]*) ;; *) printf 'Expected a release tag such as v0.4.8\n' >&2; exit 1 ;; esac
[[ "$version" != *[!a-zA-Z0-9._-]* ]] || { printf 'Invalid release tag\n' >&2; exit 1; }
case "$(uname -s)" in Linux) os=linux ;; Darwin) os=macos ;; *) printf 'Supported systems: Linux and macOS\n' >&2; exit 1 ;; esac
case "$(uname -m)" in x86_64|amd64) arch=x86_64 ;; arm64|aarch64) arch=aarch64 ;; *) printf 'Supported architectures: x86_64 and arm64\n' >&2; exit 1 ;; esac
if [[ "$os" == linux ]]; then
  libc="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
  loader="$(ldd --version 2>&1 || true)"
  if [[ "$loader" == *musl* || ! "$libc" =~ ^glibc[[:space:]]+([0-9]+)\.([0-9]+) ]]; then
    printf 'Prebuilt Linux binaries require glibc 2.35+; musl or an unidentified libc is unsupported. Use the container image or build from source for your system.\n' >&2
    exit 1
  fi
  libc_major="${BASH_REMATCH[1]}" libc_minor="${BASH_REMATCH[2]}"
  if ((10#$libc_major < 2 || (10#$libc_major == 2 && 10#$libc_minor < 35))); then
    printf 'Prebuilt Linux binaries require glibc 2.35+ (detected %s). Use the container image or build from source for your system.\n' "$libc" >&2
    exit 1
  fi
fi
command -v curl >/dev/null || { printf 'curl is required\n' >&2; exit 1; }
archive="ylcc-${os}-${arch}.tar.gz"
base="https://github.com/semanticist21/your-local-cloud-computing/releases/download/${version}"
task_tmp="$(mktemp -d)"
trap 'rm -rf "$task_tmp"; if [[ -n "${install_tmp:-}" ]]; then rm -f "$install_tmp"; fi' EXIT
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 "$base/$archive" -o "$task_tmp/$archive"
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 "$base/SHA256SUMS" -o "$task_tmp/SHA256SUMS"
expected="$(awk -v name="$archive" '$2 == name {print $1; count++} END {if(count != 1) exit 1}' "$task_tmp/SHA256SUMS")" || { printf 'Archive missing or duplicated in SHA256SUMS\n' >&2; exit 1; }
[[ ${#expected} == 64 && "$expected" != *[!a-fA-F0-9]* ]] || { printf 'Malformed checksum\n' >&2; exit 1; }
if command -v sha256sum >/dev/null; then actual="$(sha256sum "$task_tmp/$archive")"; else actual="$(shasum -a 256 "$task_tmp/$archive")"; fi
actual="${actual%% *}"
[[ "$actual" == "$expected" ]] || { printf 'Checksum mismatch: refusing installation\n' >&2; exit 1; }
# Validate names, duplicates and file types before writing any destination file.
members="$(LC_ALL=C tar -tzf "$task_tmp/$archive" | LC_ALL=C sort)"
[[ "$members" == $'LICENSE\nTHIRD_PARTY_NOTICES.txt\nylcc' ]] || { printf 'Unexpected archive contents\n' >&2; exit 1; }
LC_ALL=C tar -tvzf "$task_tmp/$archive" | awk '
  substr($0, 1, 1) != "-" {bad=1}
  END {if (NR != 3 || bad) exit 1}
' || { printf 'Archive members must be regular files\n' >&2; exit 1; }
# Stream named members rather than extracting paths or archive-owned permissions.
for member in ylcc LICENSE THIRD_PARTY_NOTICES.txt; do
  tar -xOzf "$task_tmp/$archive" "$member" > "$task_tmp/$member"
done
mkdir -p "$prefix/bin"
mkdir -p "$prefix/share/licenses/ylcc"
for member in LICENSE THIRD_PARTY_NOTICES.txt; do
  install_tmp="$(mktemp "$prefix/share/licenses/ylcc/.notice.XXXXXX")"
  install -m 644 "$task_tmp/$member" "$install_tmp"
  mv -f "$install_tmp" "$prefix/share/licenses/ylcc/$member"
  install_tmp=''
done
install_tmp="$(mktemp "$prefix/bin/.ylcc.XXXXXX")"
install -m 755 "$task_tmp/ylcc" "$install_tmp"
mv -f "$install_tmp" "$prefix/bin/ylcc"
install_tmp=''
printf 'Installed %s to %s/bin/ylcc\n' "$version" "$prefix"
printf 'Add %s/bin to PATH, then run ylcc --help.\n' "$prefix"
