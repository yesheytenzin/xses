#!/usr/bin/env python3
"""Optional real-agent verification in disposable stores (no model requests).
Requires installed pi/opencode/codex CLIs and initialized local Codex schemas.
Only schemas and migration metadata are read from local Codex stores, never session rows.
"""
import json
import base64
import re
import os
from pathlib import Path
import queue
import threading
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/xses").resolve())
root = Path(tempfile.mkdtemp(prefix="xses-native-"))
home = root / "home"
project = root / "project"
project.mkdir()
(home / ".codex").mkdir(parents=True)
env = os.environ.copy()
env.update(HOME=str(home), CODEX_HOME=str(home / ".codex"), XDG_DATA_HOME=str(root / "data"),
           XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"),
           OPENCODE_DATA_DIR=str(root / "data/opencode"), OPencode_DATA_DIR=str(root / "data/opencode"),
           PI_CODING_AGENT_DIR=str(home / ".pi/agent"), PI_OFFLINE="1", PI_TELEMETRY="0", XSES_NO_PROGRESS="1")
log = []
codex_binary = os.environ.get("XSES_VERIFY_CODEX", "codex")
if Path(shutil.which(codex_binary) or codex_binary).resolve().name == "mise":
    codex_binary = subprocess.check_output(["mise", "which", "codex"], text=True).strip()

def run(argv):
    p = subprocess.run(argv, env=env, cwd=project, text=True, capture_output=True, timeout=60)
    log.append(dict(argv=argv, status=p.returncode, stdout=p.stdout, stderr=p.stderr))
    if p.returncode:
        raise RuntimeError(log[-1])
    return p.stdout


def records():
    return [(key, json.loads(paths)) for key, paths in (line.split("\t", 1) for line in (root / "data/xses/index.jsonl").read_text().splitlines())]


try:
    # Recreate installed database layouts without copying personal data.
    native_home = Path.home() / ".codex"
    candidates = sorted(native_home.glob("state*.sqlite"), reverse=True)
    schemas = [candidates[0], native_home / "thread_history_1.sqlite"]
    for source in schemas:
        connection = sqlite3.connect(f"file:{source}?mode=ro", uri=True)
        statements = [sql for (sql,) in connection.execute("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type='index'")]
        target = sqlite3.connect(home / ".codex" / source.name)
        target.executescript(";\n".join(statements))
        if connection.execute("SELECT 1 FROM sqlite_master WHERE name='_sqlx_migrations'").fetchone():
            migration_rows = connection.execute("SELECT * FROM _sqlx_migrations").fetchall()
            if migration_rows:
                target.executemany("INSERT INTO _sqlx_migrations VALUES(" + ",".join("?" for _ in migration_rows[0]) + ")", migration_rows)
        target.commit()
        target.close()
        connection.close()
    source = home / ".pi/agent/sessions/project/source.jsonl"
    source.parent.mkdir(parents=True)
    rows = [{"type":"session","version":3,"id":"native-flow","cwd":str(project),"timestamp":"2026-10-01T00:00:00Z"},
            {"type":"message","id":"u1","message":{"role":"user","content":"native compatibility question"}},
            {"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":"native compatibility answer"}}]
    source.write_text("".join(json.dumps(row)+"\n" for row in rows))
    run(["opencode", "session", "list", "--format", "json", "--pure"])
    run([binary, "sync", "--from", "pi", "--to", "claude", "--to", "codex", "--to", "opencode", "--to", "omp"])
    oc_list = run(["opencode", "session", "list", "--format", "json", "--pure"])
    assert "native compatibility question" in oc_list
    # Pi itself parses a native-format xses import and exports the transcript.
    run([binary, "sync", "--from", "claude", "--to", "pi", "--include-imports"])
    pi_import = next(paths[0] for key, paths in records() if key.startswith("pi|claude|"))
    html = root / "pi-export.html"
    run(["pi", "--offline", "--no-extensions", "--no-mcp", "--export", pi_import, str(html)])
    payload = re.search(r'<script id="session-data"[^>]*>([^<]+)</script>', html.read_text())
    assert payload and "native compatibility answer" in base64.b64decode(payload[1]).decode()

    # Ask the native Codex app server to read the generated rollout/projection.
    codex_import = next(paths[0] for key, paths in records() if key.startswith("codex|pi|"))
    sid = json.loads(Path(codex_import).read_text().splitlines()[0])["payload"]["id"]
    stderr = (root / "codex-app-server.stderr").open("w")
    server = subprocess.Popen([codex_binary, "app-server", "--stdio"], env=env, cwd=project,
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True)
    responses = queue.Queue()
    def read_responses():
        for line in server.stdout:
            responses.put(json.loads(line))
    threading.Thread(target=read_responses, daemon=True).start()
    def rpc(identifier, method, params):
        server.stdin.write(json.dumps(dict(id=identifier, method=method, params=params))+"\n")
        server.stdin.flush()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                obj = responses.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty:
                break
            if obj.get("id") == identifier:
                log.append(dict(method=method, response=obj))
                assert "error" not in obj, obj
                return obj["result"]
        raise TimeoutError(method)
    try:
        rpc(1, "initialize", {"clientInfo":{"name":"xses-verification","version":"1.0"}})
        server.stdin.write(json.dumps({"method":"initialized"})+"\n")
        server.stdin.flush()
        result = rpc(2, "thread/read", {"threadId":sid, "includeTurns":True})
        assert "native compatibility answer" in json.dumps(result)
    finally:
        server.terminate()
        server.wait(timeout=10)
        stderr.close()
    # Verify real OpenCode re-sync leaves a single native picker row.
    with source.open("a") as stream:
        stream.write(json.dumps({"type":"message","id":"u2","message":{"role":"user","content":"updated native question"}})+"\n")
    run([binary, "sync", "--from", "pi", "--to", "opencode"])
    assert len(json.loads(run(["opencode", "session", "list", "--format", "json", "--pure"]))) == 1
    run([binary, "clean", "--all"])
    assert json.loads(run(["opencode", "session", "list", "--format", "json", "--pure"]) or "[]") == []
    assert source.exists() and not Path(pi_import).exists() and not Path(codex_import).exists()
    print("PASS: native Pi export, OpenCode listing/replacement/cleanup, Codex thread/read")
finally:
    (root / "commands.json").write_text(json.dumps(log, indent=2))
    print("Evidence:", root / "commands.json")
