"""Lost editor submission ACKs, bounded status reads, conflicts and mode races."""
import argparse
import hashlib
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading

from parity.frontend_io import environment


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    events, errors, bodies, clients = [], [], {}, []
    with tempfile.TemporaryDirectory(prefix="ansa-send-faults-", dir="/tmp") as directory:
        root = Path(directory)
        endpoint = root / "workspace.sock"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(endpoint))
        listener.listen(2)
        listener.settimeout(10)

        def serve():
            try:
                for connection in range(2):
                    stream, _ = listener.accept()
                    with stream:
                        stream.settimeout(10)
                        pipe = stream.makefile("rb")
                        hello = json.loads(pipe.readline())
                        assert hello["role"] == "editor"
                        clients.append(hello["client"])
                        def emit(frame):
                            stream.sendall(json.dumps(frame).encode() + b"\n")
                        def snapshot(cursor):
                            emit(dict(type="snapshot", version=1, session="original", incarnation="inc_a", through=cursor,
                                      state=dict(phase="idle", busy=False, input_mode="basic", mode="basic", durable=True, transcript_total=0, preview=None)))
                        emit(dict(type="welcome", version=1, role="editor", session="original", incarnation="inc_a"))
                        snapshot(1)
                        for line in pipe:
                            request = json.loads(line)
                            kind, id = request["kind"], request["id"]
                            events.append(dict(connection=connection, kind=kind, id=id, message=request.get("message")))
                            if kind == "snapshot":
                                emit(dict(type="posted", id=id, kind=kind, status="Accepted"))
                            elif kind == "history":
                                emit(dict(type="posted", id=id, kind=kind, status="Accepted"))
                                emit(dict(type="history", id=id, session="original", incarnation="inc_a", begin=0, end=0, total=0, entries=[]))
                            elif kind == "publish_context":
                                c = request["context"]
                                ack = dict(type="context_published", id=id, buffer=c["buffer"], changedtick=c["changedtick"], sequence=c["sequence"], mode="basic")
                                if c["sequence"] == 5:
                                    fresh = dict(type="snapshot", version=1, session="original", incarnation="inc_a", through=100,
                                                 state=dict(phase="generating", busy=True, turn="root_turn", request="root_request", input_mode="basic", mode="basic", transcript_total=0))
                                    stream.sendall((json.dumps(ack) + "\n" + json.dumps(fresh) + "\n").encode())
                                else:
                                    emit(ack)
                            elif kind == "submit_message":
                                assert id not in bodies, "client automatically repeated a mutating request"
                                bodies[id] = request
                                if connection == 0:
                                    break  # The request was received, but its ACK is lost.
                                if id == "m_4":
                                    emit(dict(type="error", id=id, code="workspace_mode_changed"))
                                else:
                                    if id == "m_5":
                                        assert request["turn"] == "root_turn" and request["target"] == "root_request", request
                                    emit(dict(type="message_status", id=id, status="CommandConflict" if id == "m_3" else "Unknown"))
                            elif kind == "message_lookup":
                                message = request["message"]
                                receipt = None
                                if message in ["m_1", "m_3", "m_5"]:
                                    original = bodies[message]
                                    receipt = dict(kind="steer" if message == "m_5" else "submit",
                                                   turn="root_turn" if message == "m_5" else "n_" + hashlib.sha256(hello["client"].encode()).hexdigest()[:16] + "_" + message,
                                                   request="root_request" if message == "m_5" else "runtime_request", state="completed", mode="basic", admission_durable=True, state_durable=True,
                                                   text_sha256=hashlib.sha256(("OTHER_BODY" if message == "m_3" else original["text"]).encode()).hexdigest())
                                emit(dict(type="message_receipt", id=id, message=message, session="original", incarnation="inc_a", ready=True,
                                          storage_failed=False, recovery_required=False, receipt=receipt))
                                if receipt is None:
                                    for cursor in range(2, 30):
                                        snapshot(cursor)
                            elif kind == "invalidate_context":
                                emit(dict(type="context_invalidated", id=id))
                            else:
                                raise AssertionError(request)
                        pipe.close()
            except BaseException as error:
                errors.append(repr(error))
            finally:
                listener.close()

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        plugin = Path(__file__).resolve().parents[1] / "integrations/ansa.nvim"
        script = root / "test.lua"
        script.write_text("""vim.opt.runtimepath:prepend(%s)
local a=require('ansa'); a.setup()
local endpoint=%s
local function attach()
  a.attach(endpoint,{editor=true})
  assert(vim.wait(2000,function() return a.status().snapshot ~= nil end,10),'attachment')
end
attach()
local b=vim.api.nvim_create_buf(true,false); vim.api.nvim_set_current_buf(b)
vim.api.nvim_buf_set_lines(b,0,-1,false,{'SELECTED Ж界','/exit'})
a.send(1,2,'LOST_ACK')
assert(vim.wait(2000,function() local s=a.status(); return not s.connected and s.pending_message end,10),'lost ACK did not retain pending')
local frozen=vim.deepcopy(a.status().pending_message)
assert(frozen.phase=='unknown' and frozen.id=='m_1','pending identity lost')
attach()
assert(vim.wait(2000,function() return a.status().last_message and not a.status().pending_message end,10),'positive reconnect lookup')
assert(a.status().last_message.text==frozen.text and a.status().last_message.id==frozen.id,'reconnect changed frozen request')
a.send(1,2,'UNKNOWN_TASK')
assert(vim.wait(3000,function() local p=a.status().pending_message; return p and p.id=='m_2' and p.phase=='unknown' end,10),'unknown send')
vim.wait(1600,function() return false end,10)
assert(a.status().pending_message and a.status().pending_message.text:find('UNKNOWN_TASK',1,true),'absent receipt discarded pending')
vim.cmd('AnsaForgetPending!')
assert(not a.status().pending_message,'explicit local forget')
a.send(1,2,'CONFLICT_TASK')
assert(vim.wait(2000,function() local p=a.status().pending_message; return p and p.phase=='conflict' end,10),'conflict falsely confirmed')
vim.cmd('AnsaForgetPending!')
a.send(1,2,'PRESERVED_MODE_DRAFT')
assert(vim.wait(2000,function() local p=a.status().pending_message; return p and p.id=='m_4' or not p and a.status().message_draft=='PRESERVED_MODE_DRAFT' end,10))
vim.wait(150,function() return false end,10)
assert(not a.status().pending_message and a.status().message_draft=='PRESERVED_MODE_DRAFT','mode rejection lost draft or kept false pending')
a.send(1,2,'FRESH_SNAPSHOT_ROUTING')
assert(vim.wait(2000,function() local m=a.status().last_message; return m and m.id=='m_5' and not a.status().pending_message end,10),'fresh projection steering confirmation')
assert(a.status().last_message.receipt.kind=='steer','ACK used stale idle projection')
vim.cmd('AnsaDetach')
vim.cmd('qa!')
""" % (json.dumps(str(plugin)), json.dumps(str(endpoint))))
        child = subprocess.run(["nvim", "--headless", "--clean", "-l", str(script)],
                               env=environment(root, root, True), capture_output=True, timeout=15)
        worker.join(timeout=2)
        (output / "nvim.stdout").write_bytes(child.stdout)
        (output / "nvim.stderr").write_bytes(child.stderr)
        (output / "observations.json").write_text(json.dumps(dict(events=events, errors=errors), indent=2))
        assert child.returncode == 0, child.stderr.decode(errors="replace")
        assert not worker.is_alive() and not errors, errors
        assert clients[0] == clients[1], "reconnect changed client namespace"
        assert list(bodies) == ["m_1", "m_2", "m_3", "m_4", "m_5"], bodies
        assert sum(r["kind"] == "message_lookup" and r["message"] == "m_2" for r in events) == 5, events
        assert not any(r["kind"] == "cancel_message" for r in events), "local forget cancelled runtime task"
    # Exercise cancellation while Steer is pending and again after its receipt.
    with tempfile.TemporaryDirectory(prefix="ansa-cancel-steer-", dir="/tmp") as directory:
        root = Path(directory)
        lua = root / "test.lua"
        lua.write_text("""vim.opt.runtimepath:prepend(%s)
local sent = {}
local notes = {}
local client = 'steering_editor'
local turn = 'n_' .. vim.fn.sha256(client):sub(1,16) .. '_m_1'
local attachment = {role='editor', session='same_session', incarnation='current'}
local snapshot = {state={phase='idle', input_mode='basic'}}
local message = require('ansa.messages').new({
  client=function() return client end,
  attachment=function() return attachment end,
  snapshot=function() return snapshot end,
  share=function(_,_,after) after({path='', first_line=1, text='selected'}, 'basic') end,
  notify=function(note) notes[#notes+1] = note end,
  send=function(request) sent[#sent+1] = request; return true, request.id or 'r_' .. #sent end,
})
local function confirm(kind)
  local pending = message.pending
  message:handle({type='message_status',id=pending.id,status='Accepted'})
  message:handle({type='message_receipt',id=message.lookup.id,session='same_session',incarnation='current',
    message=pending.id, ready=true, receipt={turn=turn, request='request', kind=kind, mode='basic',
      admission_durable=true, text_sha256=vim.fn.sha256(pending.text)}})
end
local function cancel_original()
  message:cancel()
  local request=sent[#sent]
  assert(request.kind=='cancel_message' and request.message=='m_1' and request.target=='request' and request.turn==turn)
  message:handle({type='message_status',id=message.cancel_request,status='Accepted'})
end
message:submit(1,1,'task')
confirm('submit')
snapshot.state={phase='generating',busy=true,turn=turn,request='request',input_mode='basic'}
message:submit(1,1,'clarification')
assert(message.pending.id=='m_2')
cancel_original()
confirm('steer')
assert(message.last.id=='m_2')
cancel_original()
snapshot.state.request='another_request'
local count=#sent
message:cancel()
assert(#sent==count and notes[#notes]:find('другая задача',1,true))
vim.cmd('qa!')
""" % json.dumps(str(plugin)))
        subprocess.run(["nvim", "--headless", "--clean", "-l", str(lua)], env=environment(root, root, True), check=True, timeout=5)
    result = dict(passed=7, cases=["lost-ACK-reconnect-read-only-confirmation", "frozen-multiline-payload-retained", "absent-receipt-bounded-five-reads",
                                  "conflicting-payload-never-confirmed", "mode-rejection-preserves-draft", "ACK-and-new-projection-route-to-current-request", "own-submit-cancellable-during-and-after-steer"])
    (output / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output)
