"""Capture real Carbon image turns at the synthetic SIWC/OpenRouter transport boundary."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import time

from bench import environment, write
from carbon import TrialStopped, capture
from carbon_workspace_pty import Pty, Terminal, completed, ready, roots, seed_siwc, until
from public_images import TASK_IDS

MODELS = {"siwc": "gpt-6.2-sol", "openrouter": "deepseek/deepseek-v4.1-flash-1017"}
# A valid 2x2 PNG for hermetic CI. --manifest exercises the original public images.
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGNkaNrC"
                       "wMDAxMDAwMDAAAAN/gE6rk+0VQAAAABJRU5ErkJggg==")


def check_wire(route, capture_file, turns, image):
    payload = image.read_bytes()
    mime = "image/png" if payload.startswith(b"\x89PNG") else "image/jpeg"
    url = f"data:{mime};base64," + base64.b64encode(payload).decode()
    if route == "siwc":
        parts = [{"type": "input_text", "text": turns[1]}, {"type": "input_image", "image_url": url}]
    else:
        parts = [{"type": "text", "text": turns[1]}, {"type": "image_url", "image_url": {"url": url}}]
    expected = [turns[0], parts, turns[2]]
    lines = capture_file.read_bytes().splitlines()
    assert len(lines) == len(turns), (route, "unexpected number of inference requests", len(lines))
    for index, line in enumerate(lines):
        users = [item["content"] for item in json.loads(line) if item.get("role") == "user"]
        # Check the entire sequence, including the original bytes on replay and a plain follow-up.
        assert users == expected[:index + 1], (route, "incorrect user content on request", index)


def exercise(binary, route, mode, image, question, evidence, supports_images=True):
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as temporary:
        root = Path(temporary)
        home, workspace = roots(root)
        home.chmod(0o700)
        workspace.chmod(0o700)
        (root / "tmp").mkdir(mode=0o700)
        wire = root / "outbound-messages.jsonl"
        wire.touch(mode=0o600)
        if supports_images:
            (root / "fixture-image-model").touch(mode=0o600)
        auth = seed_siwc(home) if route == "siwc" else home / ".ansa/auth"
        turns = ["Remember the next question.", question, "Acknowledge this text-only follow-up."]
        sha = hashlib.sha256(image.read_bytes()).hexdigest()
        arm = {"mode": mode, "route": route, "model": MODELS[route], "effort": None, "auth_dir": auth}
        try:
            _, answer = capture(binary, root, home, workspace, environment(root, home), arm, turns,
                                time.monotonic() + 30, None, None,
                                fixture_args=["--frontend-fixture", str(root), route + "-carbon"],
                                attachments=[{"turn": 1, "path": str(image), "sha256": sha}])
        except TrialStopped as error:
            if supports_images or str(error) != route + "_image_unsupported":
                raise
            assert error.status == "unsupported", error.status
            requests = wire.read_text().splitlines()
            assert len(requests) == 1, "text-only route received an image-bearing request"
            users = [m["content"] for m in json.loads(requests[0]) if m.get("role") == "user"]
            assert users == turns[:1]
            return {"route": route, "mode": mode, "status": "unsupported", "requests_checked": 1,
                    "evidence": evidence.name}
        finally:
            # Preserve the canonical journal and exact synthetic outbound payload. Never copy auth_dir.
            evidence.mkdir(mode=0o700)
            sessions = home / ".ansa/sessions"
            for part in sessions.glob("*/part-*.log"):
                target = evidence / "sessions" / part.relative_to(sessions)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(part, target)
            shutil.copyfile(wire, evidence / wire.name)
        assert supports_images, "text-only route accepted an image"
        check_wire(route, wire, turns, image)
        receipt, = answer["image_inputs"]
        assert receipt["turn"] == 1 and receipt["sha256"] == sha
        return {"route": route, "mode": mode, "sha256": sha, "bytes": image.stat().st_size,
                "evidence": evidence.name,
                "requests_checked": 3}



def check_composer(binary):
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as temporary:
        root = Path(temporary)
        home, workspace = roots(root)
        home.chmod(0o700)
        workspace.chmod(0o700)
        (root / "tmp").mkdir()
        seed_siwc(home)
        (root / "fixture-image-model").touch(mode=0o600)
        wire = root / "outbound-messages.jsonl"
        wire.touch(mode=0o600)
        screen = Pty(str(binary), root, home, workspace, "", size=(120, 38),
                     argv=["--frontend-fixture", str(root), "siwc-carbon"], base_env=environment(root, home))
        screen.vt = Terminal(120, 38)

        def submit(text):
            screen.send(b"\x1b[200~" + text.encode() + b"\x1b[201~\r")

        def command(text, status):
            submit(text)
            until(screen, lambda _: status in screen.vt.line(-1), status)

        def turn(text):
            submit(text)
            until(screen, lambda _: completed(root, text) and ready(screen, MODELS["siwc"])
                  and "Ctrl+C stop" not in screen.vt.line(-2), text)

        try:
            until(screen, lambda _: ready(screen, MODELS["siwc"]), "ready for /attach")
            image = workspace / "file with spaces.png"
            image.write_bytes(PNG)
            (workspace / "link").symlink_to(image)
            os.mkfifo(workspace / "fifo")
            command("/attach fifo", "image_regular_file_required")
            command("/attach link", "image_open")
            command("/attach file with spaces.png", "Image attached (1)")
            image.unlink()
            command("/orchestrate", "Send or clear attached images")
            screen.send(b"\x15")  # Rejected command stays editable.
            turn("Describe the attached image.")
            image.write_bytes(PNG)
            command("/attach file with spaces.png", "Image attached (1)")
            command("/attach --clear", "Image attachments cleared")
            turn("Answer without an attachment.")
            submit("/exit")
            result = screen.finish()
            assert result["exit"] == 0 and result["termios_restored"]
            first, second = [json.loads(line) for line in wire.read_text().splitlines()]
            url = "data:image/png;base64," + base64.b64encode(PNG).decode()
            assert first[-1]["content"][1] == {"type": "input_image", "image_url": url}
            assert second[-1]["content"] == "Answer without an attachment."
        finally:
            if screen.child.poll() is None:
                screen.child.terminate()
                screen.finish()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="frontend_adapter with synthetic transports")
    parser.add_argument("--output", type=Path, required=True, help="fresh directory for test receipts")
    parser.add_argument("--manifest", type=Path, help="prepared public manifest with the original images")
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(mode=0o700)
    if args.manifest:
        cases = []
        for task in json.loads(args.manifest.read_bytes())["tasks"]:
            if "attachments" in task:
                attachment, = task["attachments"]
                image = (args.manifest.parent / attachment["path"]).resolve(strict=True)
                cases.append((task["id"], task["turns"][attachment["turn"]], image))
        if sorted(task for task, _, _ in cases) != sorted(TASK_IDS):
            parser.error("expected both published image tasks exactly once in the manifest")
    else:
        image = out / "fixture image.png"
        image.write_bytes(PNG)
        cases = [("fixture", "Describe the attached figure.", image)]
    check_composer(args.binary.resolve(strict=True))
    receipts = []
    for task, question, image in cases:
        for route in MODELS:
            for mode in ("work", "plan"):
                receipt = exercise(args.binary.resolve(strict=True), route, mode, image, question,
                                   out / f"{task}.{route}.{mode}")
                receipts.append(receipt)
                print(f"{route}/{mode}: {receipt['bytes']} bytes checked", flush=True)
    task, question, image = cases[0]
    for route in MODELS:
        receipts.append(exercise(args.binary.resolve(strict=True), route, "work", image, question,
                                 out / f"{task}.{route}.text-only", supports_images=False))
    write(out / "result.json", receipts)


if __name__ == "__main__":
    main()
