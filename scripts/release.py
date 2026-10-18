#!/usr/bin/env python3
"""Keep every pinned release reference equal to Cargo.toml's version.

  scripts/release.py X.Y.Z   bump Cargo.toml, Cargo.lock and all pins, then commit
  scripts/release.py --check fail when any pin differs from Cargo.toml (run in CI)

Pushing the bump commit to main is the release: the Release workflow tags vX.Y.Z on that
commit and publishes it once every build passes, so a tag can never disagree with the code.
"""
import pathlib, re, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PINNED = ["README.md", "README.ko.md", "docs/install.md", "docs/ko/install.md",
          "scripts/install.sh", "scripts/install.ps1", "scripts/test-install.sh", "scripts/test-install.ps1"]
TAG = re.compile(r"\bv\d+\.\d+\.\d+\b")
PACKAGE = re.compile(r'^version = "([^"]+)"', re.M)


def current():
    return PACKAGE.search((ROOT / "Cargo.toml").read_text()).group(1)


def check():
    expected, bad = f"v{current()}", []
    for name in PINNED:
        for number, line in enumerate((ROOT / name).read_text().splitlines(), 1):
            bad += [f"{name}:{number}: {tag} != {expected}" for tag in TAG.findall(line) if tag != expected]
    print("\n".join(bad) or f"all release pins are {expected}")
    return not bad


def bump(version):
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        sys.exit("usage: scripts/release.py X.Y.Z (no leading v)")
    if subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT,
                      capture_output=True, text=True, check=True).stdout:
        sys.exit("commit or stash tracked changes first")
    cargo = ROOT / "Cargo.toml"
    cargo.write_text(PACKAGE.sub(f'version = "{version}"', cargo.read_text(), count=1))
    for name in PINNED:
        path = ROOT / name
        path.write_text(TAG.sub(f"v{version}", path.read_text()))
    subprocess.run(["cargo", "update", "--workspace", "--offline"], cwd=ROOT, check=True)
    assert check()
    subprocess.run(["git", "commit", "-qam", f"release: v{version}"], cwd=ROOT, check=True)
    print(f"committed v{version}; push main to publish it")


if __name__ == "__main__":
    if sys.argv[1:] == ["--check"]:
        sys.exit(0 if check() else 1)
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    bump(sys.argv[1])
