"""Producer races in real Neovim, with controlled ACKs and transport failures."""
import argparse
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading

from parity.frontend_io import environment


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    events = []
    errors = []
    with tempfile.TemporaryDirectory(prefix="ansa-editor-", dir="/tmp") as directory:
        root = Path(directory)
        endpoint = root / "workspace.sock"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(endpoint))
        listener.listen(2)
        listener.settimeout(8)

        def serve():
            try:
                for connection in range(2):
                    stream, _ = listener.accept()
                    current = None
                    delayed = None
                    with stream:
                        stream.settimeout(8)
                        pipe = stream.makefile("rb")
                        assert json.loads(pipe.readline())["role"] == "editor"
                        def emit(value):
                            stream.sendall(json.dumps(value).encode() + b"\n")
                        emit(dict(type="welcome", version=1, role="editor", session="original", incarnation="inc_a"))
                        emit(dict(type="snapshot", version=1, session="original", incarnation="inc_a", through=1,
                                  state=dict(phase="idle", durable=True, transcript_total=0, preview=None)))
                        for line in pipe:
                            request = json.loads(line)
                            kind = request["kind"]
                            if kind == "snapshot":
                                emit(dict(type="posted", id=request["id"], kind=kind, status="Accepted"))
                            elif kind == "history":
                                emit(dict(type="posted", id=request["id"], kind=kind, status="Accepted"))
                                emit(dict(type="history", id=request["id"], session="original", incarnation="inc_a",
                                          begin=0, end=0, total=0, entries=[]))
                            elif kind == "publish_context":
                                current = request["context"]
                                events.append(dict(connection=connection, kind=kind, sequence=current["sequence"]))
                                if connection == 0 and current["sequence"] == 1:
                                    delayed = request
                                    continue
                                emit(dict(type="context_published", id=request["id"], buffer=current["buffer"],
                                          changedtick=current["changedtick"], sequence=current["sequence"]))
                            elif kind == "invalidate_context":
                                events.append(dict(connection=connection, kind=kind, sequence=request["sequence"]))
                                if current and current["sequence"] == request["sequence"]:
                                    current = None
                                if delayed:
                                    old = delayed["context"]
                                    emit(dict(type="context_published", id=delayed["id"], buffer=old["buffer"],
                                              changedtick=old["changedtick"], sequence=old["sequence"]))
                                    delayed = None
                                emit(dict(type="context_invalidated", id=request["id"]))
                            else:
                                raise AssertionError(request)
                    # Closing the attachment withdraws even an undelivered
                    # invalidate. This models the production lease Drop.
                    events.append(dict(connection=connection, kind="closed", live_context_before_close=current is not None))
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
local uv=vim.uv
local make_pipe=uv.new_pipe
local blocked=false
local failed=false
uv.new_pipe=function(...)
  local pipe=make_pipe(...)
  return setmetatable({}, {__index=function(_, key)
    if key=='get_write_queue_size' then return function() return blocked and 999999 or pipe:get_write_queue_size() end end
    if key=='write' then return function(_, data, callback)
      if failed and callback then vim.schedule(function() callback('EPIPE') end); return end
      return pipe:write(data, callback)
    end end
    return function(_, ...) return pipe[key](pipe, ...) end
  end})
end
local a=require('ansa')
a.setup()
a.attach(%s,{editor=true})
assert(vim.wait(2000,function() return a.status().connected end,10),'editor attachment')
local buf=vim.api.nvim_create_buf(true,false)
vim.api.nvim_set_current_buf(buf)
vim.api.nvim_buf_set_name(buf,%s)
vim.api.nvim_buf_set_lines(buf,0,-1,false,{'first Ж界'})
a.share(1,1)
-- A second share cannot replace the still unacknowledged snapshot.
a.share(1,1)
vim.api.nvim_buf_set_lines(buf,0,-1,false,{'edited before ACK'})
vim.wait(100,function() return false end,10)
assert(not a.status().shared,'delayed ACK installed stale context')
a.share(1,1)
assert(vim.wait(2000,function() return a.status().shared ~= nil end,10),'fresh context not acknowledged')
local before=vim.api.nvim_buf_get_changedtick(buf)
vim.cmd('file '..vim.fn.fnameescape(%s))
assert(vim.api.nvim_buf_get_changedtick(buf)==before,'rename changed tick; bad fixture')
assert(not a.status().shared,'rename kept old scope context')
a.share(1,1)
assert(vim.wait(2000,function() return a.status().shared ~= nil end,10),'renamed context not acknowledged')
vim.api.nvim_buf_delete(buf,{force=true})
assert(not a.status().shared,'wipe retained context')
local hidden=vim.api.nvim_create_buf(true,false)
vim.api.nvim_buf_set_name(hidden,%s)
vim.api.nvim_set_current_buf(hidden)
vim.api.nvim_buf_set_lines(hidden,0,-1,false,{'hidden'})
a.share(1,1)
assert(vim.wait(2000,function() return a.status().shared ~= nil end,10),'hidden context acknowledgement')
vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(true,false))
vim.api.nvim_buf_set_lines(hidden,0,-1,false,{'changed while hidden'})
assert(not a.status().shared,'hidden edit retained context')
vim.api.nvim_set_current_buf(hidden)
a.share(1,1)
assert(vim.wait(2000,function() return a.status().shared ~= nil end,10),'context before bdelete')
vim.cmd('bdelete! '..hidden)
assert(not vim.api.nvim_buf_is_loaded(hidden) and not a.status().shared,'bdelete retained context')
local nextbuf=vim.api.nvim_create_buf(true,false)
vim.api.nvim_set_current_buf(nextbuf)
vim.api.nvim_buf_set_lines(nextbuf,0,-1,false,{'backpressure'})
a.share(1,1)
assert(vim.wait(2000,function() return a.status().shared ~= nil end,10),'backpressure share')
blocked=true
vim.api.nvim_buf_set_lines(nextbuf,0,-1,false,{'must revoke'})
assert(not a.status().connected,'failed invalidate enqueue kept attachment')
blocked=false
a.attach(%s,{editor=true})
assert(vim.wait(2000,function() return a.status().connected end,10),'second attachment')
a.share(1,1)
assert(vim.wait(2000,function() return a.status().shared ~= nil end,10),'second share')
failed=true
vim.api.nvim_buf_set_lines(nextbuf,0,-1,false,{'async write failure'})
assert(vim.wait(2000,function() return not a.status().connected end,10),'failed invalidate write kept attachment')
vim.fn.writefile({vim.json.encode({passed=true})},%s)
vim.cmd('qa!')
""" % (json.dumps(str(plugin)), json.dumps(str(endpoint)), json.dumps(str(root / "selection.rs")),
       json.dumps(str(root / "renamed.rs")), json.dumps(str(root / "hidden.rs")), json.dumps(str(endpoint)), json.dumps(str(report))))
        child = subprocess.run(["nvim", "--headless", "--clean", "-l", str(lua)],
                               env=environment(root, root, True), capture_output=True, timeout=12)
        worker.join(timeout=2)
        (output / "nvim.stdout").write_bytes(child.stdout)
        (output / "nvim.stderr").write_bytes(child.stderr)
        (output / "observations.json").write_text(json.dumps(dict(events=events, errors=errors), indent=2))
        assert child.returncode == 0, child.stderr.decode(errors="replace")
        assert not worker.is_alive() and not errors, errors
        assert [row["sequence"] for row in events if row["connection"] == 0 and row["kind"] == "publish_context"] == [1, 2, 3, 4, 5, 6], events
        assert [row["sequence"] for row in events if row["connection"] == 0 and row["kind"] == "invalidate_context"] == [1, 2, 3, 4, 5], events
        assert sum(row["kind"] == "closed" for row in events) == 2, events
        assert json.loads(report.read_text())["passed"]
    result = dict(passed=7, cases=["delayed-ACK-and-one-pending-share", "rename-revokes-with-same-tick", "hidden-buffer-edit-revokes", "bdelete-revokes",
                                  "wipe-revokes-context", "invalidate-queue-full-disconnects", "invalidate-write-failure-disconnects"])
    (output / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output)
