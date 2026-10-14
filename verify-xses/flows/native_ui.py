"""Native TUI checks, invoked by native.py --ui. No prompts/model requests."""
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import time


def verify(root, env, project, records, codex_binary):
    assert shutil.which('tmux'), 'tmux is required for --ui'
    paths = {key.split('|')[0]: files[0] for key, files in records if key.split('|')[1] == 'pi'}
    # The Pi import is a return from the generated Claude transcript.
    paths['pi'] = next(files[0] for key, files in records if key.startswith('pi|claude|'))
    sid = json.loads(Path(paths['codex']).read_text().splitlines()[0])['payload']['id']
    claude = subprocess.check_output(['mise', 'which', 'claude'], text=True).strip() if Path(shutil.which('claude') or '').name == 'claude' and 'command-wrappers' in (shutil.which('claude') or '') else shutil.which('claude')
    assert claude, 'claude is required for --ui'
    # Disposable client setup. The local provider cannot make a model request.
    Path(env['CODEX_HOME'], 'config.toml').write_text('model_provider="audit"\nmodel="audit"\n[model_providers.audit]\nname="Audit"\nbase_url="http://127.0.0.1:9/v1"\nwire_api="responses"\nrequires_openai_auth=false\n')
    Path(env['HOME'], '.claude.json').write_text(json.dumps({'hasCompletedOnboarding': True, 'theme': 'dark'}))
    commands = {
        'pi': ['pi', '--offline', '--no-extensions', '--no-mcp', '--session', paths['pi']],
        'omp': ['omp', '--no-extensions', '--no-skills', '--no-tools', '--no-lsp', '--resume', paths['omp']],
        'claude': [claude, '--bare', '--no-chrome', '--resume', Path(paths['claude']).stem],
        'codex': [codex_binary, 'resume', sid],
        'opencode': ['opencode', '--standalone', '--session', paths['opencode'].removeprefix('opencode://')],
    }
    base = ['tmux', '-S', str(root / 'ui.sock'), '-f', '/dev/null']
    def tmux(*args):
        return subprocess.check_output([*base, *args], env=env, text=True)
    def screen(name):
        return tmux('capture-pane', '-p', '-S', '-1000', '-t', name + ':0')
    (root / 'ui-commands.json').write_text(json.dumps(commands, indent=2))
    try:
        for name, command in commands.items():
            tmux('new-session', '-d', '-s', name, '-x', '160', '-y', '80', '-c', str(project), shlex.join(command))
        for name in commands:
            deadline = time.monotonic() + 35
            trusted = False
            while time.monotonic() < deadline:
                text = screen(name)
                if name == 'claude' and 'Yes, I trust this folder' in text and not trusted:
                    tmux('send-keys', '-t', 'claude:0', 'Down', 'Enter')
                    trusted = True
                if 'Skill history retained.' in text:
                    break
                time.sleep(.1)
            (root / (name + '-ui.txt')).write_text(text)
            assert 'Skill history retained.' in text, (name, text)
            assert 'empty message' not in text, (name, text)
            assert 'skill instruction 79' not in text, (name, text)
            assert '+587' not in text, (name, text)
            if name == 'claude':
                assert 'Bash(' in text and ('Read(' in text or 'Read 1 file' in text), text
                tmux('send-keys', '-t', 'claude:0', 'C-o')
                time.sleep(.2)
                tmux('send-keys', '-t', 'claude:0', 'C-e')
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    expanded = screen(name)
                    if 'audit-output' in expanded:
                        break
                    time.sleep(.1)
                (root / 'claude-expanded-ui.txt').write_text(expanded)
                assert 'audit-output' in expanded, expanded
            print('PASS: native UI', name)
    finally:
        subprocess.run([*base, 'kill-server'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
