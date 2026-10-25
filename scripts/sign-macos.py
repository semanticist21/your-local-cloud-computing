#!/usr/bin/env python3
"""Sign and notarize a release CLI without exposing credentials in process arguments."""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import subprocess
import sys
import tempfile
import zipfile

IDENTIFIER = "com.kobbokkom.ylcc"
CREDENTIALS = ["APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"]


def run(*args, stdin=None, timeout=60):
    try:
        result = subprocess.run(args, input=stdin, text=True, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{Path(args[0]).name} timed out; output withheld to protect credentials") from None
    if result.returncode:
        if Path(args[0]).name == "codesign":
            detail = result.stderr
            for name in CREDENTIALS:
                if value := os.environ.get(name):
                    detail = detail.replace(value, "<redacted>")
            detail = re.sub(r"Developer ID Application:[^\r\n]+", "Developer ID Application: <redacted>", detail)
            raise RuntimeError(f"codesign exited {result.returncode}: {detail.strip()}")
        raise RuntimeError(f"{Path(args[0]).name} exited {result.returncode}; output withheld to protect credentials")
    return result


def security(*args):
    if any(char in arg for arg in args for char in "\r\n\0"):
        raise ValueError("invalid security command input")
    return run("security", "-i", stdin=shlex.join(args) + "\n")


def sign(binary):
    binary = Path(binary).resolve(strict=True)
    if not all(os.environ.get(name) for name in CREDENTIALS):
        raise ValueError("release signing credentials are missing")
    team = os.environ["APPLE_TEAM_ID"]
    if not re.fullmatch(r"[A-Z0-9]{10}", team):
        raise ValueError("invalid signing team")
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="ylcc-sign-") as folder:
        folder = Path(folder)
        keychain = str(folder / "release.keychain-db")
        certificate = folder / "certificate.p12"
        certificate.write_bytes(base64.b64decode(os.environ["APPLE_CERTIFICATE"], validate=True))
        password = secrets.token_hex(32)
        try:
            security("create-keychain", "-p", password, keychain)
            security("set-keychain-settings", "-lut", "3600", keychain)
            security("unlock-keychain", "-p", password, keychain)
            security("import", str(certificate), "-P", os.environ["APPLE_CERTIFICATE_PASSWORD"],
                     "-k", keychain, "-T", "/usr/bin/codesign")
            security("set-key-partition-list", "-S", "apple-tool:,apple:,codesign:", "-s", "-k", password, keychain)
            # --keychain limits certificate lookup; the private key also needs the user search list.
            search = shlex.split(run("security", "list-keychains", "-d", "user").stdout)
            security("list-keychains", "-d", "user", "-s", keychain,
                     *[path for path in search if Path(path).resolve() != Path(keychain).resolve()])
            identities = run("security", "find-identity", "-v", "-p", "codesigning", keychain).stdout
            identities = re.findall(r'([A-F0-9]{40}) "Developer ID Application: [^"\n]+"', identities)
            if len(identities) != 1:
                raise ValueError("exactly one valid Developer ID Application identity is required")
            run("codesign", "--force", "--sign", identities[0], "--keychain", keychain,
                "--identifier", IDENTIFIER, "--options", "runtime", "--timestamp", str(binary))
            requirement = (f'identifier "{IDENTIFIER}" and anchor apple generic and '
                           f'certificate leaf[subject.OU] = "{team}" and '
                           'certificate leaf[field.1.2.840.113635.100.6.1.13] exists')
            run("codesign", "--verify", "--strict", "-R", "=" + requirement, str(binary))
            details = run("codesign", "-dv", "--verbose=4", str(binary)).stderr
            if f"Identifier={IDENTIFIER}\n" not in details or f"TeamIdentifier={team}\n" not in details:
                raise ValueError("signing identifier or team does not match")
            if "Signature=adhoc" in details or "Timestamp=" not in details:
                raise ValueError("a timestamped Developer ID signature is required")
            displayed = run("codesign", "-dr", "-", str(binary))
            designated = next(line.split("designated => ", 1)[1] for line in
                              (displayed.stdout + displayed.stderr).splitlines() if "designated => " in line)
            if f'identifier "{IDENTIFIER}"' not in designated or team not in designated or "cdhash" in designated:
                raise ValueError("the designated requirement must preserve developer identity across updates")

            run("xcrun", "notarytool", "store-credentials", "ylcc-release", "--keychain", keychain,
                "--apple-id", os.environ["APPLE_ID"], "--team-id", team,
                stdin=os.environ["APPLE_PASSWORD"] + "\n")
            archive = folder / "ylcc.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
                output.write(binary, "ylcc")
            authentication = ["--keychain-profile", "ylcc-release", "--keychain", keychain]
            submission = json.loads(run("xcrun", "notarytool", "submit", str(archive), *authentication,
                                        "--wait", "--timeout", "30m", "--output-format", "json", timeout=1860).stdout)
            if submission["status"] != "Accepted":
                raise ValueError(f"notarization rejected; submission {submission['id']}")
            log = json.loads(run("xcrun", "notarytool", "log", submission["id"], *authentication).stdout)
            if any(issue["severity"] == "error" for issue in log.get("issues") or []):
                raise ValueError("notarization log contains errors")
            run("codesign", "--verify", "--strict", "-R", "=" + requirement, str(binary))
            print(json.dumps({"identifier": IDENTIFIER, "requirement_sha256": hashlib.sha256(designated.encode()).hexdigest(),
                              "notarization_id": submission["id"], "notarization_status": submission["status"],
                              "notarization_warnings": len(log.get("issues") or [])}))
        finally:
            if Path(keychain).exists():
                run("security", "delete-keychain", keychain)


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.platform != "darwin":
        sys.exit("usage (macOS): scripts/sign-macos.py PATH_TO_YLCC")
    sign(sys.argv[1])
