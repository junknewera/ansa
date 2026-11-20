"""Production JSONL startup and cancellation using owned synthetic auth only."""
import argparse
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent / "parity"))
from frontend_io import Lines, stop


def run_case(binary, output, name, *, expired=False, cancel_signal=None):
    directory = output / name
    directory.mkdir(mode=0o700)
    auth = directory / "auth"
    auth.mkdir(mode=0o700)
    identity = dict(version=1, host_id="synthetic-host", client_id="synthetic-client",
                    subject="synthetic-subject")
    credentials = dict(identity, scope="openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
                       session="synthetic-session", expires_at=0 if expired else 4102444800,
                       earliest_refresh_at=0, refresh_pending=False,
                       access_token="synthetic-access", refresh_token="synthetic-refresh", nonce="synthetic-nonce")
    for filename, value in [("registration.json", identity), ("credentials.json", credentials)]:
        path = auth / filename
        path.write_text(json.dumps(value))
        path.chmod(0o600)
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(4)
    listener.setblocking(False)
    proxy = "http://127.0.0.1:" + str(listener.getsockname()[1])
    env = dict(os.environ)
    for key in ("http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"):
        env[key] = proxy
    env.update(no_proxy="", NO_PROXY="", TERM="dumb", NO_COLOR="1")
    argv = [str(binary), "--live-jsonl", "--session", str(directory / "journal"),
            "--auth-dir", str(auth), "--model", "gpt-6.1-sol", "--returned-model", "gpt-6.1-sol"]
    begin = time.monotonic()
    child = None
    lines = None
    connection = None
    result = dict(case=name, passed=False, error=None, submitted=False, proxy_connections=0)
    try:
        child = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True)
        lines = Lines(child)
        deadline = time.monotonic() + 3
        while not lines.events:
            assert not select.select([listener], [], [], 0)[0], "network before JSONL readiness/Submit"
            assert child.poll() is None, "process exited before JSONL readiness"
            assert time.monotonic() < deadline, "JSONL startup timeout"
            lines.drain(.005)
        first = lines.events[0]
        assert first["type"] == "snapshot" and first["state"] == "idle", "fresh idle snapshot"
        assert first["worker_busy"] is False and first["history_items"] == 0, "fresh session"
        assert not select.select([listener], [], [], 0)[0], "network before Submit"
        if cancel_signal is not None:
            assert expired
            prefix = {key: first[key] for key in ("schema_version", "session", "incarnation")}
            lines.send(dict(prefix, turn="startup", command_id="submit", type="submit", text="hello", mode="basic"))
            result["submitted"] = True
            lines.until(lambda events: any(e["type"] == "command_accepted" for e in events))
            assert select.select([listener], [], [], 3)[0], "dispatch did not validate expired auth"
            connection, _ = listener.accept()
            result["proxy_connections"] += 1
            connection.settimeout(2)
            head = b""
            while b"\r\n\r\n" not in head:
                data = connection.recv(4096)
                assert data and len(head) < 16384, "proxy header bound"
                head += data
            assert head.startswith(b"CONNECT auth.openai.com:443 "), "unexpected auth route"
            interrupted = time.monotonic()
            child.send_signal(cancel_signal)
            finished = lines.finish()
            result["cancel_seconds"] = time.monotonic() - interrupted
            assert finished["exit"] == 128 + cancel_signal, "signal exit status"
            assert result["cancel_seconds"] < 2, "cancel blocked by auth transport"
            assert not any(e["type"] in ("done", "text_delta", "tool_started") for e in lines.events), "dispatch after startup cancellation"
            assert not select.select([listener], [], [], 0)[0], "unexpected auth retry"
        else:
            finished = lines.finish()
            assert finished["exit"] == 0, "idle EOF exit status"
            assert not select.select([listener], [], [], 0)[0], "network on idle EOF"
        result["passed"] = True
    except (AssertionError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
    finally:
        if child is not None:
            stop(child)
            result["exit_code"] = child.returncode
        if lines is not None:
            for _ in range(3):
                lines.drain(0)
            (directory / "stdout.jsonl").write_bytes(lines.out)
            (directory / "stderr.raw").write_bytes(lines.err)
        if child is not None:
            for stream in (child.stdin, child.stdout, child.stderr):
                if stream is not None and not stream.closed:
                    stream.close()
        if connection is not None:
            connection.close()
        listener.close()
        result["wall_seconds"] = time.monotonic() - begin
        (directory / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=32)
    args = parser.parse_args()
    if not 1 <= args.repeat <= 256:
        parser.error("repeat must be between 1 and 256")
    binary = args.binary.resolve(strict=True)
    os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:2])
    args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
    # Auth files are synthetic and confined to this newly created output.
    rows = [run_case(binary, args.output, "valid-" + str(index)) for index in range(args.repeat)]
    rows += [run_case(binary, args.output, "expired-idle", expired=True)]
    rows += [run_case(binary, args.output, "expired-" + str(sig), expired=True, cancel_signal=sig)
             for sig in (signal.SIGINT, signal.SIGTERM)]
    report = dict(passed=sum(row["passed"] for row in rows), total=len(rows), cases=rows)
    (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"passed": report["passed"], "total": report["total"],
                      "errors": [{"case": row["case"], "error": row["error"]} for row in rows if not row["passed"]]}))
    return int(report["passed"] != report["total"])


if __name__ == "__main__":
    raise SystemExit(main())
