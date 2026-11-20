"""Native PTY regression for a cancelled/stalled external-editor handoff."""
import argparse
import json
from pathlib import Path
import shlex
import signal
import tempfile
import termios
import time

from carbon_workspace_pty import start, until, visible, local
from cli_progress_pty import roots


def exercise(binary, output, name, terminate):
    started = time.monotonic()
    screen = None
    result = {"case": name, "passed": False}
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as directory:
        root = Path(directory)
        home, cwd = roots(root)
        marker = cwd / "editor-launched"
        editor = cwd / "fixture-editor"
        editor.write_text("#!/bin/sh\nprintf launched > " + shlex.quote(str(marker)) + "\n")
        editor.chmod(0o700)
        (cwd / "notes.md").write_text("# fixture document\n")
        try:
            screen = start(binary, root, home, cwd, {"EDITOR": str(editor)})
            local(screen, "/files notes.md")
            until(screen, lambda text: "read-only" in text and "fixture document" in text,
                  "stable file inspector")
            for _ in range(12):
                screen.drain(.01)
            paused_at = len(screen.raw)
            termios.tcflow(screen.slave, termios.TCOOFF)
            screen.send("e")
            # Output remains kernel-paused while the validation/handoff runs.
            # The editor must never exec without its reporting-disable drain.
            if terminate:
                time.sleep(.08)
                interrupted = time.monotonic()
                screen.child.send_signal(signal.SIGTERM)
                while screen.child.poll() is None:
                    assert time.monotonic() - interrupted < 2, "editor setup ignored SIGTERM under TCOOFF"
                    screen.drain(.005)
                result["signal_to_exit_seconds"] = time.monotonic() - interrupted
                assert screen.child.returncode == 143, screen.child.returncode
                assert termios.tcgetattr(screen.slave) == screen.original, "terminal state not restored"
                termios.tcflow(screen.slave, termios.TCOON)
                final = screen.finish()
            else:
                time.sleep(1.25)
                assert screen.child.poll() is None, "setup stall terminated the interactive frontend"
                assert not marker.exists(), "editor launched before output drain"
                termios.tcflow(screen.slave, termios.TCOON)
                until(screen, lambda text: "stalled" in text,
                      "bounded handoff drain error after flow-control recovery")
                screen.send(b"\x1b")
                until(screen, lambda text: "q terminal" not in text,
                      "file inspector closed after failed handoff")
                screen.send("AFTER_EDITOR_SETUP")
                until(screen, lambda text: "AFTER_EDITOR_SETUP" in text,
                      "input reporting restored after failed handoff")
                final = screen.finish(signal.SIGTERM)
                assert final["exit"] == 143, final["exit"]
            assert not marker.exists(), "cancelled/stalled handoff executed the editor"
            assert final["termios_restored"] and final["flags_restored"], "stdio restoration"
            result.update(passed=True, exit=final["exit"], termios_restored=True,
                          flags_restored=True, editor_launched=False,
                          paused_capture_bytes=len(screen.raw) - paused_at)
        except Exception as error:
            result["error"] = repr(error)
            raise
        finally:
            result["duration_seconds"] = time.monotonic() - started
            if screen is not None:
                try:
                    termios.tcflow(screen.slave, termios.TCOON)
                except OSError:
                    pass
                screen.drain(0)
                (output / (name + ".pty")).write_bytes(screen.raw)
                (output / (name + ".screen.txt")).write_text(visible(screen))
                screen.close()
            (output / (name + ".json")).write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    results = [exercise(args.binary.resolve(), args.output, "editor-setup-blocked-sigterm", True),
               exercise(args.binary.resolve(), args.output, "editor-setup-stall-recovery", False)]
    (args.output / "report.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps({"passed": len(results), "cases": [item["case"] for item in results]}))


if __name__ == "__main__":
    main()
