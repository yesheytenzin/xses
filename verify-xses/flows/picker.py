#!/usr/bin/env python3
"""Drive the real picker in an isolated tmux terminal; requires tmux (Linux/macOS)."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/xses").resolve())
root = Path(tempfile.mkdtemp(prefix="xses-picker-"))
home = root / "home"
project = root / "project"
project.mkdir()
store = home / ".pi/agent/sessions/project"
store.mkdir(parents=True)
for i in range(12):
    text = "jkneedle old history" if i == 0 else f"ordinary session {i}"
    rows = [{"type":"session","id":str(i),"cwd":str(project)},
            {"type":"message","message":{"role":"user","content":text}},
            {"type":"message","message":{"role":"assistant","content":"answer"}}]
    path = store / f"{i}.jsonl"
    path.write_text("".join(json.dumps(r)+"\n" for r in rows))
    os.utime(path, (1600000000 + i, 1600000000 + i))
env = os.environ.copy()
env.update(HOME=str(home), XDG_DATA_HOME=str(root / "data"),
           OPENCODE_DATA_DIR=str(root / "data/opencode"), OPencode_DATA_DIR=str(root / "data/opencode"))
base = ["tmux", "-S", str(root / "tmux.sock"), "-f", "/dev/null"]

def tmux(*args):
    return subprocess.check_output([*base, *args], env=env, text=True)

def wait_for(test):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        screen = tmux("capture-pane", "-p", "-t", "verify:0")
        if test(screen):
            return screen
        time.sleep(0.025)
    raise AssertionError(screen)

try:
    command = f'{shlex.quote(binary)} pick; result=$?; stty -a > {shlex.quote(str(root / "terminal.txt"))}; printf "__EXIT__%s\\n" "$result"; sleep 30'
    tmux("new-session", "-d", "-s", "verify", "-x", "110", "-y", "20", "-c", str(project), command)
    before = wait_for(lambda screen: "5 sessions" in screen)
    assert "jkneedle" not in before
    tmux("send-keys", "-t", "verify:0", "-l", "jkneedle")
    after = wait_for(lambda screen: "❯ jkneedle" in screen and "jkneedle old history" in screen)
    assert "1 sessions" in after
    tmux("send-keys", "-t", "verify:0", "Escape")
    wait_for(lambda screen: "__EXIT__0" in screen)
    terminal = (root / "terminal.txt").read_text()
    assert "-icanon" not in terminal and "-echo " not in terminal
    (root / "before.txt").write_text(before)
    (root / "filtered.txt").write_text(after)
    print("PASS: j/k type into search, search reaches older sessions beyond the default cap, Escape restores terminal modes")
    print("Evidence:", root)
finally:
    subprocess.run([*base, "kill-server"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
