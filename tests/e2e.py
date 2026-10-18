#!/usr/bin/env python3
"""Real hub/worker/CLI contract test; uses only disposable local data."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
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
        prefix = [binary, "--hub", hub_url, "--token-file", str(token), "--results-dir", str(base / "results"), "--json"]
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

        def start(role, *args):
            stream = (base / f"{role}-{len(streams)}.log").open("w+")
            streams.append(stream)
            process = subprocess.Popen(prefix + [role, *args], stdout=stream, stderr=stream, start_new_session=os.name != "nt")
            processes.append(process)
            return process

        def cli(*args, ok=True, timeout=90):
            result = subprocess.run(prefix + list(args), cwd=project, text=True, capture_output=True, timeout=timeout)
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
            hub = start("hub", "--listen", f"127.0.0.1:{port}", "--data-dir", str(base / "hub"))
            until(lambda: cli("workers")[0] == [])
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
            print("PASS: native compile, snapshots, dedup, isolation, cancellation, reconnect, persistent environments, YAML task, priority, pause/resume, live concurrency")
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
