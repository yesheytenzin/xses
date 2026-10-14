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
import signal
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

# Isolate the actual OpenCode CLI from the person's background server. Probe
# capabilities instead of assuming flags from a particular release.
opencode_binary = shutil.which("opencode")
assert opencode_binary, "opencode is required"
list_help = subprocess.check_output([opencode_binary, "session", "list", "--help"], text=True)
list_flags = ["--pure"] if "--pure" in list_help else []
if "--standalone" in list_help:
    wrapper_dir = root / "bin"
    wrapper_dir.mkdir()
    wrapper = wrapper_dir / "opencode"
    wrapper.write_text("#!/usr/bin/env python3\nimport os,sys\n" +
                       f"binary={opencode_binary!r}\n" +
                       "args=sys.argv[1:]\nif args and args[0]=='session': args.append('--standalone')\n" +
                       "os.execv(binary,[binary,*args])\n")
    wrapper.chmod(0o755)
    env["PATH"] = str(wrapper_dir) + os.pathsep + env["PATH"]

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
    run(["opencode", "session", "list", "--format", "json", *list_flags])
    run([binary, "sync", "--from", "pi", "--to", "claude", "--to", "codex", "--to", "opencode", "--to", "omp"])
    oc_list = run(["opencode", "session", "list", "--format", "json", *list_flags])
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
    old_rollout = Path(codex_import)
    source_rows = [json.loads(line) for line in source.read_text().splitlines()]
    source_rows[0]['timestamp'] = '2026-09-30T00:00:00Z'
    source.write_text(''.join(json.dumps(row)+'\n' for row in source_rows))
    run([binary, 'sync', '--from', 'pi', '--to', 'codex'])
    codex_import = next(paths[0] for key, paths in records() if key.startswith('codex|pi|'))
    assert Path(codex_import) != old_rollout and not old_rollout.exists()
    with sqlite3.connect(home / '.codex' / schemas[0].name) as connection:
        assert connection.execute('SELECT rollout_path FROM threads WHERE id=?', (sid,)).fetchone()[0] == codex_import
    stderr = (root / "codex-app-server.stderr").open("w")
    server = subprocess.Popen([codex_binary, "app-server"], env=env, cwd=project,
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
    # Kill a real xses refresh after its rollout/state mutation but before its
    # history seed. The next invocation must restore both native databases and
    # the exact previous rollout before selecting sources.
    if os.name == 'posix':
        original_rollout = Path(codex_import).read_bytes()
        def codex_rows():
            saved = []
            for database, tables, column in [(home / '.codex' / schemas[0].name, ['threads'], 'id'),
                    (home / '.codex/thread_history_1.sqlite', ['thread_items', 'thread_turns', 'thread_history_projection_state'], 'thread_id')]:
                with sqlite3.connect(database) as connection:
                    for table in tables:
                        saved.append((table, connection.execute(f'SELECT * FROM {table} WHERE {column}=? ORDER BY 1,2', (sid,)).fetchall()))
            return saved
        original_rows = codex_rows()
        marker = root / 'history-seed-blocked'
        wrapper_dir = root / 'bin'
        wrapper_dir.mkdir(exist_ok=True)
        sqlite_binary = shutil.which('sqlite3')
        wrapper = wrapper_dir / 'sqlite3'
        wrapper.write_text('#!/usr/bin/env python3\nimport os,sys,time\nfrom pathlib import Path\n' +
            f'binary={sqlite_binary!r}\nmarker=Path({str(marker)!r})\n' +
            "args=sys.argv[1:]\n" +
            "if '-readonly' not in args and any('thread_history_1.sqlite' in arg for arg in args):\n" +
            "    text=sys.stdin.read()\n    marker.write_text(str(os.getpid()))\n    while True: time.sleep(0.1)\n" +
            "os.execv(binary,[binary,*args])\n")
        wrapper.chmod(0o755)
        crash_env = env | {'PATH': str(wrapper_dir) + os.pathsep + env['PATH']}
        with source.open('a') as stream:
            stream.write(json.dumps({'type': 'message', 'id': 'before-crash', 'message': {'role': 'user', 'content': 'interrupted update'}})+'\n')
        process = subprocess.Popen([binary, 'sync', '--from', 'pi', '--to', 'codex'], env=crash_env,
            cwd=project, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 15
            while not marker.exists() and process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.025)
            assert marker.exists(), process.communicate(timeout=1)
            process.kill()
            os.kill(int(marker.read_text()), signal.SIGKILL)
            stdout, errors = process.communicate(timeout=10)
            log.append(dict(argv=[binary, 'sync', '--from', 'pi', '--to', 'codex'],
                            status=process.returncode, stdout=stdout, stderr=errors, killed_at='history seed'))
        finally:
            if process.poll() is None:
                process.kill()
            wrapper.unlink()
        run([binary, 'sync', '--from', 'omp', '--to', 'pi'])
        assert Path(codex_import).read_bytes() == original_rollout
        assert codex_rows() == original_rows
        assert not list((root / 'data/xses/pending-imports').glob('*/journal.json'))
    # Verify real OpenCode re-sync leaves a single native picker row.
    with source.open("a") as stream:
        stream.write(json.dumps({"type":"message","id":"u2","message":{"role":"user","content":"updated native question"}})+"\n")
    run([binary, "sync", "--from", "pi", "--to", "opencode"])
    assert len(json.loads(run(["opencode", "session", "list", "--format", "json", *list_flags]))) == 1
    # Force replacement imports to fail while allowing the saved original's
    # native export to be re-imported. This exercises real OpenCode rollback.
    if os.name == 'posix':
        wrapper_dir = root / 'bin'
        wrapper_dir.mkdir(exist_ok=True)
        wrapper = wrapper_dir / 'opencode'
        previous_wrapper = wrapper.read_text() if wrapper.exists() else None
        standalone = ['--standalone'] if '--standalone' in list_help else []
        wrapper.write_text('#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\n' +
            f'binary={opencode_binary!r}\nstandalone={standalone!r}\n' +
            "args=sys.argv[1:]\n" +
            "if len(args)>1 and args[:2]==['session','import']:\n" +
            "    payload=next((Path(arg) for arg in args[2:] if Path(arg).is_file()),None)\n" +
            "    if payload and 'forced-import-failure' in payload.read_text():\n" +
            "        print('injected import failure',file=sys.stderr)\n        sys.exit(1)\n" +
            "if args and args[0]=='session': args.extend(standalone)\n" +
            "os.execv(binary,[binary,*args])\n")
        wrapper.chmod(0o755)
        failure_env = env | {'PATH': str(wrapper_dir) + os.pathsep + env['PATH']}
        with source.open('a') as stream:
            stream.write(json.dumps({'type': 'message', 'id': 'failure', 'message': {'role': 'user', 'content': 'forced-import-failure'}})+'\n')
        try:
            result = subprocess.run([binary, 'sync', '--from', 'pi', '--to', 'opencode'],
                cwd=project, env=failure_env, capture_output=True, text=True, timeout=60)
            log.append(dict(argv=[binary, 'sync', '--from', 'pi', '--to', 'opencode'],
                status=result.returncode, stdout=result.stdout, stderr=result.stderr, injected_failure=True))
            assert result.returncode != 0
        finally:
            if previous_wrapper is None:
                wrapper.unlink()
            else:
                wrapper.write_text(previous_wrapper)
                wrapper.chmod(0o755)
        oc_id = next(paths[0].removeprefix('opencode://') for key, paths in records() if key.startswith('opencode|pi|'))
        restored = run(['opencode', 'session', 'export', oc_id])
        assert 'updated native question' in restored and 'forced-import-failure' not in restored
        assert not list((root / 'data/xses/pending-imports').glob('*/journal.json'))
    run([binary, "clean", "--all"])
    assert json.loads(run(["opencode", "session", "list", "--format", "json", *list_flags]) or "[]") == []
    assert source.exists() and not Path(pi_import).exists() and not Path(codex_import).exists()
    print("PASS: native Pi export, OpenCode replacement/rollback/cleanup, Codex thread/read and killed-publication recovery")
finally:
    (root / "commands.json").write_text(json.dumps(log, indent=2))
    print("Evidence:", root / "commands.json")
