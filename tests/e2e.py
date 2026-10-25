#!/usr/bin/env python3
"""Real hub/worker/CLI contract test; uses only disposable local data."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import sqlite3
import urllib.request
import socket
import subprocess
import sys
import tempfile
import time


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/ylcc").resolve())
    with tempfile.TemporaryDirectory(prefix="ylcc-e2e-") as temporary:
        base = Path(temporary)
        token = base / "token"
        subprocess.run([binary, "--token-file", str(token), "init"], check=True, capture_output=True)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        hub_url = f"http://127.0.0.1:{port}"
        def using(token_file):
            return [binary, "--hub", hub_url, "--token-file", str(token_file), "--results-dir", str(base / "results"), "--json"]
        prefix = using(token)
        project = base / "project"
        project.mkdir()
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        (project / ".gitignore").write_text("ignored.txt\napp\nanswer.txt\n")
        (project / "input.txt").write_text("41\n")
        (project / "ignored.txt").write_text("explicit only\n")
        (project / "main.c").write_text('#include <stdio.h>\nint main(void){int n;if(scanf("%d",&n)!=1)return 2;printf("%d\\n",n+1);return 0;}\n')
        (project / "main.rs").write_text('use std::io::{self, Read}; fn main() { let mut s = String::new(); io::stdin().read_to_string(&mut s).unwrap(); println!("{}", s.trim().parse::<i32>().unwrap() + 1); }\n')
        subprocess.run(["git", "-C", str(project), "add", "input.txt", "main.c", "main.rs", ".gitignore"], check=True)
        (project / "input.txt").write_text("42\n")
        processes = []
        streams = []

        def start(role, *args, token_file=token, env=None):
            stream = (base / f"{role}-{len(streams)}.log").open("w+")
            streams.append(stream)
            process = subprocess.Popen(using(token_file) + [role, *args], stdout=stream, stderr=stream, start_new_session=os.name != "nt", env=None if env is None else {**os.environ, **env})
            processes.append(process)
            return process

        def cli(*args, ok=True, timeout=90, token_file=token):
            result = subprocess.run(using(token_file) + list(args), cwd=project, text=True, capture_output=True, timeout=timeout)
            if ok and result.returncode:
                raise AssertionError(f"{args}: {result.stderr}\n{result.stdout}")
            if not ok:
                assert result.returncode != 0, result.stdout
                return result
            return json.loads(result.stdout), result.stderr

        def until(function, timeout=60):
            deadline = time.monotonic() + timeout
            last = None
            while time.monotonic() < deadline:
                try:
                    last = function()
                    if last:
                        return last
                except (AssertionError, json.JSONDecodeError):
                    pass
                time.sleep(0.2)
            raise AssertionError(f"timed out waiting; last={last}")

        def state(job):
            return cli("jobs", "show", job["id"])[0]

        def wait(job):
            return cli("jobs", "wait", job["id"])[0]

        def result(job, name="answer.txt"):
            return (base / "results" / job["id"] / name).read_text()

        def stop(process):
            if process.poll() is None:
                if os.name == "nt":
                    process.terminate()
                    process.wait(timeout=10)
                    return
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()

        def python(code):
            return [sys.executable, "-c", code]

        copy_input = "from pathlib import Path; Path('answer.txt').write_bytes(Path('input.txt').read_bytes())"
        noop = python("pass")

        def process_alive(pid):
            if os.name == "nt":
                import ctypes
                from ctypes import wintypes
                kernel = ctypes.WinDLL("kernel32", use_last_error=True)
                kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
                kernel.OpenProcess.restype = wintypes.HANDLE
                kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
                kernel.CloseHandle.argtypes = [wintypes.HANDLE]
                handle = kernel.OpenProcess(0x1000, False, pid)
                if not handle:
                    error = ctypes.get_last_error()
                    if error in (87, 1168):
                        return False
                    raise ctypes.WinError(error)
                try:
                    code = wintypes.DWORD()
                    if not kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
                        raise ctypes.WinError(ctypes.get_last_error())
                    return code.value == 259
                finally:
                    kernel.CloseHandle(handle)
            status = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
            return bool(status.stdout.strip()) and not status.stdout.strip().startswith("Z")

        try:
            # Environment limits are read once, validated before touching persistent data, and enforced by HTTP.
            for variable in ["YLCC_WS_MAX_MIB", "YLCC_HTTP_JSON_MAX_MIB", "YLCC_METADATA_MAX_MIB"]:
                for role in ["hub", "worker"]:
                    invalid_data = base / f"invalid-{role}"
                    invalid = subprocess.run(prefix + [role, "--data-dir", str(invalid_data)],
                                             env={**os.environ, variable: "0"}, capture_output=True, text=True, timeout=10)
                    assert invalid.returncode != 0 and variable in invalid.stderr, invalid.stderr
                    assert not invalid_data.exists()
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"),
                        env={"YLCC_WS_MAX_MIB": "192", "YLCC_HTTP_JSON_MAX_MIB": "1", "YLCC_METADATA_MAX_MIB": "768"})
            until(lambda: cli("workers")[0] == [])
            with urllib.request.urlopen(hub_url + "/health") as response:
                limits = json.load(response)["transfer_limits"]
            assert limits == {"websocket": 192 * 1024 * 1024, "http_json": 1024 * 1024, "metadata": 768 * 1024 * 1024}, limits
            request = urllib.request.Request(hub_url + "/v1/blobs/missing", data=json.dumps({"hashes": [], "padding": "x" * (1024 * 1024)}).encode(),
                                             headers={"Authorization": "Bearer " + token.read_text().strip(), "Content-Type": "application/json"}, method="POST")
            try:
                urllib.request.urlopen(request).close()
                raise AssertionError("configured HTTP JSON limit was not enforced")
            except urllib.error.HTTPError as error:
                assert error.code == 413, error
            stop(hub)
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"))
            until(lambda: cli("workers")[0] == [])
            with urllib.request.urlopen(hub_url + "/health") as response:
                assert json.load(response)["transfer_limits"] == {"websocket": 128 * 1024 * 1024, "http_json": 128 * 1024 * 1024, "metadata": 512 * 1024 * 1024}
            worker = start("worker", "--id", "test-worker", "--data-dir", str(base / "worker"))
            until(lambda: any(w["online"] for w in cli("workers")[0]))
            os_name = "windows" if os.name == "nt" else "macos" if sys.platform == "darwin" else "linux"
            arch = "aarch64" if platform.machine() in ("arm64", "aarch64") else "x86_64"
            common = ["--os", os_name, "--arch", arch]

            empty, _ = cli("run", "--no-input", *common, "--", *noop)
            assert empty["state"] == "succeeded" and not empty["spec"]["inputs"]

            manifest, _ = cli("manifest", "--project")
            assert "ignored.txt" not in {f["path"] for f in manifest["files"]}
            assert all(not f["path"].startswith(".git/") for f in manifest["files"])
            explicit, _ = cli("manifest", "--input", "ignored.txt")
            assert [f["path"] for f in explicit["files"]] == ["ignored.txt"]

            if os.name == "nt":
                compile_command = python("import subprocess; from pathlib import Path; subprocess.run(['rustc', 'main.rs', '-o', 'app.exe'], check=True); Path('answer.txt').write_bytes(subprocess.check_output(['./app.exe'], input=Path('input.txt').read_bytes()))")
                compiler = "rustc=*"
            else:
                compile_command = ["sh", "-c", "cc main.c -o app && ./app < input.txt > answer.txt"]
                compiler = "cc=*"
            first, _ = cli("run", "--project", "--output", "answer.txt", *common, "--tool", compiler, "--", *compile_command)
            assert first["state"] == "succeeded" and result(first) == "43\n"
            assert first["worker_id"] == "test-worker"
            assert not (project / "answer.txt").exists()
            assert hashlib.sha256((base / "results" / first["id"] / "answer.txt").read_bytes()).hexdigest() == first["results"][0]["hash"]
            second, transfer = cli("run", "--project", "--output", "answer.txt", *common, "--", *python(copy_input))
            assert "uploaded 0 missing blobs (0 bytes)" in transfer

            frozen, _ = cli("run", "--input", "input.txt", "--output", "answer.txt", *common, "--detach", "--", *python("import time; time.sleep(2); " + copy_input))
            (project / "input.txt").write_text("99\n")
            assert result(wait(frozen)) == "42\n"
            changed, transfer = cli("run", "--input", "input.txt", "--output", "answer.txt", *common, "--", *python("from pathlib import Path; assert not Path('main.c').exists(); " + copy_input))
            assert result(changed) == "99\n" and "uploaded 1 missing blobs" in transfer
            rejected = cli("run", "--input", "input.txt", "--arch", "impossible-cpu", "--", *noop, ok=False)
            assert "no compatible" in rejected.stderr

            busy, _ = cli("run", "--input", "input.txt", *common, "--detach", "--", *python("import time; time.sleep(3)"))
            queued, _ = cli("run", "--input", "input.txt", *common, "--detach", "--", *noop)
            assert queued["state"] == "queued"
            wait(busy)
            wait(queued)

            cancel, _ = cli("run", "--input", "input.txt", *common, "--detach", "--", *python("import subprocess,sys,time; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); print('childpid='+str(child.pid),flush=True); time.sleep(60)"))
            until(lambda: state(cancel)["state"] == "running")
            child_pid = until(lambda: re.search(r"childpid=(\d+)", "".join(line["text"] for line in cli("jobs", "logs", cancel["id"])[0])))
            cli("jobs", "cancel", cancel["id"])
            until(lambda: state(cancel)["state"] == "cancelled", timeout=15)
            until(lambda: not process_alive(int(child_pid[1])), timeout=15)

            reconnect, _ = cli("run", "--input", "input.txt", "--output", "answer.txt", *common, "--detach", "--", *python("import time; from pathlib import Path; print('start',flush=True); time.sleep(5); Path('answer.txt').write_bytes(b'done')"))
            until(lambda: state(reconnect)["state"] == "running")
            stop(hub)
            time.sleep(1)
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"))
            until(lambda: any(w["online"] for w in cli("workers")[0]))
            resumed = wait(reconnect)
            assert resumed["attempt_id"] == reconnect["attempt_id"] and result(resumed) == "done"
            logs, _ = cli("jobs", "logs", reconnect["id"])
            assert len({line["seq"] for line in logs}) == len(logs)
            assert "".join(line["text"] for line in logs).count("start") == 1

            if os.name == "nt":
                setup = 'New-Item -ItemType Directory -Force -Path "$env:YLCC_ENV_DIR\\bin" | Out-Null; [System.IO.File]::WriteAllText("$env:YLCC_ENV_DIR\\bin\\prepared.cmd", "@echo prepared")'
                verify = "if ((cmd.exe /d /c prepared).Trim() -ne 'prepared') { exit 1 }"
                prepared_command = python("import subprocess; from pathlib import Path; Path('answer.txt').write_bytes(subprocess.check_output(['cmd.exe','/d','/c','prepared']).strip())")
            else:
                setup = 'mkdir -p "$YLCC_ENV_DIR/bin"; printf \'#!/bin/sh\\nprintf prepared\\n\' > "$YLCC_ENV_DIR/bin/prepared"; chmod +x "$YLCC_ENV_DIR/bin/prepared"'
                verify = 'test "$(prepared)" = prepared'
                prepared_command = ["sh", "-c", "prepared > answer.txt"]
            environment = {"id": "test-tools", "revision": "v1", "requirements": {"os": os_name, "arch": arch}, "setup": [setup], "verify": [verify]}
            environment_file = base / "environment.json"
            environment_file.write_text(json.dumps(environment))
            cli("env", "add", str(environment_file))
            prepared, _ = cli("run", "--input", "input.txt", "--output", "answer.txt", *common, "--environment", "test-tools", "--", *prepared_command)
            assert result(prepared) == "prepared"
            statuses, _ = cli("env", "status")
            assert any(s["state"] == "ready" and s["revision"] == "v1" for s in statuses)
            stop(worker)
            worker = start("worker", "--id", "test-worker", "--data-dir", str(base / "worker"))
            until(lambda: any(w["online"] for w in cli("workers")[0]))
            reused, _ = cli("run", "--input", "input.txt", "--output", "answer.txt", *common, "--environment", "test-tools", "--", *prepared_command)
            assert result(reused) == "prepared"

            # A reviewed project task must resolve inputs at its own file, not the caller's cwd.
            project_file = project / "ylcc.yaml"
            project_file.write_text(json.dumps({"version": 1, "tasks": {"copy": {"command": python(copy_input), "inputs": ["input.txt"], "outputs": ["answer.txt"], "requirements": {"os": os_name, "arch": arch}, "priority": 10, "timeout_secs": 30}}}))
            task = subprocess.run(prefix + ["task", "run", "copy", "--file", str(project_file)],
                                  cwd=base, text=True, capture_output=True, timeout=90)
            assert task.returncode == 0, task.stderr
            assert result(json.loads(task.stdout)) == "99\n"

            # Inline environment, defaults, targets and a worker-persistent cache, then save an ad-hoc run.
            repeat = base / "repeat"
            repeat.mkdir()
            counter = "from pathlib import Path; import os; c = Path('cache/count'); c.parent.mkdir(exist_ok=True); n = int(c.read_text()) + 1 if c.exists() else 1; c.write_text(str(n)); Path('answer.txt').write_text(os.environ['GREETING'] + str(n))"
            (repeat / "ylcc.yaml").write_text(
                "version: 1\n"
                "environments:\n  inline-tools: {setup: ['exit 0'], verify: ['exit 0'], env: {GREETING: inline}}\n"
                f"defaults:\n  requirements: {json.dumps({'os': os_name, 'arch': arch})}\n  cache: [cache]\n  environment: inline-tools\n"
                f"tasks:\n  count:\n    command: {json.dumps(python(counter))}\n    no_input: true\n    outputs: [answer.txt]\n    targets: [{{}}, {{}}]\n")
            def repeat_cli(*args):
                done = subprocess.run(prefix + list(args), cwd=repeat, text=True, capture_output=True, timeout=120)
                assert done.returncode == 0, done.stderr
                return json.loads(done.stdout), done.stderr
            counted, _ = repeat_cli("task", "run", "count")
            assert sorted(result(job) for job in counted) == ["inline1", "inline2"], counted
            counted_again, _ = repeat_cli("task", "run", "count")
            assert sorted(result(job) for job in counted_again) == ["inline3", "inline4"]
            # Defaults (environment, cache) would change this run, so saving is refused before submission.
            refused = subprocess.run(prefix + ["run", "--no-input", *common, "--save-task", "noop", "--", *noop],
                                     cwd=repeat, text=True, capture_output=True, timeout=60)
            assert refused.returncode != 0 and "defaults would change" in refused.stderr, refused.stderr
            fresh = base / "fresh"
            fresh.mkdir()
            (fresh / "input.txt").write_text("saved\n")
            done = subprocess.run(prefix + ["run", "--input", "./input.txt", "--output", "answer.txt", *common,
                                  "--save-task", "copy", "--", *python(copy_input)], cwd=fresh, text=True, capture_output=True, timeout=120)
            assert done.returncode == 0 and "saved task copy" in done.stderr, done.stderr
            rerun = subprocess.run(prefix + ["task", "run", "copy"], cwd=fresh, text=True, capture_output=True, timeout=120)
            assert rerun.returncode == 0, rerun.stderr
            assert result(json.loads(rerun.stdout)) == "saved\n"

            paused, _ = cli("workers", "pause", "test-worker")
            assert paused["paused"]
            low, _ = cli("run", "--no-input", *common, "--detach", "--", *noop)
            high, _ = cli("run", "--no-input", *common, "--detach", "--", *python("import time; time.sleep(2)"))
            reprioritized, _ = cli("jobs", "priority", high["id"], "20")
            assert reprioritized["spec"]["priority"] == 20
            assert state(low)["state"] == state(high)["state"] == "queued"
            cli("workers", "limit", "test-worker", "--max-jobs", "2", ok=False)
            # Both Hub restart and worker reconnect must retain the operator's pause.
            stop(worker)
            stop(hub)
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"))
            until(lambda: cli("workers")[0])
            worker = start("worker", "--id", "test-worker", "--data-dir", str(base / "worker"), "--max-jobs", "2")
            until(lambda: any(w["online"] and w["paused"] for w in cli("workers")[0]))
            limited, _ = cli("workers", "limit", "test-worker", "--max-jobs", "1")
            assert limited["max_jobs"] == 1
            cli("workers", "resume", "test-worker")
            until(lambda: state(high)["state"] == "running")
            assert state(low)["state"] == "queued"
            cli("jobs", "priority", high["id"], "30", ok=False)
            wait(high)
            wait(low)
            cli("workers", "limit", "test-worker", "--max-jobs", "2")
            parallel_a, _ = cli("run", "--no-input", *common, "--detach", "--", *python("import time; time.sleep(3)"))
            parallel_b, _ = cli("run", "--no-input", *common, "--detach", "--", *python("import time; time.sleep(3)"))
            until(lambda: state(parallel_a)["state"] == state(parallel_b)["state"] == "running")
            cli("workers", "limit", "test-worker", "--max-jobs", "1")
            assert state(parallel_a)["state"] == state(parallel_b)["state"] == "running"
            wait(parallel_a)
            wait(parallel_b)

            # Scoped tokens: a requester runs work but cannot administer, a worker token serves only
            # its own id, revocation takes effect at once, and the audit log records all of it.
            laptop_token = base / "laptop.token"
            created, _ = cli("tokens", "create", "--name", "laptop", "--role", "requester", "--out", str(laptop_token))
            assert created["token"]["role"] == "requester" and "secret" not in created
            assert laptop_token.read_text().strip() not in json.dumps(created)
            second_token = base / "second.token"
            cli("tokens", "create", "--name", "second", "--role", "worker", "--worker-id", "second-worker", "--out", str(second_token))
            cli("tokens", "create", "--name", "dup", "--role", "requester", "--out", str(laptop_token), ok=False)
            submitted, _ = cli("run", "--no-input", *common, "--", *noop, token_file=laptop_token)
            assert submitted["state"] == "succeeded" and submitted["submitted_by"] == "laptop"
            denied = cli("workers", "delete", "test-worker", ok=False, token_file=laptop_token)
            assert "403" in denied.stderr, denied.stderr
            cli("tokens", "list", ok=False, token_file=laptop_token)
            second = start("worker", "--id", "second-worker", "--data-dir", str(base / "worker2"), token_file=second_token)
            until(lambda: any(w["id"] == "second-worker" and w["online"] for w in cli("workers")[0]))
            impostor = start("worker", "--id", "impostor", "--data-dir", str(base / "worker3"), token_file=second_token)
            until(lambda: any(e["action"] == "worker.rejected" and e["target"] == "impostor" for e in cli("audit")[0]))
            assert not any(w["id"] == "impostor" for w in cli("workers")[0])
            stop(impostor)
            # Large metadata uses the same scoped worker token and streaming blob path as real jobs.
            headers = {"Authorization": "Bearer " + token.read_text().strip(), "Content-Type": "application/json"}
            def api(method, path, value=None):
                body = None if value is None else json.dumps(value).encode()
                request = urllib.request.Request(hub_url + path, data=body, headers=headers, method=method)
                with urllib.request.urlopen(request, timeout=60) as response:
                    return json.load(response)
            huge_spec = {"command": python("raise SystemExit(23)"), "requirements": {"os": os_name, "arch": arch, "worker": "second-worker"}, "env": {"LARGE_METADATA": "x" * (65 * 1024 * 1024)}}
            huge_run = api("POST", "/v1/jobs", huge_spec)
            def huge_outcome():
                current = state(huge_run)
                return current if current["state"] in {"succeeded", "failed", "cancelled", "unknown"} else None
            outcome = until(huge_outcome)
            assert outcome["state"] == "failed", {"state": outcome["state"], "error": (outcome.get("error") or "")[:500]}
            huge_journal = base / "worker2" / "attempts" / huge_run["attempt_id"]
            assert len(json.loads((huge_journal / "job.json").read_text())["env"]["LARGE_METADATA"]) == 65 * 1024 * 1024
            assert json.loads((huge_journal / "journal.json").read_text()).get("delivery") is None
            # Reconcile a >2 MiB successful file manifest and a >16 MiB failure without aggregate Hello.
            large, _ = cli("run", "--no-input", *common, "--worker", "second-worker", "--output", "out", "--", *python("from pathlib import Path; Path('out').mkdir()"))
            failure, _ = cli("run", "--no-input", *common, "--worker", "second-worker", "--", *noop)
            empty_hash = hashlib.sha256(b"").hexdigest()
            with urllib.request.urlopen(urllib.request.Request(hub_url + "/v1/blobs/" + empty_hash, data=b"", headers=headers, method="PUT")) as response:
                assert response.status == 201
            # Keep a deleted environment cache while the worker is offline.
            stale = {"worker_id": "second-worker", "environment_id": "test-tools", "revision": "v1", "state": "ready", "error": None}
            stop(second)
            stale_dir = base / "worker2" / "environments" / "test-tools" / "v1"
            stale_dir.mkdir(parents=True, exist_ok=True)
            (stale_dir / "status.json").write_text(json.dumps(stale))
            stop(worker)
            cli("env", "delete", "test-tools")
            stop(hub)
            results = [{"path": f"out/{i:05d}", "hash": empty_hash, "size": 0, "mode": 420} for i in range(20000)]
            assert len(json.dumps(results)) > 2 * 1024 * 1024
            database = sqlite3.connect(base / "hub" / "hub.sqlite")
            for job, report_state, output, error in [(large, "succeeded", results, None), (failure, "failed", [], "x" * (17 * 1024 * 1024))]:
                journal_path = base / "worker2" / "attempts" / job["attempt_id"] / "journal.json"
                journal = json.loads(journal_path.read_text())
                journal["report"].update(state=report_state, results=output, error=error, exit_code=0 if report_state == "succeeded" else 1)
                journal["seq"] = 9  # A failed append can leave the counter beyond readable logs.
                journal_path.write_text(json.dumps(journal))
                stored = json.loads(database.execute("SELECT data FROM jobs WHERE id=?", (job["id"],)).fetchone()[0])
                stored.update(state="unknown", results=[], error=None, exit_code=None)
                database.execute("UPDATE jobs SET data=? WHERE id=?", (json.dumps(stored), job["id"]))
            database.commit()
            database.close()
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"))
            until(lambda: cli("workers")[0])
            second = start("worker", "--id", "second-worker", "--data-dir", str(base / "worker2"), token_file=second_token)
            until(lambda: state(large)["state"] == "succeeded", timeout=60)
            recovered = state(large)
            assert recovered["results"] == results
            until(lambda: state(failure)["state"] == "failed", timeout=60)
            assert len(state(failure)["error"]) == 17 * 1024 * 1024
            next_job, _ = cli("run", "--no-input", *common, "--worker", "second-worker", "--", *noop)
            assert next_job["state"] == "succeeded"
            worker = start("worker", "--id", "test-worker", "--data-dir", str(base / "worker"), "--max-jobs", "2")
            until(lambda: any(w["id"] == "test-worker" and w["online"] for w in cli("workers")[0]))

            tokens, _ = cli("tokens", "list")
            ids = {t["name"]: t["id"] for t in tokens}
            revoked, _ = cli("tokens", "revoke", ids["second"])
            assert revoked["revoked"]["revoked_at"]
            until(lambda: not next(w for w in cli("workers")[0] if w["id"] == "second-worker")["online"], timeout=15)
            cli("tokens", "revoke", ids["laptop"])
            gone = cli("jobs", "list", ok=False, token_file=laptop_token)
            assert "401" in gone.stderr, gone.stderr
            stop(second)
            events, _ = cli("audit", "--limit", "500")
            actions = {e["action"] for e in events}
            assert {"token.create", "token.revoke", "job.submit", "job.priority", "worker.control", "worker.connect", "worker.rejected", "environment.upsert"} <= actions, actions
            assert any(e["action"] == "job.submit" and e["actor"] == "laptop" and e["target"] == submitted["id"] for e in events)

            # Differing receiver budgets must fail only the operation, not enter reconnect/replay loops.
            mismatch, _ = cli("run", "--no-input", *common, "--worker", "test-worker", "--", *noop)
            stop(worker)
            stop(hub)
            mismatch_journal = base / "worker" / "attempts" / mismatch["attempt_id"] / "journal.json"
            journal = json.loads(mismatch_journal.read_text())
            journal["report"].update(state="failed", error="x" * (2 * 1024 * 1024), exit_code=1)
            mismatch_journal.write_text(json.dumps(journal))
            database = sqlite3.connect(base / "hub" / "hub.sqlite")
            stored = json.loads(database.execute("SELECT data FROM jobs WHERE id=?", (mismatch["id"],)).fetchone()[0])
            stored.update(state="unknown", error=None, exit_code=None)
            database.execute("UPDATE jobs SET data=? WHERE id=?", (json.dumps(stored), mismatch["id"]))
            database.commit()
            database.close()
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"), env={"YLCC_METADATA_MAX_MIB": "1"})
            until(lambda: cli("workers")[0])
            worker = start("worker", "--id", "test-worker", "--data-dir", str(base / "worker"), env={"YLCC_METADATA_MAX_MIB": "4"})
            until(lambda: state(mismatch)["state"] == "failed")
            assert "1 MiB" in state(mismatch)["error"]
            after_mismatch, _ = cli("run", "--no-input", *common, "--worker", "test-worker", "--", *noop)
            assert after_mismatch["state"] == "succeeded"
            stop(worker)
            stop(hub)
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"))
            until(lambda: cli("workers")[0])
            worker = start("worker", "--id", "test-worker", "--data-dir", str(base / "worker"), env={"YLCC_METADATA_MAX_MIB": "1"})
            until(lambda: any(w["id"] == "test-worker" and w["online"] for w in cli("workers")[0]))
            limit_environment = base / "limit-environment.json"
            limit_environment.write_text(json.dumps({"id": "limit-tools", "revision": "v1", "requirements": {"os": os_name, "arch": arch}, "env": {"PAD": "x" * (2 * 1024 * 1024)}}))
            cli("env", "add", str(limit_environment))
            cli("env", "prepare", "limit-tools", "--worker", "test-worker")
            failed_prepare = until(lambda: next((s for s in cli("env", "status")[0] if s["environment_id"] == "limit-tools" and s["state"] == "failed"), None))
            assert "1 MiB" in failed_prepare["error"]
            too_large = api("POST", "/v1/jobs", {"command": noop, "requirements": {"os": os_name, "arch": arch, "worker": "test-worker"}, "env": {"PAD": "x" * (2 * 1024 * 1024)}})
            until(lambda: state(too_large)["state"] == "failed")
            assert "1 MiB" in state(too_large)["error"]
            after_mismatch, _ = cli("run", "--no-input", *common, "--worker", "test-worker", "--", *noop)
            assert after_mismatch["state"] == "succeeded"
            cli("env", "delete", "limit-tools")

            # Command policy (last: it removes the unrestricted worker). A policy worker refuses
            # anything outside its allowlist, and the Hub rejects such jobs at submission.
            stop(worker)
            until(lambda: not next(w for w in cli("workers")[0] if w["id"] == "test-worker")["online"], timeout=15)
            for registered in ("test-worker", "second-worker"):
                cli("workers", "delete", registered)
            policy_dir = base / "policy"
            policy_dir.mkdir(mode=0o700)
            policy_file = policy_dir / "policy.yaml"
            policy_file.write_text(f"version: 1\ncommands: [{json.dumps([sys.executable, '-c', 'pass'])}]\nenv: []\n")
            if os.name != "nt":
                policy_dir.chmod(0o700)
                policy_file.chmod(0o600)
            checked, _ = cli("policy", "check", str(policy_file))
            assert checked["categories"]["commands"] == "restricted" and checked["categories"]["containers"].startswith("unrestricted")
            assert any("interpreter" in warning for warning in checked["warnings"]), checked
            strict = start("worker", "--id", "strict-worker", "--data-dir", str(base / "worker-strict"), "--policy", str(policy_file))
            until(lambda: any(w["id"] == "strict-worker" and w["online"] for w in cli("workers")[0]))
            advertised = next(w for w in cli("workers")[0] if w["id"] == "strict-worker")["capabilities"]["policy"]
            assert advertised["commands"] == [[sys.executable, "-c", "pass"]] and advertised["warnings"]
            allowed, _ = cli("run", "--no-input", *common, "--", *noop)
            assert allowed["state"] == "succeeded" and allowed["worker_id"] == "strict-worker"
            refused = cli("run", "--no-input", *common, "--", *python("print('not allowed')"), ok=False)
            assert "not allowed by worker policy" in refused.stderr, refused.stderr
            refused_env = cli("run", "--no-input", *common, "--env", "FOO=1", "--", *noop, ok=False)
            assert "FOO is not allowed" in refused_env.stderr, refused_env.stderr
            stop(strict)
            print("PASS: native compile, snapshots, dedup, isolation, cancellation, reconnect, persistent environments, YAML task, priority, pause/resume, live concurrency, scoped tokens, streamed Run/results, deleted-environment reconnect, audit, command policy")
        except BaseException:
            for stream in streams:
                stream.flush()
                print(f"--- {stream.name} ---", file=sys.stderr)
                print(Path(stream.name).read_text()[-12000:], file=sys.stderr)
            raise
        finally:
            for process in reversed(processes):
                stop(process)
            for stream in streams:
                stream.close()


if __name__ == "__main__":
    main()
