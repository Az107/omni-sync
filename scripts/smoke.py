"""Isolated Linux two-daemon release smoke test; never uses real user state.

Build both binaries first, then: python3 scripts/smoke.py [--bin-dir target/debug]
All files/logs are retained under /tmp/opencode for diagnosis.
"""

import argparse
import base64
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import tomllib
import urllib.error
import urllib.request


def wait_for(description, predicate, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {description}")


def encoded(path):
    return base64.b64encode(path.encode()).decode()


class Smoke:
    def __init__(self, binaries, root):
        self.binaries, self.root = binaries, root
        self.processes, self.logs = [], []
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("OMNI_SYNC_")}
        self.env.update(
            HOME=str(root / "home"),
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_STATE_HOME=str(root / "state"),
            XDG_DATA_HOME=str(root / "data"),
            RUST_LOG="info",
        )
        # The brief bind/release reserves an ephemeral candidate; server startup
        # still fails explicitly if another process happens to take it first.
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        self.base = f"http://127.0.0.1:{port}"
        self.server_config = root / "server.toml"
        self.server_config.write_text(
            f'addr = "127.0.0.1:{port}"\n'
            f'data_dir = {json.dumps(str(root / "server-data"))}\n'
            f'state_dir = {json.dumps(str(root / "server-state"))}\n'
            'dashboard_password_hash = ""\n'
        )

    def command(self, who, *args):
        binary = "omni-sync-server" if who == "server" else "omni-sync"
        return [str(self.binaries / binary), "--config", str(self.root / f"{who}.toml"), *map(str, args)]

    def run(self, who, *args):
        result = subprocess.run(
            self.command(who, *args), env=self.env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=120,
        )
        if result.returncode:
            raise RuntimeError(f"{who} {' '.join(map(str, args[:1]))} failed: {result.stderr}")
        return result.stdout.strip()

    def start(self, who, *args):
        log = (self.root / f"{who}-{len(self.logs)}.log").open("w")
        self.logs.append(log)
        process = subprocess.Popen(
            self.command(who, *args), env=self.env, stdin=subprocess.DEVNULL,
            stdout=log, stderr=subprocess.STDOUT,
        )
        self.processes.append(process)
        return process

    def stop(self, process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise RuntimeError("process did not shut down within 10 seconds")

    def close(self):
        try:
            for process in reversed(self.processes):
                self.stop(process)
        finally:
            for log in self.logs:
                log.close()

    def api(self, path, payload=None, who="a", method=None):
        with (self.root / f"{who}.toml").open("rb") as source:
            token = tomllib.load(source)["token"]
        data = None if payload is None else json.dumps(payload).encode()
        request = urllib.request.Request(
            self.base + "/api/v1" + path, data=data, method=method,
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=10) as response:
            return json.load(response)

    def healthy(self):
        try:
            with urllib.request.urlopen(self.base + "/healthz", timeout=1) as response:
                return json.load(response)["ok"]
        except (OSError, urllib.error.URLError):
            return False

    def daemon(self, who):
        process = self.start(who, "daemon")
        wait_for(f"{who} daemon control socket", lambda: (self.root / f"{who}.daemon.ctl").exists())
        return process

    def same(self, who, relative, content):
        path = self.root / who / relative
        return path.is_file() and path.read_bytes() == content

    def exercise(self):
        self.start("server", "serve")
        wait_for("server health", self.healthy)
        for who in ("a", "b", "c"):
            (self.root / who).mkdir()
            code = self.run("server", "pair").splitlines()[0]
            self.run(who, "enroll", "--server", self.base, "--code", code, "--name", f"smoke-{who}")
        folder = self.run("a", "folder", "create", "Docs")
        self.run("a", "add", folder, self.root / "a")
        self.run("b", "add", folder, self.root / "b")
        a, b = self.daemon("a"), self.daemon("b")

        first = bytes(range(256)) * 8192 + b"first"
        (self.root / "a" / "binary.dat").write_bytes(first)
        wait_for("binary create A -> B", lambda: self.same("b", "binary.dat", first))
        first_version = self.api(f"/folders/{folder}/history?path={encoded('binary.dat')}")["entries"][0]["version_id"]
        second = b"bidirectional\x00\xff" + first[::-1]
        (self.root / "b" / "binary.dat").write_bytes(second)
        wait_for("binary edit B -> A", lambda: self.same("a", "binary.dat", second))
        self.run("a", "resolve", folder, "binary.dat", "--to", first_version)
        wait_for("CLI rollback", lambda: all(self.same(w, "binary.dat", first) for w in ("a", "b")))
        (self.root / "a" / "binary.dat").unlink()
        wait_for("binary delete", lambda: not (self.root / "b" / "binary.dat").exists())
        print("PASS binary create/edit/delete and CLI rollback", flush=True)

        (self.root / "a" / "offline.txt").write_bytes(b"baseline")
        wait_for("offline baseline", lambda: self.same("b", "offline.txt", b"baseline"))
        self.stop(a)
        self.stop(b)
        (self.root / "a" / "offline.txt").write_bytes(b"offline-a")
        (self.root / "b" / "offline.txt").write_bytes(b"offline-b")
        self.run("a", "flush")
        self.run("b", "flush")
        a, b = self.daemon("a"), self.daemon("b")
        wait_for("offline convergence", lambda: self.same("a", "offline.txt", b"offline-b") and self.same("b", "offline.txt", b"offline-b"))
        rows = self.api("/overwrites")
        assert len([row for row in rows if row["path"] == encoded("offline.txt")]) == 1, rows
        assert "via running daemon" in self.run("a", "flush")
        duplicate = subprocess.run(self.command("a", "daemon"), env=self.env, capture_output=True, text=True, timeout=10)
        assert duplicate.returncode != 0 and "holds the lock" in duplicate.stderr, duplicate.stderr
        print("PASS offline conflict, control handoff and exclusive daemon ownership", flush=True)

        # Add folders while daemons are already running: exercise config reload.
        other = self.run("a", "folder", "create", "Hot")
        (self.root / "a-hot").mkdir()
        (self.root / "b-hot").mkdir()
        self.run("a", "add", other, self.root / "a-hot", "--mode", "on_close")
        self.run("b", "add", other, self.root / "b-hot")
        held = self.root / "a-hot" / "held.txt"
        with held.open("wb") as writer:
            writer.write(b"held until close")
            writer.flush()
            wait_for("on_close hold recorded", lambda: "held_by_pid" in self.run("a", "status"))
            assert not (self.root / "b-hot" / "held.txt").exists()
        wait_for("on_close release", lambda: (self.root / "b-hot" / "held.txt").read_bytes() == b"held until close" if (self.root / "b-hot" / "held.txt").exists() else False)
        print("PASS live folder reload and on_close hold/release", flush=True)

        # More than one 500-entry page, with a pre-existing matching local file
        # on the final page. It must be adopted, not falsely uploaded.
        paged = self.run("a", "folder", "create", "Paged")
        ops = [{"kind": "put", "path": encoded(f"page/{index:04}.txt")} for index in range(510)]
        seeded = self.api(f"/folders/{paged}/ops", {"ops": ops})["applied"][-1]["seq"]
        local = self.root / "c" / "page"
        local.mkdir(parents=True)
        (local / "0509.txt").write_bytes(b"")
        self.run("c", "add", paged, self.root / "c")
        assert len(list(local.iterdir())) == 510
        assert self.api(f"/folders/{paged}/head")["head"] == seeded, "bootstrap generated unexpected uploads"
        assert not any(row.get("folder_id") == paged for row in self.api("/overwrites"))
        print("PASS paginated bootstrap and existing-file adoption", flush=True)

        history = self.run("a", "folder", "create", "HistoricalConflict")
        self.api(f"/folders/{history}/ops", {"ops": [
            {"kind": kind, "path": encoded("note.txt")}
            for kind in ("put", "delete", "put")
        ]})
        conflicted = self.root / "c-history"
        conflicted.mkdir()
        content = b"unique pre-existing local content"
        (conflicted / "note.txt").write_bytes(content)
        self.run("c", "add", history, conflicted)
        assert (conflicted / "note.txt").read_bytes() == content
        manifest = self.api(f"/folders/{history}/manifest")["entries"]
        assert len(manifest) == 1 and manifest[0]["size"] == len(content)
        print("PASS pre-existing conflict survives historical puts and deletes", flush=True)

        # An unauthenticated idle local peer must not block SIGTERM cleanup.
        control = json.loads((self.root / "a.daemon.ctl").read_text())
        host, port = control["addr"].rsplit(":", 1)
        with socket.create_connection((host, int(port)), timeout=5):
            self.stop(a)
        assert not (self.root / "a.daemon.ctl").exists()
        self.stop(b)
        print("PASS shutdown with idle control connection", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    for name in ("omni-sync", "omni-sync-server"):
        if not (binaries / name).is_file():
            parser.error(f"build {binaries / name} first")
    Path("/tmp/opencode").mkdir(exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="omni-release-smoke-", dir="/tmp/opencode"))
    root.chmod(0o700)
    print(f"Isolated smoke state and logs: {root}", flush=True)
    smoke = Smoke(binaries, root)
    try:
        smoke.exercise()
    finally:
        smoke.close()


if __name__ == "__main__":
    main()
