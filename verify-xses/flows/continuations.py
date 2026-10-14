#!/usr/bin/env python3
"""Drive continued imports through real CLI conversions in disposable stores."""
import json
import os
from pathlib import Path
import subprocess
import sqlite3
import sys
import tempfile

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/xses").resolve())
root = Path(tempfile.mkdtemp(prefix="xses-continuations-"))
log = []


def rows(path, records, mode="w"):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open(mode) as stream:
        for record in records:
            stream.write(json.dumps(record) + "\n")


def messages(tool, prompt, reply):
    if tool in ("pi", "omp"):
        return [{"type": "message", "message": {"role": role, "content": text}}
                for role, text in [("user", prompt), ("assistant", reply)]]
    if tool == "claude":
        return [{"type": role, "message": {"role": role, "content": text}}
                for role, text in [("user", prompt), ("assistant", reply)]]
    return [{"type": "response_item", "payload": {
        "type": "message", "role": role,
        "content": [{"type": "input_text" if role == "user" else "output_text", "text": text}]}}
        for role, text in [("user", prompt), ("assistant", reply)]]


try:
    for target in ("claude", "codex", "omp", "pi"):
        project = root / target / "project"
        home = root / target / "home"
        project.mkdir(parents=True)
        home.mkdir()
        if target == "codex":
            codex = home / ".codex"
            codex.mkdir()
            with sqlite3.connect(codex / "state_5.sqlite") as db:
                db.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, " + ",".join(
                    column + " TEXT" for column in [
                        "rollout_path", "created_at", "updated_at", "source", "model_provider",
                        "cwd", "title", "sandbox_policy", "approval_mode", "has_user_event",
                        "archived", "cli_version", "first_user_message", "preview", "created_at_ms",
                        "updated_at_ms", "recency_at", "recency_at_ms", "thread_source", "originator",
                        "history_mode", "memory_mode", "name"]) + ")")
            with sqlite3.connect(codex / "thread_history_1.sqlite") as db:
                db.executescript("""
                    CREATE TABLE thread_turns (thread_id TEXT, turn_id TEXT, rollout_ordinal INTEGER,
                        status TEXT, started_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
                        first_user_item_id TEXT, final_agent_item_id TEXT, rollout_end_ordinal INTEGER,
                        PRIMARY KEY (thread_id, turn_id));
                    CREATE TABLE thread_items (thread_id TEXT, turn_id TEXT, item_id TEXT,
                        rollout_ordinal INTEGER, created_at_ms INTEGER, item_json TEXT, item_type TEXT,
                        PRIMARY KEY (thread_id, item_id));
                    CREATE TABLE thread_history_projection_state (thread_id TEXT PRIMARY KEY,
                        next_rollout_byte_offset INTEGER, next_rollout_ordinal INTEGER);
                """)
        env = os.environ | {
            "HOME": str(home), "XDG_DATA_HOME": str(root / target / "data"),
            "XDG_CACHE_HOME": str(root / target / "cache"),
            "XDG_CONFIG_HOME": str(root / target / "config"),
            "OPENCODE_DATA_DIR": str(root / target / "opencode"), "XSES_NO_PROGRESS": "1"}

        def run(*args, ok=True):
            proc = subprocess.run([binary, *args], cwd=project, env=env,
                                  capture_output=True, text=True, timeout=30)
            log.append({"target": target, "argv": [binary, *args], "status": proc.returncode,
                        "stdout": proc.stdout, "stderr": proc.stderr})
            assert (proc.returncode == 0) == ok, log[-1]
            return proc.stdout + proc.stderr

        def records():
            index = root / target / "data/xses/index.jsonl"
            return [(key.split("|"), json.loads(paths))
                    for key, paths in (line.split("\t", 1) for line in index.read_text().splitlines())]

        def imported_from(tool, identifier, destination):
            matches = [paths for key, paths in records()
                       if key[:3] == [destination, tool, identifier]]
            assert len(matches) == 1, matches
            return next(Path(p) for p in matches[0] if p.endswith(".jsonl"))

        source_tool = "claude" if target == "pi" else "pi"
        if source_tool == "pi":
            source = home / ".pi/agent/sessions/project/native.jsonl"
            rows(source, [{"type": "session", "version": 3, "id": "native", "cwd": str(project),
                           "timestamp": "2026-10-14T00:00:00Z"},
                          *messages("pi", "original task", "original answer")])
        else:
            source = home / ".claude/projects/project/native.jsonl"
            rows(source, [{"type": "summary", "sessionId": "native", "cwd": str(project)},
                          *messages("claude", "original task", "original answer")])
        original = source.read_bytes()
        assert "1 written" in run("sync", "--from", source_tool, "--to", target)
        imported = imported_from(source_tool, "native", target)
        assert "0 source sessions" in run("sync", "--from", target, "--to", source_tool)

        # Metadata changes alone must not echo a transcript.
        rows(imported, [{"type": "title", "title": "renamed imported session"}], "a")
        assert "0 source sessions" in run("sync", "--from", target, "--to", source_tool)
        # A repeated prompt still counts as a new turn.
        rows(imported, messages(target, "original task", "continued answer"), "a")
        assert "1 written" in run("sync", "--from", target, "--to", source_tool, "--dry-run")
        assert len(records()) == 1, "dry-run published a branch"
        assert "1 written" in run("sync", "--from", target, "--to", source_tool)
        assert source.read_bytes() == original, "native source changed"
        return_paths = [paths for key, paths in records() if key[0] == source_tool]
        assert len(return_paths) == 1
        returned = next(Path(p) for p in return_paths[0] if p.endswith(".jsonl"))
        assert "continued answer" in returned.read_text()
        assert "1 skipped" in run("sync", "--from", target, "--to", source_tool)
        assert "1 skipped" in run("sync", "--from", source_tool, "--to", target), "returned copy echoed"

        # Later continuation updates the same branch identity.
        rows(imported, messages(target, "next task", "next answer"), "a")
        assert "1 written" in run("sync", "--from", target, "--to", source_tool)
        assert "next answer" in returned.read_text()
        assert len(records()) == 2

        # Parallel work in the original must not erase the continued target.
        before = imported.read_bytes()
        rows(source, messages(source_tool, "parallel task", "parallel answer"), "a")
        failed = run("sync", "--from", source_tool, "--to", target, ok=False)
        assert "continued copy" in failed and "preserved" in failed
        assert imported.read_bytes() == before
        # Preserve both sides if the returned branch itself is continued.
        rows(returned, messages(source_tool, "another branch task", "another branch answer"), "a")
        branch_before = returned.read_bytes()
        rows(imported, messages(target, "third task", "third answer"), "a")
        assert "continued copy" in run("sync", "--from", target, "--to", source_tool, ok=False)
        assert returned.read_bytes() == branch_before
        print(f"PASS: {source_tool} -> {target} -> {source_tool}: continuation, no echo, stable branch, conflicts")
    print(f"PASS: {len(log)} CLI commands")
finally:
    evidence = root / "commands.json"
    evidence.write_text(json.dumps(log, indent=2))
    print(f"Evidence: {evidence}")
