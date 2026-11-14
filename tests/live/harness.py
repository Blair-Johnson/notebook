"""Drive the real nb binary in tmux and check every key binding.

Each case starts a fresh `nb` on a fixture in its own detached session on a
private tmux server (`tmux -L nbtest`, never your own sessions), sends real
keys with `send-keys`, and checks the app's state (`NB_STATE_FILE`) and
screen. Expectations (cases.py) are written by hand from vim semantics;
nothing is read back from nb to decide what "correct" is.

Each run is isolated: a temporary config, autosave and Jupyter runtime
directory, an in-memory clipboard, and a fake $VISUAL. Kernel cases use the
ipykernel in this folder's pixi environment.

    pixi run --manifest-path tests/live/pixi.toml live          # everything
    python3 -I tests/live/harness.py target/debug/nb cell.visual  # a subset

Keys tmux 3.3 cannot send without `set -s extended-keys always` (ctrl+, and
ctrl+backspace) fail unless the extended-keys option is set: pass
`--extended-keys` to set it on the private server.
"""

import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cases import CASES  # noqa: E402

ROOT = os.path.dirname(os.path.abspath(__file__))
ENV_PREFIX = os.path.join(ROOT, ".pixi", "envs", "default")
SOCKET = "nbtest"
WIDTH, HEIGHT = 120, 40


def tmux(*args, check=True):
    config = ["-f", "/dev/null"]
    result = subprocess.run(
        ["tmux", "-L", SOCKET, *config, *args],
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        raise RuntimeError(f"tmux {' '.join(args)}: {result.stderr.strip()}")
    return result


NAMED = {
    "esc": "Escape", "cr": "Enter", "tab": "Tab", "s-tab": "BTab", "bs": "BSpace",
    "del": "DC", "space": "Space", "up": "Up", "down": "Down", "left": "Left",
    "right": "Right", "home": "Home", "end": "End", "pgup": "PPage", "pgdn": "NPage",
}


def tokens(keys):
    """`ab<c-g>` -> [("lit","a"),("lit","b"),("key","C-g")]."""
    out = []
    i = 0
    while i < len(keys):
        c = keys[i]
        if c == "<" and ">" in keys[i + 1:] and keys.index(">", i + 1) > i + 1:
            end = keys.index(">", i + 1)
            name = keys[i + 1:end]
            i = end + 1
            if name == "lt":
                out.append(("lit", "<"))
            elif name.startswith("wait:"):
                out.append(("wait", int(name[5:])))
            elif name in NAMED:
                out.append(("key", NAMED[name]))
            elif name[:2] in ("c-", "a-"):
                prefix = "C-" if name[0] == "c" else "M-"
                rest = name[2:]
                rest = NAMED.get(rest, rest)
                out.append(("key", prefix + rest))
            else:
                raise ValueError(f"unknown key <{name}>")
            continue
        out.append(("lit", c))
        i += 1
    return out


class Session:
    def __init__(self, case):
        self.case = case
        self.dir = tempfile.mkdtemp(prefix="case-", dir=os.path.join(ROOT, "runs"))
        self.name = os.path.basename(self.dir)
        fixture_name, fixture_text = case["fixture"]
        self.notebook = os.path.join(self.dir, fixture_name)
        with open(self.notebook, "w") as f:
            f.write(fixture_text)
        self.state_file = os.path.join(self.dir, "state.json")
        config = os.path.join(self.dir, "config.toml")
        with open(config, "w") as f:
            f.write(case.get("config", ""))
        editor = os.path.join(self.dir, "editor.sh")
        with open(editor, "w") as f:
            f.write('#!/bin/sh\nprintf "# edited\\n" >> "$1"\n')
        os.chmod(editor, 0o755)
        for sub in ("state", "runtime", "xdg"):
            os.makedirs(os.path.join(self.dir, sub))
        env = {
            "NB_STATE_FILE": self.state_file,
            "NB_CLIPBOARD": "memory",
            "NB_STATE_DIR": os.path.join(self.dir, "state"),
            "JUPYTER_RUNTIME_DIR": os.path.join(self.dir, "runtime"),
            "XDG_STATE_HOME": os.path.join(self.dir, "xdg"),
            "VISUAL": editor,
            "EDITOR": editor,
            "CONDA_PREFIX": ENV_PREFIX,
            "PATH": f"{ENV_PREFIX}/bin:{os.environ.get('PATH', '/usr/bin:/bin')}",
            "TERM": "xterm-256color",
            "HOME": os.environ["HOME"],
        }
        exports = " ".join(f"{k}='{v}'" for k, v in env.items())
        command = f"cd '{self.dir}' && env -i {exports} '{NB}' --config '{config}' '{self.notebook}'"
        tmux("new-session", "-d", "-s", self.name, "-x", str(WIDTH), "-y", str(HEIGHT), command)

    def alive(self):
        return tmux("has-session", "-t", self.name, check=False).returncode == 0

    def state(self):
        try:
            with open(self.state_file) as f:
                return json.load(f)
        except (OSError, ValueError):
            return None

    def settle(self, quiet=0.3, limit=6.0):
        """Wait until the state file stops changing."""
        start = time.time()
        last = None
        since = time.time()
        while time.time() - start < limit:
            try:
                stamp = os.stat(self.state_file).st_mtime_ns
            except OSError:
                stamp = None
            if stamp != last:
                last = stamp
                since = time.time()
            elif stamp is not None and time.time() - since >= quiet:
                return
            if not self.alive():
                return
            time.sleep(0.03)

    def wait_for(self, predicate, limit=20.0):
        start = time.time()
        while time.time() - start < limit:
            state = self.state()
            if state is not None and predicate(state):
                return True
            if not self.alive():
                return False
            time.sleep(0.05)
        return False

    def send(self, keys):
        for kind, value in tokens(keys):
            if kind == "wait":
                time.sleep(value / 1000)
                continue
            args = ["send-keys", "-t", self.name]
            if kind == "lit":
                # tmux reads a lone ";" argument as a command separator.
                args += ["-l", "\\;" if value == ";" else value]
            else:
                args.append(value)
            result = tmux(*args, check=False)
            if result.returncode != 0:
                raise RuntimeError(f"tmux cannot send {value}: {result.stderr.strip()}")
            time.sleep(0.04)

    def screen(self):
        if not self.alive():
            return ""
        return tmux("capture-pane", "-p", "-t", self.name).stdout

    def close(self):
        tmux("kill-session", "-t", self.name, check=False)
        shutil.rmtree(self.dir, ignore_errors=True)


def run_case(case):
    session = Session(case)
    try:
        if not session.wait_for(lambda s: s is not None, 10):
            return case, False, "nb did not start", ""
        if case.get("kernel"):
            if not session.wait_for(lambda s: s["kernel"]["phase"] == "Ready" and not s["kernel"]["busy"], 40):
                return case, False, "kernel never became ready", session.screen()
        session.settle()
        if case.get("setup"):
            session.send(case["setup"])
            if case.get("setup_until") and not session.wait_for(case["setup_until"], 30):
                return case, False, "setup never reached its state", session.screen()
            session.settle()
        before = session.state()
        session.send(case["keys"])
        if case.get("wait_until"):
            session.wait_for(lambda s: case["wait_until"](s), case.get("wait_limit", 20))
        session.settle(quiet=case.get("quiet", 0.35))
        after = session.state()
        screen = session.screen()
        alive = session.alive()
        try:
            ok, detail = case["check"](before, after, screen, alive)
        except Exception as error:  # a check that crashes is a failure
            ok, detail = False, f"check raised {error!r}"
        return case, ok, detail, screen
    except Exception as error:
        return case, False, f"harness: {error}", traceback.format_exc()
    finally:
        session.close()


def main():
    global NB
    args = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    if not args:
        sys.exit(__doc__)
    NB = os.path.abspath(args[0])
    pattern = args[1] if len(args) > 1 else ""
    if not os.path.isdir(ENV_PREFIX):
        sys.exit(f"no kernel environment: run `pixi install --manifest-path {ROOT}/pixi.toml`")
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    tmux("kill-server", check=False)
    tmux("start-server", ";", "set", "-s", "exit-empty", "off", check=False)
    if "--extended-keys" in sys.argv:
        tmux("set", "-s", "extended-keys", "always")
    cases = [c for c in CASES if pattern in c["name"]]
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        for case, ok, detail, screen in pool.map(run_case, cases):
            results.append((case, ok, detail, screen))
            mark = "PASS" if ok else "FAIL"
            print(f"{mark}  {case['name']}  {'' if ok else detail}", flush=True)
    tmux("kill-server", check=False)
    failed = [r for r in results if not r[1]]
    terminal = [r for r in failed if r[0].get("terminal")]
    with open(os.path.join(ROOT, "failures.txt"), "w") as f:
        for case, _, detail, screen in failed:
            f.write(f"=== {case['name']}\n{detail}\n{screen}\n")
    print(f"\n{len(results) - len(failed)} passed, {len(failed)} failed of {len(results)}")
    if terminal:
        print(f"{len(terminal)} of the failures are keys tmux cannot send without extended-keys (rerun with --extended-keys)")
    sys.exit(1 if len(failed) > len(terminal) else 0)


NB = None
if __name__ == "__main__":
    main()
