#!/usr/bin/env python3
"""Exercise signed Linux updates using two prebuilt debug binary directories.

Run inside Linux, with a session D-Bus and ALSA null device available. Inputs must
contain requiem, requiemd and requiem-gui, built with distinct versions from the
same updater implementation. No GitHub or installed user configuration is used.
"""
import argparse
import contextlib
import functools
import http.server
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import struct
import subprocess
import tarfile
import tempfile
import threading
import time

NAMES = ("requiem", "requiemd", "requiem-gui")


def run(argv, *, env=None, data=None, timeout=120):
    result = subprocess.run([str(a) for a in argv], input=data, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            env=env, timeout=timeout)
    print("$", " ".join(str(a) for a in argv), flush=True)
    print(result.stdout, end="", flush=True)
    if result.returncode:
        raise AssertionError(f"command exited {result.returncode}")
    return result.stdout


def version(directory):
    versions = []
    for name in NAMES:
        output = run([directory / name, "--version"]).strip()
        prefix = name + " "
        assert output.startswith(prefix), output
        versions.append(output[len(prefix):])
    assert len(set(versions)) == 1, versions
    return versions[0]


def eventually(check, timeout=45):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            answer = check()
            if answer:
                return answer
        except (OSError, ValueError, AssertionError, subprocess.SubprocessError) as error:
            last = error
        time.sleep(.2)
    raise AssertionError(f"condition timed out: {last}")


def statuses(binary, env):
    output = run([binary, "--json", "update"], env=env, timeout=10)
    return [row for line in output.splitlines()
            if line.startswith("{") and (row := json.loads(line)).get("type") == "update"]


def set_auto(socket_path, enabled, protocol, version_text):
    def send(stream, msg):
        body = json.dumps(msg).encode()
        stream.sendall(struct.pack(">I", len(body)) + body)

    def read_exact(stream, size):
        result = b""
        while len(result) < size:
            piece = stream.recv(size - len(result))
            if not piece:
                raise EOFError("daemon closed IPC")
            result += piece
        return result

    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(10)
        stream.connect(str(socket_path))
        send(stream, {"Hello": {"protocol": protocol, "role": "Cli", "version": version_text}})
        send(stream, {"SetAutoUpdate": enabled})
        for _ in range(200):
            length, = struct.unpack(">I", read_exact(stream, 4))
            assert length <= 4 * 1024 * 1024
            row = json.loads(read_exact(stream, length))
            if row.get("Update", {}).get("auto") is enabled:
                return
        raise AssertionError("automatic setting was not acknowledged")


def gui_snapshot(install, env, output):
    assert env.get("DISPLAY"), "--gui-snapshot requires Xvfb/DISPLAY"
    assert shutil.which("import"), "--gui-snapshot requires ImageMagick import"
    output.parent.mkdir(parents=True, exist_ok=True)
    with (output.parent / (output.stem + "-gui.log")).open("w") as log:
        gui = subprocess.Popen([str(install / "requiem-gui"), "--view", "settings"],
                               env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            def painted():
                assert gui.poll() is None, "GUI exited before screenshot"
                run(["import", "-window", "root", output], env=env, timeout=15)
                mean = float(run(["identify", "-format", "%[fx:mean]", output], env=env))
                return output.stat().st_size > 1000 and mean > .005
            time.sleep(2)
            eventually(painted, timeout=15)
        finally:
            if gui.poll() is None:
                gui.terminate()
                try:
                    gui.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    gui.kill()
                    gui.wait()


def case(root, old, new, old_version, new_version, base_env, failure, protocol, gui_dir, automatic=False):
    case_dir = root / ("automatic" if automatic else "rollback" if failure else "install")
    case_dir.mkdir(mode=0o700)
    install = case_dir / "bin"
    install.mkdir(mode=0o700)
    for name in NAMES:
        shutil.copy2(old / name, install / name)
    (install / "unrelated.txt").write_text("must survive\n")
    runtime = case_dir / "ipc"
    runtime.mkdir(mode=0o700)
    state = case_dir / "player.json"
    state.write_text(json.dumps({"version": 1, "volume": 37, "resume": None, "queue": []}))
    env = dict(base_env, REQUIEM_SOCKET_DIR=str(runtime),
               REQUIEM_STATE_FILE=str(state),
               REQUIEM_UPDATE_STATE_FILE=str(case_dir / "update.json"),
               REQUIEM_IDLE_EXIT_SECS="600", HOME=str(case_dir),
               XDG_CONFIG_HOME=str(case_dir / "config"),
               XDG_STATE_HOME=str(case_dir / "state"),
               XDG_CACHE_HOME=str(case_dir / "cache"))
    if failure:
        env["REQUIEM_TEST_FAIL_START_VERSION"] = new_version
    log_path = root / (case_dir.name + "-daemon.log")
    with log_path.open("w") as log:
        daemon = subprocess.Popen([str(install / "requiemd")], env=env,
                                  stdout=log, stderr=subprocess.STDOUT)
        try:
            def current(expected):
                rows = statuses(install / "requiem", env)
                return rows and rows[-1]["current"] == expected and rows[-1]
            eventually(lambda: current(old_version))
            set_auto(runtime / "daemon.sock", False, protocol, old_version)
            eventually(lambda: not current(old_version)["auto"])
            run([install / "requiem", "--connect", "--script", "-"], env=env,
                data="shutdown-daemon\n", timeout=15)
            daemon.wait(timeout=20)
            daemon = subprocess.Popen([str(install / "requiemd")], env=env,
                                      stdout=log, stderr=subprocess.STDOUT)
            restarted = eventually(lambda: current(old_version))
            assert restarted["auto"] is False, restarted
            if automatic:
                set_auto(runtime / "daemon.sock", True, protocol, old_version)
                def automatic_ready():
                    rows = statuses(install / "requiem", env)
                    return rows and rows[-1]["state"] == "ready" and rows[-1]
                ready = eventually(automatic_ready, timeout=50)
            else:
                run([install / "requiem", "--json", "update", "check"], env=env)
                ready = statuses(install / "requiem", env)[-1]
            assert ready["state"] == "ready" and ready["version"] == new_version, ready
            if gui_dir:
                gui_snapshot(install, env, gui_dir / (case_dir.name + "-ready.png"))
            if automatic:
                run([install / "requiem", "--connect", "--script", "-"], env=env,
                    data="shutdown-daemon\n", timeout=15)
            else:
                run([install / "requiem", "--json", "update", "install"], env=env)
            expected = old_version if failure else new_version
            final = eventually(lambda: current(expected), timeout=60)
            assert final["auto"] is automatic, final
            assert version(install) == expected
            if gui_dir:
                gui_snapshot(install, env, gui_dir / (case_dir.name + "-final.png"))
            assert (install / "unrelated.txt").read_text() == "must survive\n"
            assert json.loads(state.read_text())["volume"] == 37
            persisted = json.loads((case_dir / "update.json").read_text())
            assert persisted["auto"] is automatic
            if failure:
                log.flush()
                logs = log_path.read_text()
                assert "REQUIEM_TEST_FAIL_START_VERSION" in logs or "rollback" in logs.lower(), logs
            print(f"PASS {case_dir.name}: three binaries {expected}; IPC healthy; volume and auto retained", flush=True)
        finally:
            try:
                run([install / "requiem", "--connect", "--script", "-"], env=env,
                    data="shutdown-daemon\n", timeout=15)
            except Exception as error:
                print(f"cleanup: {error}", flush=True)
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()
            print(log_path.read_text(), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old-bin", required=True, type=Path)
    parser.add_argument("--new-bin", required=True, type=Path)
    parser.add_argument("--release-tool", required=True, type=Path)
    parser.add_argument("--protocol", type=int, default=9)
    parser.add_argument("--gui-snapshot", type=Path, help="capture connected Settings into this directory; run under Xvfb")
    parser.add_argument("--keep-fixtures", action="store_true", help="retain isolated installs and print their path for live GUI checks")
    parser.add_argument("--logs", type=Path, default=Path("/tmp/requiem-linux-live-update"))
    args = parser.parse_args()
    assert platform.system() == "Linux", "run this harness inside Linux"
    old, new, tool = args.old_bin.resolve(), args.new_bin.resolve(), args.release_tool.resolve()
    old_version, new_version = version(old), version(new)
    assert old_version != new_version, "two distinct real builds are required"
    args.logs.mkdir(parents=True, exist_ok=True)
    fixture = (contextlib.nullcontext(tempfile.mkdtemp(prefix="rq-live-", dir="/tmp"))
               if args.keep_fixtures else tempfile.TemporaryDirectory(prefix="rq-live-", dir="/tmp"))
    with fixture as temp:
        root = Path(temp)
        if args.keep_fixtures:
            print(f"Retained fixtures: {root}", flush=True)
        key = root / "disposable.key"
        public = run([tool, "keygen", "--out", key]).strip().splitlines()[-1]
        releases = root / "releases"
        arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine(), platform.machine())
        name = f"requiem-{new_version}-linux-{arch}"
        dist = releases / "download" / ("v" + new_version)
        dist.mkdir(parents=True)
        archive = dist / (name + ".tar.gz")
        with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as tar:
            for binary in NAMES:
                info = tar.gettarinfo(str(new / binary), arcname=name + "/" + binary)
                info.mtime = int(info.mtime)
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                with (new / binary).open("rb") as stream:
                    tar.addfile(info, stream)
        run([tool, "linux-manifest", "--archive", archive, "--version", new_version, "--arch", arch])
        manifest = dist / f"requiem-update-linux-{arch}.json"
        run([tool, "sign", "--key", key, "--in", manifest])
        run([tool, "linux-verify", "--dir", dist, "--arch", arch, "--pubkey", public])
        latest = releases / "latest" / "download"
        latest.mkdir(parents=True)
        for suffix in ("", ".sig"):
            shutil.copy2(str(manifest) + suffix, latest / (manifest.name + suffix))
        handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(root))
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        env = dict({k: v for k, v in os.environ.items() if not k.startswith("REQUIEM_")}, REQUIEM_UPDATE_BASE=f"http://127.0.0.1:{server.server_port}/releases",
                   REQUIEM_UPDATE_PUBKEY=public)
        try:
            for failure in (False, True):
                case(root, old, new, old_version, new_version, env, failure, args.protocol, args.gui_snapshot.resolve() if args.gui_snapshot else None)
            case(root, old, new, old_version, new_version, env, False, args.protocol,
                 args.gui_snapshot.resolve() if args.gui_snapshot else None, automatic=True)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
            for path in root.glob("*-daemon.log"):
                shutil.copy2(path, args.logs / path.name)
        print("PASS signed Linux update and failed-start rollback", flush=True)


if __name__ == "__main__":
    main()
