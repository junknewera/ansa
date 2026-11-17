"""Real Neovim with deterministic protocol faults and delayed projection updates."""
import argparse
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from parity.frontend_io import environment


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    observations = []
    errors = []
    with tempfile.TemporaryDirectory(prefix="ansa-nvim-", dir="/tmp") as directory:
        root = Path(directory)
        endpoint = root / "workspace.sock"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(endpoint))
        listener.listen(4)
        listener.settimeout(8)

        def serve():
            try:
                for connection in range(1, 4):
                    stream, _ = listener.accept()
                    with stream:
                        stream.settimeout(8)
                        pipe = stream.makefile("rb")
                        hello = json.loads(pipe.readline())
                        assert hello["role"] == "viewer"
                        session = "other" if connection == 3 else "original"
                        incarnation = "inc_b" if connection == 3 else "inc_a"
                        def emit(value):
                            data = json.dumps(value, ensure_ascii=False).encode() + b"\n"
                            stream.sendall(data[:7])
                            stream.sendall(data[7:])
                        def snapshot(sequence):
                            return dict(type="snapshot", version=1, session=session, incarnation=incarnation,
                                        through=sequence, state=dict(phase="completed", durable=True,
                                        transcript_total=2, turn="t1", request="r1", preview=None))
                        emit(dict(type="welcome", version=1, role="viewer", session=session, incarnation=incarnation))
                        emit(snapshot(1))
                        count = 0
                        sequence = 1
                        for line in pipe:
                            request = json.loads(line)
                            if request["kind"] == "snapshot":
                                sequence += 1
                                emit(snapshot(sequence))
                                continue
                            assert request["kind"] == "history"
                            count += 1
                            if connection == 1 and count <= 4:
                                # Snapshots during backoff and after exhaustion
                                # must not bypass the shared automatic read gate.
                                emit(dict(type="posted", kind="history", id=request["id"], status="QueueFull"))
                                for _ in range(8):
                                    sequence += 1
                                    emit(snapshot(sequence))
                                continue
                            emit(dict(type="posted", kind="history", id=request["id"], status="Accepted"))
                            if connection == 2:
                                time.sleep(.4)
                            text = "OTHER_DOCUMENT" if connection == 3 else "VIEWER_INITIAL Ж界\n" + "\n".join(f"line {i}" for i in range(90))
                            emit(dict(type="history", id=request["id"], session=session, incarnation=incarnation,
                                      begin=0, end=3, total=3, entries=[dict(role="user", text=text)]))
                        observations.append(dict(connection=connection, history_requests=count))
            except BaseException as error:
                errors.append(repr(error))
            finally:
                listener.close()

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        plugin = Path(__file__).resolve().parents[1] / "integrations/ansa.nvim"
        lua = root / "test.lua"
        report = root / "report.json"
        lua.write_text("""vim.opt.runtimepath:prepend(%s)
local a = require('ansa')
a.setup()
a.attach(%s)
assert(vim.wait(2000, function() return a.status().connected end, 10), 'handshake')
-- Exhaust initial + three read retries while the server floods snapshots.
vim.wait(900, function() return false end, 10)
vim.cmd('AnsaRefresh')
assert(vim.wait(2000, function()
  local s=a.status()
  return s.buffer and table.concat(vim.api.nvim_buf_get_lines(s.buffer,0,-1,false),'\\n'):find('VIEWER_INITIAL',1,true)
end, 10), 'manual refresh did not recover QueueFull')
a.open()
local buf = a.status().buffer
vim.api.nvim_win_set_cursor(0, {60,0})
vim.fn.winrestview({topline=40,lnum=60,col=0})
local view=vim.fn.winsaveview()
-- Give an older snapshot time to trigger a bad repeated history loop.
vim.wait(150, function() return false end, 10)
vim.cmd('AnsaDetach')
assert(not a.status().snapshot, 'stale snapshot after detach')
a.attach(%s)
assert(vim.wait(2000, function() return a.status().connected and a.status().snapshot end, 10), 'reconnect')
assert(vim.api.nvim_get_current_buf()==buf, 'reconnect moved buffer')
assert(vim.api.nvim_win_get_cursor(0)[1]==60, 'reconnect clamped cursor before delayed history')
assert(vim.fn.winsaveview().topline==view.topline, 'reconnect clamped scroll')
vim.wait(600, function() return false end, 10)
assert(vim.api.nvim_win_get_cursor(0)[1]==60, 'history response moved cursor')
vim.bo[buf].modifiable=true
vim.api.nvim_buf_set_lines(buf,0,0,false,{'DIRTY_SELECTION'})
assert(vim.bo[buf].modified, 'viewer buffer does not track edits')
local old=vim.api.nvim_buf_get_lines(buf,0,-1,false)
vim.cmd('AnsaDetach')
a.attach(%s)
assert(vim.wait(2000, function()
 local s=a.status()
 return s.connected and s.buffer and s.buffer ~= buf and table.concat(vim.api.nvim_buf_get_lines(s.buffer,0,-1,false),'\\n'):find('OTHER_DOCUMENT',1,true)
end,10), 'new session did not get a separate document')
assert(vim.deep_equal(old,vim.api.nvim_buf_get_lines(buf,0,-1,false)), 'new session overwrote old document')
assert(vim.api.nvim_get_current_buf()==buf, 'new attachment changed current buffer')
vim.fn.writefile({vim.json.encode({passed=true, delayed_view=true, session_isolation=true})},%s)
vim.cmd('qa!')
""" % (json.dumps(str(plugin)), *(json.dumps(str(endpoint)) for _ in range(3)), json.dumps(str(report))))
        child = subprocess.run(["nvim", "--headless", "--clean", "-l", str(lua)],
                               env=environment(root, root, True), capture_output=True, timeout=15)
        worker.join(timeout=2)
        (output / "nvim.stdout").write_bytes(child.stdout)
        (output / "nvim.stderr").write_bytes(child.stderr)
        (output / "observations.json").write_text(json.dumps(dict(connections=observations, errors=errors), indent=2))
        assert child.returncode == 0, child.stderr.decode(errors="replace")
        assert not worker.is_alive() and not errors, errors
        assert [row["history_requests"] for row in observations] == [5, 1, 1], observations
        assert json.loads(report.read_text())["passed"]
    result = dict(passed=4, cases=["QueueFull-bounded-retry-and-explicit-refresh", "newer-page-no-read-loop",
                                "delayed-reconnect-preserves-cursor-scroll", "new-session-preserves-dirty-document"])
    (output / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output)
