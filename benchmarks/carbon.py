"""Drive a fresh benchmark session through the normal Carbon frontend."""

import base64
import hashlib
import json
from pathlib import Path
import os
import re
import stat
import subprocess
import time

from carbon_workspace_pty import Pty, Terminal, visible
from parity import session_cases

MODES = {"work": "Work", "plan": "Plan", "planner": "Orchestrate",
         "orchestrate": "Orchestrate", "orchestrate+": "Orchestrate+"}
MAX_IMAGES = 4
MAX_IMAGE_BYTES = 2 * 1024 * 1024
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


def prepare_turn_images(root, attachments, turns):
    """Stage the exact frozen bytes under generated PTY-safe names, outside the workspace."""
    queued = [[] for _ in turns]
    if not attachments:
        return queued
    directory = root / "images"
    directory.mkdir(mode=0o700)
    for index, attachment in enumerate(attachments):
        images = queued[attachment["turn"]]
        remaining = MAX_IMAGE_BYTES - sum(item["bytes"] for item in images)
        if len(images) >= MAX_IMAGES:
            raise ValueError("at most four images per turn")
        file = Path(attachment["path"])
        with os.fdopen(os.open(file, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW), "rb") as stream:
            st = os.fstat(stream.fileno())
            if not stat.S_ISREG(st.st_mode) or not 0 < st.st_size <= remaining:
                raise ValueError("invalid or oversized image input")
            data = stream.read(remaining + 1)
        sha = hashlib.sha256(data).hexdigest()
        if len(data) > remaining or sha != attachment["sha256"]:
            raise ValueError("image input changed")
        if data.startswith(b"\x89PNG\r\n\x1a\n"):
            mime, suffix = "image/png", "png"
        elif data.startswith(b"\xff\xd8\xff"):
            mime, suffix = "image/jpeg", "jpg"
        else:
            raise ValueError("image input must be PNG or JPEG")
        path = directory / f"{index}.{suffix}"
        path.write_bytes(data)
        images.append({"path": path, "sha256": sha, "bytes": len(data), "media_type": mime})
    return queued


def verify_turn_images(images, message, accepted, response):
    if accepted.get("image_sha256", []) != [image["sha256"] for image in images]:
        raise ValueError("accepted image hashes differ from the manifest")
    # Check the actual user input in the admitted provider request, even after a
    # tool loop. Historical images must never migrate to a later plain-text turn.
    request = response["response"]["request"]
    user = next((item for item in reversed(request["input"]) if item.get("role") == "user"), None)
    if not user:
        raise ValueError("provider request is missing the current user turn")
    content = user["content"]
    if not images:
        if content != message:
            raise ValueError("unexpected image or text in a later turn")
        return
    if not isinstance(content, list) or len(content) != 1 + len(images) or content[0] != {
            "type": "input_text", "text": message}:
        raise ValueError("incorrect text or image count in provider request")
    for part, image in zip(content[1:], images):
        prefix = f"data:{image['media_type']};base64,"
        url = part.get("image_url", "")
        if part.get("type") != "input_image" or not isinstance(url, str) or not url.startswith(prefix):
            raise ValueError("expected an inline original image in provider request")
        actual = base64.b64decode(url[len(prefix):], validate=True)
        if len(actual) != image["bytes"] or hashlib.sha256(actual).hexdigest() != image["sha256"]:
            raise ValueError("provider image bytes differ from the frozen input")


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
            reason = event.get("diagnostic", kind)
            if reason in ("siwc_image_unsupported", "openrouter_image_unsupported"):
                raise TrialStopped("unsupported", reason)
            if reason.startswith(("http_", "auth_", "provider_", "openrouter_catalog")) or reason in (
                    "openrouter_auth", "openrouter_http", "openrouter_key_invalid", "openrouter_model",
                    "openrouter_provider", "openrouter_route_state", "siwc_structured_format_unconfirmed"):
                raise TrialStopped("infra_error", reason)
            raise TrialStopped("failed", reason)
    return None


def check_run_status(footer):
    status = footer.strip()
    if status.startswith(("Plan stopped", "Orchestration paused")):
        # A check whose effect is unknown cannot establish a model outcome.
        kind = "infra_error" if status == "Plan stopped (orchestration_check_effect_unknown)" else "failed"
        raise TrialStopped(kind, status)


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
            fixture_args=None, attachments=()):
    turn_images = prepare_turn_images(root, attachments, turns)
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
        image_inputs = []
        plan = None
        candidate = None
        result_workspace = workspace
        if mode in ("work", "plan"):
            for index, (message, images) in enumerate(zip(turns, turn_images)):
                for count, image in enumerate(images, 1):
                    submit("/attach " + str(image["path"]))

                    def acknowledged(_):
                        footer = screen.vt.line(-1).strip()
                        if footer.startswith("Could not attach image"):
                            raise ValueError(footer)
                        return ready() and footer.startswith(f"Image attached ({count})")

                    try:
                        wait(acknowledged, "confirming image attachment")
                    except TrialStopped as error:
                        raise TrialStopped("infra_error", "Carbon did not acknowledge the image") from error
                before = len(parent_events())
                submit(message)
                turn, response = wait(lambda _: completed_turn(
                    parent_events()[before:], lambda event: event["text"] == message),
                    "completing turn")
                if turn["mode"] != ("basic" if mode == "work" else "plan") or turn.get("draft"):
                    raise ValueError("unexpected native turn mode")
                if attachments:
                    verify_turn_images(images, message, turn, response)
                    image_inputs.extend({"turn": index, "session": parent, "bytes": image["bytes"],
                                         "media_type": image["media_type"], "sha256": image["sha256"]}
                                        for image in images)
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
                    check_run_status(screen.vt.line(-1))
                    if "Plan paused" in frame or "Agent paused" in frame:
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
        answer = {"answers": answers, "plan": plan, "candidate": candidate}
        if attachments:
            answer["image_inputs"] = image_inputs
        return result_workspace, answer
    finally:
        if screen.child.poll() is None:
            screen.child.terminate()
            try:
                screen.child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
        screen.close()
        (root / "carbon.pty").write_bytes(screen.raw)
