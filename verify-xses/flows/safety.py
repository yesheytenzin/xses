#!/usr/bin/env python3
"""Failure-injection checks against the real CLI; every store is disposable."""
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/xses').resolve())
root = Path(tempfile.mkdtemp(prefix='xses-safety-'))
log = []


def fixture(name):
    base = root / name
    home, project = base / 'home', base / 'project'
    project.mkdir(parents=True)
    home.mkdir()
    env = os.environ | {'HOME': str(home), 'XDG_DATA_HOME': str(base / 'data'),
        'XDG_CONFIG_HOME': str(base / 'config'), 'XDG_CACHE_HOME': str(base / 'cache'),
        'OPENCODE_DATA_DIR': str(base / 'opencode'), 'XSES_NO_PROGRESS': '1'}
    source = home / '.pi/agent/sessions/project/native.jsonl'
    source.parent.mkdir(parents=True)
    source.write_text(''.join(json.dumps(row)+'\n' for row in [
        {'type': 'session', 'version': 3, 'id': 'native', 'cwd': str(project), 'timestamp': '2026-10-14T00:00:00Z'},
        {'type': 'message', 'message': {'role': 'user', 'content': 'original task'}},
        {'type': 'message', 'message': {'role': 'assistant', 'content': 'original answer'}}]))

    def run(*args, ok=True):
        p = subprocess.run([binary, *args], env=env, cwd=project, capture_output=True, text=True, timeout=30)
        log.append(dict(case=name, argv=[binary, *args], status=p.returncode, stdout=p.stdout, stderr=p.stderr))
        assert (p.returncode == 0) == ok, log[-1]
        return p.stdout + p.stderr

    def paths():
        return [Path(p) for line in (base / 'data/xses/index.jsonl').read_text().splitlines()
                for p in json.loads(line.split('\t')[1])]

    return base, source, env, project, run, paths


def append(path, role, text, claude=False):
    with path.open('a') as f:
        f.write(json.dumps({'type': role if claude else 'message', 'message': {'role': role, 'content': text}})+'\n')


try:
    base, source, env, project, run, paths = fixture('duplicates-and-repair')
    assert '1 written' in run('sync', '--from', 'pi', '--to', 'claude', '--to', 'pi', '--to', 'claude')
    dest = next(p for p in paths() if p.suffix == '.jsonl')
    dest.unlink()
    assert '1 written' in run('sync', '--from', 'pi', '--to', 'claude')
    assert dest.is_file()
    assert '1 skipped' in run('sync', '--from', 'pi', '--to', 'claude')

    base, source, env, project, run, paths = fixture('second-artifact-failure')
    run('sync', '--from', 'pi', '--to', 'claude')
    dest = next(p for p in paths() if p.suffix == '.jsonl')
    sidecar = next(p for p in paths() if p.name == 'custom-title.json')
    original, title = dest.read_bytes(), sidecar.read_bytes()
    old_index = (base / 'data/xses/index.jsonl').read_bytes()
    sidecar.unlink()
    sidecar.mkdir()
    append(source, 'user', 'source update')
    run('sync', '--from', 'pi', '--to', 'claude', ok=False)
    assert dest.read_bytes() == original
    assert (base / 'data/xses/index.jsonl').read_bytes() == old_index
    sidecar.rmdir()
    sidecar.write_bytes(title)
    assert '1 written' in run('sync', '--from', 'pi', '--to', 'claude')

    base, source, env, project, run, paths = fixture('missing-baseline-and-clean')
    run('sync', '--from', 'pi', '--to', 'claude')
    dest = next(p for p in paths() if p.suffix == '.jsonl')
    append(dest, 'user', 'unsynced local task', claude=True)
    append(dest, 'assistant', 'unsynced local answer', claude=True)
    before = dest.read_bytes()
    for p in (base / 'data/xses/import-baselines').glob('*.json'):
        p.unlink()
    append(source, 'user', 'source update')
    assert 'preserved' in run('sync', '--from', 'pi', '--to', 'claude', ok=False)
    assert dest.read_bytes() == before
    assert 'preserved' in run('clean', '--dry-run')
    assert 'preserved' in run('clean')
    assert dest.read_bytes() == before
    assert (base / 'data/xses/index.jsonl').read_text().strip()
    run('clean', '--force')
    assert not dest.exists()
    assert not (base / 'data/xses/index.jsonl').read_text().strip()

    base, source, env, project, run, paths = fixture('corrupt-baseline-partial-progress')
    run('sync', '--from', 'pi', '--to', 'claude')
    next((base / 'data/xses/import-baselines').glob('*.json')).write_text('{broken')
    healthy = base / 'home/.claude/projects/project/healthy.jsonl'
    healthy.parent.mkdir(parents=True, exist_ok=True)
    healthy.write_text(json.dumps({'type': 'summary', 'sessionId': 'healthy', 'cwd': str(project)})+'\n')
    append(healthy, 'user', 'healthy task', claude=True)
    append(healthy, 'assistant', 'healthy answer', claude=True)
    result = run('sync', '--from', 'claude', '--to', 'pi', '--all', ok=False)
    assert '1 written' in result and 'invalid import baseline' in result

    for i in range(8):
        base, source, env, project, run, paths = fixture('omp-clean-'+str(i))
        run('sync', '--from', 'pi', '--to', 'omp')
        artifacts = paths()
        run('clean')
        assert all(not p.exists() for p in artifacts)
        assert source.exists()

    for mode in ['assistant', 'image']:
        base, source, env, project, run, paths = fixture('continuation-'+mode)
        run('sync', '--from', 'pi', '--to', 'claude')
        dest = next(p for p in paths() if p.suffix == '.jsonl')
        if mode == 'assistant':
            append(dest, 'assistant', 'assistant-only local work', claude=True)
        else:
            append(dest, 'user', [{'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': 'aGVsbG8='}}], claude=True)
        assert '1 written' in run('sync', '--from', 'claude', '--to', 'pi')
        assert 'preserved' in run('clean', '--all')
        assert dest.exists()

    base, source, env, project, run, paths = fixture('same-stamp-edit')
    run('sync', '--from', 'pi', '--to', 'claude')
    dest = next(p for p in paths() if p.suffix == '.jsonl')
    stamp = dest.stat()
    changed = dest.read_bytes().replace(b'original answer', b'parallel answer')
    dest.write_bytes(changed)
    os.utime(dest, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    append(source, 'user', 'source update')
    assert 'preserved' in run('sync', '--from', 'pi', '--to', 'claude', ok=False)
    assert dest.read_bytes() == changed

    base, source, env, project, run, paths = fixture('duplicate-file-ids')
    source.with_name('duplicate.jsonl').write_bytes(source.read_bytes())
    assert re.search(r'pi\s+1 sessions', run('ls', '--count'))
    assert '1 sessions' in run('ls', 'pi', '--all', '--include-noise')

    # Mapping upgrades can create a new native ID without overwriting or
    # retiring an older branch whose baseline is no longer trustworthy.
    base, source, env, project, run, paths = fixture('legacy-mapping-upgrade')
    run('sync', '--from', 'pi', '--to', 'claude')
    original = next(p for p in paths() if p.suffix == '.jsonl')
    legacy = original.with_name('00000000-0000-4000-8000-000000000013.jsonl')
    original.rename(legacy)
    legacy_rows = [json.loads(line) for line in legacy.read_text().splitlines()]
    for row in legacy_rows:
        if 'sessionId' in row:
            row['sessionId'] = legacy.stem
    legacy.write_text(''.join(json.dumps(row)+'\n' for row in legacy_rows))
    append(legacy, 'assistant', 'legacy local work', claude=True)
    index = base / 'data/xses/index.jsonl'
    key, artifacts = index.read_text().strip().split('\t', 1)
    parts = key.split('|')
    parts[5] = 'm13'
    index.write_text('|'.join(parts)+'\t'+json.dumps([str(legacy)])+'\n')
    before = legacy.read_bytes()
    assert '1 written' in run('sync', '--from', 'pi', '--to', 'claude')
    assert legacy.read_bytes() == before and original.exists()
    assert '1 skipped' in run('sync', '--from', 'pi', '--to', 'claude')

    # Fresh OpenCode stores can be v2-only. Mixed stores must count IDs once,
    # prefer the v2 representation, and invalidate caches after DB/WAL writes.
    for mixed in [False, True]:
        base, source, env, project, run, paths = fixture('database-counts-'+str(mixed))
        db_path = base / 'opencode/opencode.db'
        db_path.parent.mkdir()
        with sqlite3.connect(db_path) as db:
            db.execute('PRAGMA journal_mode=WAL')
            db.executescript('''
                CREATE TABLE session_v2(id TEXT PRIMARY KEY,slug TEXT,title TEXT,directory TEXT,
                    time_created INTEGER,time_updated INTEGER,time_archived INTEGER,parent_id TEXT,metadata TEXT);
                CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,
                    time_created INTEGER,time_updated INTEGER,data TEXT);
            ''')
            if mixed:
                db.executescript('''CREATE TABLE session(id TEXT PRIMARY KEY,slug TEXT,title TEXT,directory TEXT,
                    time_created INTEGER,time_updated INTEGER,time_archived INTEGER,parent_id TEXT);
                    CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);
                    CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);''')
                db.execute('INSERT INTO session VALUES(?,?,?,?,1,1,NULL,NULL)', ('shared', 'native', 'old title', str(project)))
            db.execute('INSERT INTO session_v2 VALUES(?,?,?,?,1,1,NULL,NULL,?)', ('shared', 'native', 'v2 title', str(project), '{}'))
            for seq, role in enumerate(['user', 'assistant']):
                db.execute('INSERT INTO session_message VALUES(?,?,?,?,1,1,?)', (role, 'shared', role, seq, json.dumps({'content': [{'type': 'text', 'text': 'hello'}]})))
            db.commit()
            assert 'v2 title' in run('ls', 'opencode', '--all')
            for _ in range(2):
                assert re.search(r'opencode\s+1 sessions', run('ls', '--count'))
            db.execute('INSERT INTO session_v2 VALUES(?,?,?,?,1,1,NULL,NULL,?)', ('empty', 'native', 'empty', str(project), '{}'))
            db.commit()
            assert re.search(r'opencode\s+2 sessions', run('ls', '--count'))
            assert '2 sessions' in run('ls', 'opencode', '--all', '--include-noise')
            blocked = base / 'blocked-cache-directory'
            blocked.write_text('cache storage unavailable')
            env['XDG_DATA_HOME'] = str(blocked)
            assert '2 sessions' in run('ls', 'opencode', '--all', '--include-noise')
            assert re.search(r'opencode\s+2 sessions', run('ls', '--count'))

    # Simulate process death with the exact durable undo-journal format. A real
    # sync with no selected sources must recover before touching its index.
    base, source, env, project, run, paths = fixture('interrupted-publication')
    run('sync', '--from', 'pi', '--to', 'claude')
    dest = next(p for p in paths() if p.suffix == '.jsonl')
    before = dest.read_bytes()
    pending = base / 'data/xses/pending-imports/interrupted'
    pending.mkdir(parents=True)
    backup = pending / 'file-0'
    backup.write_bytes(before)
    (pending / 'journal.json').write_text(json.dumps({'files': [{'path': str(dest), 'backup': str(backup)}], 'sql': [], 'opencode': []}))
    dest.write_text('interrupted replacement')
    run('sync', '--from', 'omp', '--to', 'pi')
    assert dest.read_bytes() == before and not pending.exists()

    print(f'PASS: {len(log)} CLI commands; refresh rollback, missing artifacts/baselines, partial progress, cleanup ordering, continuation detection, recovery')
finally:
    (root / 'commands.json').write_text(json.dumps(log, indent=2))
    print('Evidence:', root / 'commands.json')
