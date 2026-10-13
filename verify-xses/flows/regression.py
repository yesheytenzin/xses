#!/usr/bin/env python3
"""Drive the shipped CLI against disposable stores; never imports into real stores.

Usage: python3 verify-xses/flows/regression.py target/debug/xses
Requires Python 3 and sqlite3 on PATH. No API keys or network calls.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/xses").resolve())
root = Path(tempfile.mkdtemp(prefix="xses-regression-"))
home = root / "home"
project = root / "project"
project.mkdir()
home.mkdir()
env = os.environ.copy()
# Child process isolation: never change the invoking shell's home or stores.
env.update(HOME=str(home), XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"),
           XDG_CACHE_HOME=str(root / "cache"), OPENCODE_DATA_DIR=str(root / "opencode"),
           OPencode_DATA_DIR=str(root / "opencode"), XSES_NO_PROGRESS="1", PI_OFFLINE="1")
log = []

def run(*args, ok=True, extra=None):
    proc = subprocess.run([binary, *args], cwd=project, env=env | (extra or {}), text=True,
                          capture_output=True, timeout=30)
    log.append({"argv": [binary, *args], "status": proc.returncode, "stdout": proc.stdout, "stderr": proc.stderr})
    assert (proc.returncode == 0) == ok, log[-1]
    return proc.stdout + proc.stderr


def write_rows(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))


def conversation(identifier, text="first question"):
    return [{"type": "session", "version": 3, "id": identifier, "cwd": str(project), "timestamp": "2026-10-01T00:00:00Z"},
            {"type": "message", "id": "u1", "message": {"role": "user", "content": text}},
            {"type": "message", "id": "a1", "parentId": "u1", "message": {"role": "assistant", "content": "first answer"}},
            {"type": "message", "id": "u2", "parentId": "a1", "message": {"role": "user", "content": "second question"}},
            {"type": "message", "id": "a2", "parentId": "u2", "message": {"role": "assistant", "content": "second answer"}}]


def index_records():
    path = root / "data/xses/index.jsonl"
    if not path.exists():
        return []
    return [(key, json.loads(paths)) for key, paths in (line.split("\t", 1) for line in path.read_text().splitlines())]


def source_record(tool="pi", identifier="fixture-pi"):
    return next((key, paths) for key, paths in index_records() if key.split("|")[1:3] == [tool, identifier])


try:
    source = home / ".pi/agent/sessions/project/source.jsonl"
    write_rows(source, conversation("fixture-pi"))
    source_digest = hashlib.sha256(source.read_bytes()).hexdigest()
    assert "first question" in run("ls", "pi")
    cache = root / "data/xses/scan-cache-pi.json"
    stamp = cache.stat().st_mtime_ns
    run("ls", "pi")
    assert cache.stat().st_mtime_ns == stamp, "warm listings rewrote the cache"

    # A broken unrelated database must not block a source-filtered command.
    broken = root / "opencode/opencode.db"
    broken.parent.mkdir()
    broken.write_text("not a database")
    assert "first question" in run("ls", "pi")
    run("sync", "--from", "pi", "--to", "claude", "--scope", "1", "--dry-run")
    assert not (root / "data/xses/index.jsonl").exists()
    run("ls", ok=False)
    broken.unlink()

    run("sync", "--from", "pi", "--to", "claude", "--scope", "1")
    old_key, old_paths = source_record()
    old_rollout = next(Path(p) for p in old_paths if p.endswith(".jsonl"))
    turns = [r for r in map(json.loads, old_rollout.read_text().splitlines()) if r.get("type") in ("user", "assistant")]
    assert "second question" in json.dumps(turns) and "first question" not in json.dumps(turns)
    assert "1 skipped" in run("sync", "--from", "pi", "--to", "claude", "--scope", "1")
    assert source_record()[0] == old_key

    run("sync", "--from", "pi", "--to", "claude")
    full_key, full_paths = source_record()
    assert full_key != old_key and len(index_records()) == 1
    assert all(not Path(p).exists() for p in old_paths)
    full_rollout = next(Path(p) for p in full_paths if p.endswith(".jsonl"))
    assert "first question" in full_rollout.read_text()
    assert "first question" in run("ls", "claude")

    assert hashlib.sha256(source.read_bytes()).hexdigest() == source_digest

    # Keep the last good generation when the writer cannot create its store.
    target_root = home / ".claude/projects"
    parked = home / ".claude/parked"
    target_root.rename(parked)
    target_root.write_text("block directory creation")
    before_index = (root / "data/xses/index.jsonl").read_bytes()
    with source.open("a") as stream:
        stream.write(json.dumps({"type": "message", "message": {"role": "user", "content": "third question"}}) + "\n")
    run("sync", "--from", "pi", "--to", "claude", ok=False)
    assert (root / "data/xses/index.jsonl").read_bytes() == before_index
    assert all((parked / Path(p).relative_to(target_root)).exists() for p in full_paths)
    target_root.unlink()
    parked.rename(target_root)
    run("sync", "--from", "pi", "--to", "claude")
    assert len(index_records()) == 1
    assert "third question" in Path(next(p for p in source_record()[1] if p.endswith(".jsonl"))).read_text()

    # The kernel lock blocks another process, then releases without deleting it.
    lockpath = root / "data/xses/index.lock"
    with lockpath.open("a+b") as held:
        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert "another xses" in run("sync", "--from", "pi", "--to", "claude", ok=False)
    assert "1 skipped" in run("sync", "--from", "pi", "--to", "claude")

    # Independent sources survive scoped cleanup, and all generations disappear.
    other = home / ".pi/agent/sessions/project/other.jsonl"
    write_rows(other, conversation("other-pi", "other question"))
    run("sync", "other-pi", "--from", "pi", "--to", "claude")
    keep = source_record()[1]
    other_paths = source_record(identifier="other-pi")[1]
    run("clean", "--dry-run")
    assert all(Path(p).exists() for p in other_paths)
    run("clean")
    assert all(not Path(p).exists() for p in other_paths)
    assert all(Path(p).exists() for p in keep)
    assert len(index_records()) == 1

    # Read a real SQLite fixture: same-length part edits with unchanged timestamps
    # must re-import, and deleting a part must do the same.
    db = sqlite3.connect(broken)
    db.executescript("""
      CREATE TABLE session(id TEXT PRIMARY KEY, slug TEXT, title TEXT, directory TEXT,
        time_created INTEGER, time_updated INTEGER, time_archived INTEGER, parent_id TEXT);
      CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
      CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
      CREATE INDEX message_session ON message(session_id);
    """)
    db.execute("INSERT INTO session VALUES(?,?,?,?,?,?,NULL,NULL)", ("oc-source", "native", "OpenCode fixture", str(project), 1000, 2000))
    for i, role in enumerate(["user", "assistant"]):
        mid = f"m{i}"
        db.execute("INSERT INTO message VALUES(?,?,?,?,?)", (mid, "oc-source", i, i, json.dumps({"role": role})))
        db.execute("INSERT INTO part VALUES(?,?,?,?,?,?)", (f"p{i}", "oc-source", mid, i, i, json.dumps({"type": "text", "text": "alpha"})))
    db.commit()
    run("sync", "--from", "opencode", "--to", "pi")
    previous = source_record("opencode", "oc-source")
    assert "1 skipped" in run("sync", "--from", "opencode", "--to", "pi")
    db.execute("UPDATE part SET data=? WHERE id='p1'", (json.dumps({"type": "text", "text": "bravo"}),))
    db.commit()
    assert "1 written" in run("sync", "--from", "opencode", "--to", "pi")
    current = source_record("opencode", "oc-source")
    assert previous[0] != current[0] and all(not Path(p).exists() for p in previous[1])
    assert "bravo" in Path(current[1][0]).read_text()
    db.execute("DELETE FROM part WHERE id='p1'")
    db.commit()
    assert "1 written" in run("sync", "--from", "opencode", "--to", "pi")
    assert "bravo" not in Path(source_record("opencode", "oc-source")[1][0]).read_text()
    db.close()
    run("ls", "--count")
    run("doctor")
    run("sync", "latest:0", "--dry-run", ok=False)
    run("sync", "--from", "pi", "--to", "pi", "--dry-run")
    run("clean", "--all", "--dry-run")
    run("clean", "--all")
    assert index_records() == []
    assert source.exists() and other.exists()
    assert '"xses":' not in source.read_text()
    print(f"PASS: {len(log)} CLI commands; scope, idempotency, failed replacement, locking, incremental cache, OpenCode edits/deletions, cleanup")
finally:
    (root / "commands.json").write_text(json.dumps(log, indent=2))
    print(f"Evidence: {root / 'commands.json'}")
