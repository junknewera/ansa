"""Editor task admission through the production Carbon runtime and journal."""
import argparse
import hashlib
import json
from pathlib import Path
import signal
import subprocess
import tempfile

from carbon_workspace_pty import start, local, ready, until, records
from cli_progress_pty import roots
from parity.frontend_io import environment, journals
from workspace_bridge import Client, snapshots


def response(client, kind, **fields):
    id = client.command(kind, **fields)
    client.until(lambda rows: any(row.get("id") == id for row in rows))
    return next(row for row in client.messages if row.get("id") == id)


def run(binary, output):
    output.mkdir(parents=True, exist_ok=False)
    cases = []
    with tempfile.TemporaryDirectory(prefix="ansa-messages-", dir="/tmp") as directory:
        root = Path(directory)
        home, cwd = roots(root)
        endpoint = root / "bridge"
        screen = start(binary, root, home, cwd, argv_extra=["--workspace-dir", str(endpoint), "--workspace-editor"])
        clients = []
        try:
            viewer = Client(endpoint / "workspace.sock", "readonly")
            editor = Client(endpoint / "workspace.sock", "sender", "editor")
            clients += [viewer, editor]
            editor.until(lambda rows: any(row["state"].get("phase") != "loading" for row in snapshots(rows)))
            before = journals(root)
            for kind in ["submit_message", "message_lookup", "cancel_message"]:
                answer = response(viewer, kind, message="m_1", text="FORBIDDEN", turn="m_1", target=None, expected_mode="basic")
                assert answer["code"] == "workspace_permission_denied", answer
            assert journals(root) == before
            cases.append("viewer-cannot-send-inspect-or-cancel")

            text = "LITERAL_EDITOR_TASK\n/exit\nЖ界"
            body = dict(text=text, turn="m_1", target=None, expected_mode="basic")
            def post(fields):
                editor.send(dict(version=1, session=editor.welcome["session"], incarnation=editor.welcome["incarnation"],
                                 id="m_1", kind="submit_message", **fields))
                editor.until(lambda rows: any(row.get("type") == "message_status" and row.get("id") == "m_1" for row in rows))
                row = next(row for row in editor.messages if row.get("type") == "message_status" and row.get("id") == "m_1")
                editor.messages.remove(row)
                return row
            assert post(body)["status"] == "Accepted"
            until(screen, lambda _: any(r["kind"] == "done" for r in records(root)) and ready(screen), "editor task committed")
            receipt = response(editor, "message_lookup", message="m_1")
            info = receipt["receipt"]
            assert info["kind"] == "submit" and info["mode"] == "basic" and info["state"] == "completed", receipt
            assert info["admission_durable"] and info["state_durable"] and info["text_sha256"] == hashlib.sha256(text.encode()).hexdigest()
            assert "text" not in info and "tools" not in info
            accepted = [r for r in records(root) if r["kind"] == "accepted"]
            assert len(accepted) == 1 and accepted[0]["text"] == text
            cases.append("typed-editor-submit-keeps-slash-as-data-and-commits-once")
            before = journals(root)
            assert post(body)["status"] == "Duplicate"
            assert post(dict(body, text="CONFLICTING_TASK"))["status"] == "CommandConflict"
            assert journals(root) == before
            cases.append("duplicate-and-conflict-do-not-execute-or-append")

            editor.close()
            clients.remove(editor)
            editor = Client(endpoint / "workspace.sock", "sender", "editor")
            clients.append(editor)
            again = response(editor, "message_lookup", message="m_1")
            assert again["command"] == receipt["command"] and again["receipt"] == info
            absent = response(editor, "message_lookup", message="m_absent")
            assert absent["receipt"] is None and absent["ready"]
            assert journals(root) == before
            assert response(editor, "cancel_message", message="m_1", turn="wrong", target=info["request"])["code"] == "workspace_target_unconfirmed"
            cases.append("reconnect-lookup-preserves-identity-and-rejects-wrong-cancel")

            local(screen, "/plan")
            until(screen, lambda text: "План" in text and ready(screen), "Carbon plan mode")
            next_id = f"r_{editor.sequence + 1}"
            denied = response(editor, "submit_message", text="NO_MODE_ESCALATION", turn=next_id, target=None, expected_mode="basic")
            assert denied["code"] == "workspace_mode_changed", denied
            assert journals(root) == before
            cases.append("editor-cannot-escalate-carbon-plan-mode")

            plugin = Path(__file__).resolve().parents[1] / "integrations/ansa.nvim"
            script, report = root / "send.lua", root / "report.json"
            script.write_text("""vim.opt.runtimepath:prepend(%s)
local a=require('ansa'); a.setup(); a.attach(%s,{editor=true})
assert(vim.wait(3000,function() local s=a.status(); return s.snapshot and s.snapshot.state.input_mode=='plan' end,10),'plan attachment')
local b=vim.api.nvim_create_buf(true,false); vim.api.nvim_set_current_buf(b)
vim.api.nvim_buf_set_name(b,%s)
vim.api.nvim_buf_set_lines(b,0,-1,false,{'UNSAVED_NEOVIM_TASK','/exit','Ж界'})
a.send(1,3,'Объясни выделение')
assert(vim.wait(4000,function() local s=a.status(); return s.last_message and not s.pending_message end,10),'durable send confirmation')
local sent=vim.deepcopy(a.status().last_message)
assert(sent.receipt.mode=='plan' and sent.receipt.admission_durable,'mode or admission proof')
assert(sent.text:find('\\n/exit\\n',1,true),'selection newlines were escaped')
vim.cmd('AnsaDetach'); a.attach(%s,{editor=true})
assert(vim.wait(3000,function() return a.status().snapshot ~= nil end,10),'reconnect')
vim.cmd('AnsaPending')
assert(vim.wait(3000,function() return a.status().last_message.receipt.state=='completed' end,10),'receipt after reconnect')
assert(a.status().last_message.id==sent.id and not a.status().pending_message,'reconnect replaced message')
assert(vim.api.nvim_buf_get_lines(b,0,-1,false)[1]=='UNSAVED_NEOVIM_TASK','source changed')
vim.fn.writefile({vim.json.encode({passed=true,text=sent.text,id=sent.id})},%s)
vim.cmd('qa!')
""" % (json.dumps(str(plugin)), json.dumps(str(endpoint / "workspace.sock")), json.dumps(str(cwd / 'selection.rs')),
       json.dumps(str(endpoint / "workspace.sock")), json.dumps(str(report))))
            child = subprocess.run(["nvim", "--headless", "--clean", "-l", str(script)],
                                   env=environment(home, root, True), capture_output=True, timeout=15)
            (output / "nvim.stdout").write_bytes(child.stdout)
            (output / "nvim.stderr").write_bytes(child.stderr)
            assert child.returncode == 0, child.stderr.decode(errors="replace")
            sent = json.loads(report.read_text())
            accepted = [r for r in records(root) if r["kind"] == "accepted"]
            assert len(accepted) == 2 and accepted[-1]["text"] == sent["text"] and accepted[-1]["mode"] == "plan", accepted
            assert not (cwd / "selection.rs").exists(), "selection was implicitly saved"
            cases.append("real-neovim-selection-submit-plan-reconnect-without-resend")
            screen.finish(signal.SIGTERM)
            assert not (endpoint / "workspace.sock").exists()
            cases.append("shutdown-joins-clients-and-removes-owned-socket")
        finally:
            for client in clients:
                client.close()
            if screen.child.poll() is None:
                screen.finish(signal.SIGKILL)
            (output / "terminal.raw").write_bytes(screen.raw)
    result = dict(passed=len(cases), cases=cases, binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    (output / "report.json").write_text(json.dumps(result, ensure_ascii=False, indent=2))
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--ansa", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    run(args.ansa.resolve(), args.output)
