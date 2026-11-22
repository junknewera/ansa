"""Drive a fresh benchmark session through the normal Carbon frontend."""

import hashlib
import json
import re
import stat
import subprocess
import time

from carbon_workspace_pty import Pty, Terminal, visible
from parity import session_cases

MODES = {"work": "Work", "plan": "Plan", "planner": "Orchestrate",
         "orchestrate": "Orchestrate", "orchestrate+": "Orchestrate+"}
MAX_JOURNAL = 64 * 1024 * 1024


class TrialStopped(Exception):
    def __init__(self, status, reason):
        self.status = status
        super().__init__(reason)


def journals(home, only=None):
    budget = MAX_JOURNAL
    traces = {}
    catalog = home / ".ansa/sessions"
    for directory in [catalog / only] if only else sorted(catalog.glob("*")):
        if not directory.is_dir() or directory.is_symlink():
            continue
        parts = {}
        for file in directory.glob("part-*.log"):
            meta = file.lstat()
            if not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1 or meta.st_size > budget:
                raise ValueError("invalid session evidence")
            budget -= meta.st_size
            parts[file.name] = file.read_bytes()
        if parts:
            rows = json.loads(session_cases.compact(session_cases.decode(parts)))
            if rows:
                traces[directory.name] = rows
    return traces


def response_text(event):
    return "".join(part["text"] for item in event["response"]["items"]
                   if item["type"] == "message" and item.get("role") == "assistant"
                   for part in item["content"] if part["type"] == "output_text")


def completed_turn(events, match):
    active = None
    responses = []
    for event in events:
        kind = event["kind"]
        if kind == "accepted":
            active = event if match(event) else None
            responses.clear()
        elif active and kind == "response":
            responses.append(event)
        elif active and kind == "done":
            return active, responses[-1]
        elif active and kind in ("pause", "cancel"):
            raise TrialStopped("failed", event.get("diagnostic", kind))
    return None


def candidate_for(run, source, input_commit, git):
    release = run / "release.json"
    if not release.is_file():
        return None
    receipt = json.loads(release.read_text())
    commit, baseline, head = (receipt[key] for key in ("candidate", "baseline", "source_head"))
    if not all(re.fullmatch(r"[a-f0-9]{40}|[a-f0-9]{64}", value)
               for value in (commit, baseline, head)):
        raise ValueError("invalid release identity")
    checkout = run / "checkout"
    if git(checkout, "rev-parse", "refs/heads/ansa-result").strip() != commit:
        raise ValueError("candidate and released branch differ")
    source_tree = git(source, "rev-parse", f"{head}^{{tree}}").strip()
    if (input_commit != head or git(source, "rev-parse", "HEAD").strip() != head
            or git(checkout, "rev-parse", f"{baseline}^{{tree}}").strip() != source_tree
            or git(checkout, "rev-list", "--parents", "-n", "1", commit).split() != [commit, baseline]):
        raise ValueError("candidate does not descend from the trial inputs")
    return commit


def install_roles(home, route, account, roles):
    profile = {"offline": "offline", "siwc": "live", "openrouter": account}[route]
    key = hashlib.sha256(f"{profile}\n{account}".encode()).hexdigest()
    path = home / ".ansa/model-defaults" / key / "orchestrate-plus"
    path.mkdir(parents=True, mode=0o700)
    path.chmod(0o700)
    preference = path / "generation.json"
    preference.write_text(json.dumps({"version": 1, **roles}))
    preference.chmod(0o600)


def capture(binary, root, home, workspace, env, arm, turns, deadline, git, input_commit,
            fixture_args=None):
    argv = ["--cwd", str(workspace), "--model", arm["model"]]
    if arm["route"] == "offline":
        argv.append("--offline")
        env = env | {"ANSA_OFFLINE_EVAL_ARM": "N0"}  # Existing feature-gated Work fixture.
    else:
        argv += ["--auth-dir", str(arm["auth_dir"])]
        if arm["route"] == "openrouter":
            argv.append("--openrouter")

    screen = Pty(str(binary), root, home, workspace, "", size=(120, 38),
                 argv=argv if fixture_args is None else fixture_args, base_env=env)
    screen.vt = Terminal(120, 38)
    mode = arm["mode"]
    model = arm["model"]

    def ready():
        return (screen.vt.line(-4).startswith("  ›")
                and "Tab mode" in screen.vt.line(-2)
                and model in screen.vt.line(-2))

    def wait(predicate, stage):
        while True:
            if time.monotonic() >= deadline:
                raise TrialStopped("timeout", stage)
            frame = visible(screen)
            if (result := predicate(frame)):
                return result
            if screen.child.poll() is not None:
                raise TrialStopped("infra_error", f"ansa exited during {stage}")
            screen.drain(0.05)

    def parent_events():
        return journals(home, only=parent).get(parent, [])

    def submit(message):
        screen.send(b"\x1b[200~" + message.encode() + b"\x1b[201~\r")

    def selected(frame, label):
        return screen.vt.line(-2).strip().startswith(f"{label} ·")

    def proposal(frame):
        return "Refine" in frame and "Cancel" in frame and "Start" in frame

    try:
        wait(lambda _: ready(), "startup")
        traces = journals(home)
        if len(traces) != 1:
            raise ValueError("expected one fresh Carbon session")
        parent, rows = next(iter(traces.items()))
        binding = rows[0]["configuration"]["binding"]
        if binding["requested_model"] != model:
            raise ValueError("unexpected initial model")
        if mode == "orchestrate+":
            route = arm["route"] if fixture_args is None else "siwc"
            install_roles(home, route, binding["account"], arm["roles"])
        if arm["effort"] is not None:
            screen.send(f'/effort {arm["effort"]}\r')
            wait(lambda _: ready() and any(event["kind"] == "generation_selected"
                 and event.get("model") == model and event.get("effort") == arm["effort"]
                 for event in parent_events()), "reasoning effort selection")

        target = ("Work", "Plan", "Orchestrate", "Orchestrate+").index(MODES[mode])
        for label in ("Plan", "Orchestrate", "Orchestrate+")[:target]:
            screen.send(b"\t")
            wait(lambda frame: ready() and selected(frame, label), "mode selection")

        answers = []
        plan = None
        candidate = None
        result_workspace = workspace
        if mode in ("work", "plan"):
            for message in turns:
                before = len(parent_events())
                submit(message)
                turn, response = wait(lambda _: completed_turn(
                    parent_events()[before:], lambda event: event["text"] == message),
                    "completing turn")
                if turn["mode"] != ("basic" if mode == "work" else "plan") or turn.get("draft"):
                    raise ValueError("unexpected native turn mode")
                answers.append(response_text(response))
                wait(lambda _: ready() and "Ctrl+C stop" not in screen.vt.line(-2), "chat ready")
                # Require the submitted draft to have left the editor.
                wait(lambda _: screen.vt.line(-4) == "  ›", "chat editor clear")
        else:
            message, = turns
            submit(message)

            def drafted(frame):
                if not proposal(frame):
                    return None
                events = parent_events()
                latest = next((i for i in range(len(events) - 1, -1, -1)
                               if events[i]["kind"] == "accepted" and events[i].get("draft")), None)
                return completed_turn(events[latest:], lambda _: True) if latest is not None else None

            _, response = wait(drafted, "plan confirmation")
            plan = json.loads(response_text(response))
            if mode == "planner":
                screen.send(b"\x1b")
                wait(lambda frame: ready() and not proposal(frame), "dismiss plan")
            else:
                frame = visible(screen)
                if "[Start]" not in frame and "[Start read only]" not in frame:
                    raise TrialStopped("failed", "Carbon did not admit plan execution")
                screen.send(b"\r")

                def finished(frame):
                    if "Plan paused" in frame or "Agent paused" in frame or "Plan stopped ·" in frame:
                        raise TrialStopped("failed", "orchestration stopped")
                    turn = completed_turn(parent_events(), lambda event: event["turn"].startswith("summary-"))
                    if not turn or not ready() or "Ctrl+C stop" in screen.vt.line(-2):
                        return None
                    return turn

                turn, response = wait(finished, "orchestration handoff")
                handoff = json.loads(turn["text"])
                if handoff.get("orchestration_outcome") == "stopped":
                    raise TrialStopped("failed", "orchestration stopped")
                answers.append(response_text(response))
                run_id = turn["turn"].removeprefix("summary-")
                if not re.fullmatch(r"[a-f0-9]{32}", run_id):
                    raise ValueError("invalid orchestration identity")
                run = home / ".ansa/orchestration" / parent / run_id
                wait(lambda _: (run / "summary-admitted").is_file(), "durable orchestration handoff")
                candidate = candidate_for(run, workspace, input_commit, git)
                if candidate is None and any(step["access"] == "write" for step in plan["steps"]):
                    raise TrialStopped("failed", "orchestration did not release a candidate")
                if candidate is not None:
                    if handoff.get("verified_candidate", {}).get("commit") != candidate:
                        raise ValueError("orchestration handoff identifies a different candidate")
                    result_workspace = root / "candidate"
                    git(result_workspace.parent, "clone", "--quiet", "--no-local",
                        "--branch", "ansa-result", "--single-branch", str(run / "checkout"),
                        str(result_workspace))
                    if git(result_workspace, "rev-parse", "HEAD").strip() != candidate:
                        raise ValueError("verifier workspace has a different candidate")

        screen.send("/exit\r")
        exit_result = screen.finish()
        if exit_result["exit"] != 0 or not exit_result["termios_restored"]:
            raise ValueError("Carbon did not exit cleanly")
        return result_workspace, {"answers": answers, "plan": plan, "candidate": candidate}
    finally:
        if screen.child.poll() is None:
            screen.child.terminate()
            try:
                screen.child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
        screen.close()
        (root / "carbon.pty").write_bytes(screen.raw)
