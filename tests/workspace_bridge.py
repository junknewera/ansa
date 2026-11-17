"""Private workspace IPC and a real adjacent Neovim against offline Carbon."""
import argparse
import hashlib
import json
from pathlib import Path
import select
import signal
import socket
import subprocess
import tempfile
import time

from carbon_workspace_pty import start, local, ready, until, visible, records
from cli_progress_pty import roots
from parity.frontend_io import environment, journals


class Client:
    def __init__(self, path, name="viewer"):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.connect(str(path))
        self.socket.setblocking(False)
        self.buffer = b""
        self.messages = []
        self.sequence = 0
        self.send(dict(version=1, role="viewer", client=name))
        self.until(lambda rows: any(row.get("type") == "welcome" for row in rows))
        self.welcome = next(row for row in self.messages if row.get("type") == "welcome")

    def send(self, value):
        self.socket.sendall(json.dumps(value, ensure_ascii=False).encode() + b"\n")

    def command(self, kind, **fields):
        self.sequence += 1
        self.send(dict(version=1, session=self.welcome["session"],
                       incarnation=self.welcome["incarnation"], id=f"r_{self.sequence}", kind=kind, **fields))
        return f"r_{self.sequence}"

    def drain(self):
        if select.select([self.socket], [], [], .01)[0]:
            chunk = self.socket.recv(65536)
            assert chunk, "workspace unexpectedly closed"
            self.buffer += chunk
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                assert len(line) < 512 * 1024
                self.messages.append(json.loads(line))

    def until(self, predicate, timeout=4):
        deadline = time.monotonic() + timeout
        while not predicate(self.messages):
            assert time.monotonic() < deadline, self.messages[-5:]
            self.drain()

    def close(self):
        self.socket.close()

    def history_reply(self, id, **query):
        # The runtime has a bounded read mailbox: definite QueueFull may be
        # retried as a new read; an accepted request must get its own page.
        for _ in range(4):
            self.until(lambda rows: any(row.get("type") == "posted" and row.get("id") == id for row in rows))
            posted = next(row for row in self.messages if row.get("type") == "posted" and row.get("id") == id)
            if posted["status"] == "QueueFull":
                time.sleep(.025)
                id = self.command("history", **query)
                continue
            assert posted["status"] == "Accepted", posted
            self.until(lambda rows: any(row.get("type") == "history" and row.get("id") == id for row in rows))
            return next(row for row in self.messages if row.get("type") == "history" and row.get("id") == id)
        raise AssertionError("bounded history retries exhausted")


def snapshots(rows):
    return [row for row in rows if row.get("type") == "snapshot"]


def run(binary, output):
    output.mkdir(parents=True, exist_ok=False)
    observations = []
    begin = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as directory:
        root = Path(directory)
        home, cwd = roots(root)
        endpoint = root / "bridge"
        screen = start(binary, root, home, cwd, argv_extra=["--workspace-dir", str(endpoint)])
        clients = []
        try:
            first = Client(endpoint / "workspace.sock", "first")
            second = Client(endpoint / "workspace.sock", "second")
            clients = [first, second]
            assert first.welcome["session"] == second.welcome["session"]
            assert first.welcome["incarnation"] == second.welcome["incarnation"]
            assert (endpoint.stat().st_mode & 0o777) == 0o700
            assert ((endpoint / "workspace.sock").stat().st_mode & 0o777) == 0o600
            before = journals(root)
            first.command("submit", text="MUST_NOT_EXECUTE", mode="basic")
            first.until(lambda rows: any(row.get("code") == "workspace_permission_denied" for row in rows))
            assert journals(root) == before
            observations.append("private-socket-viewer-denied")

            text = "BRIDGE_SHARED_Привет_界"
            screen.send(text + "\r")
            until(screen, lambda _: any(record["kind"] == "done" for record in records(root)) and ready(screen), "shared turn committed")
            for client in clients:
                client.command("snapshot")
                client.command("history", before=0, limit=64, forward=False)
                client.until(lambda rows: any(
                    any(entry["text"] == text for entry in row.get("entries", []))
                    for row in rows if row.get("type") == "history")
                    and any(row["state"].get("phase") == "completed" for row in snapshots(rows)))
                assert all("tools" not in row.get("state", {}) for row in snapshots(client.messages))
            accepted = [record for record in records(root) if record["kind"] == "accepted"]
            assert len(accepted) == 1 and accepted[0]["text"] == text
            observations.append("two-viewers-one-runtime-one-journal")

            # Different pages must remain per-client even while Carbon copies.
            before = journals(root)
            first_id = first.command("history", before=1, limit=1, forward=False)
            second_id = second.command("history", before=0, limit=1, forward=False)
            screen.send(b"\x19")
            a = first.history_reply(first_id, before=1, limit=1, forward=False)
            b = second.history_reply(second_id, before=0, limit=1, forward=False)
            assert a["entries"][0]["text"] == text and b["entries"][0]["role"] == "assistant"
            assert a["begin"] != b["begin"] and journals(root) == before
            observations.append("correlated-pages-isolated-from-other-viewer-and-carbon-copy")

            # The plugin is loaded without the person's editor configuration.
            plugin = Path(__file__).resolve().parents[1] / "integrations/ansa.nvim"
            lua = root / "viewer.lua"
            report = root / "nvim-report.json"
            lua.write_text("""local api = vim.api
vim.opt.runtimepath:prepend(%s)
local ansa = require('ansa')
ansa.setup()
ansa.attach(%s)
assert(vim.wait(4000, function()
  local s = ansa.status()
  return s.connected and s.snapshot and s.snapshot.state.phase == 'completed'
    and s.buffer and table.concat(api.nvim_buf_get_lines(s.buffer, 0, -1, false), '\\n'):find('BRIDGE_SHARED_', 1, true)
end, 10), 'Neovim viewer did not receive the shared transcript')
ansa.open()
local buf = ansa.status().buffer
local cursor = {2, 0}
api.nvim_win_set_cursor(0, cursor)
vim.bo[buf].modifiable = true
api.nvim_buf_set_lines(buf, 0, 0, false, {'DIRTY_VIEWER_DRAFT'})
vim.bo[buf].modified = true
local original = api.nvim_buf_get_lines(buf, 0, -1, false)
vim.cmd('AnsaDetach')
ansa.attach(%s)
local reconnect_ok = vim.wait(4000, function()
  local s = ansa.status()
  return s.connected and s.buffer ~= buf and s.snapshot
end, 10)
if not reconnect_ok then
  vim.fn.writefile({vim.inspect(ansa.status()), vim.api.nvim_exec2('messages', {output=true}).output}, %s)
end
assert(reconnect_ok, 'dirty viewer was overwritten or no replacement was published')
assert(vim.deep_equal(original, api.nvim_buf_get_lines(buf, 0, -1, false)), 'dirty buffer changed')
assert(vim.bo[buf].modified, 'dirty flag cleared')
assert(api.nvim_get_current_buf() == buf, 'attachment moved the current buffer')
assert(api.nvim_win_get_cursor(0)[1] == 3, 'attachment moved the cursor')
vim.fn.writefile({vim.json.encode({passed=true, dirty_preserved=true, current_buffer_preserved=true})}, %s)
vim.cmd('qa!')
""" % (json.dumps(str(plugin)), json.dumps(str(endpoint / "workspace.sock")),
       json.dumps(str(endpoint / "workspace.sock")), json.dumps(str(report) + '.debug'), json.dumps(str(report))))
            child = subprocess.run(["nvim", "--headless", "--clean", "-l", str(lua)],
                                   env=environment(home, root, True), capture_output=True, timeout=12)
            (output / "nvim.stdout").write_bytes(child.stdout)
            (output / "nvim.stderr").write_bytes(child.stderr)
            if Path(str(report) + '.debug').exists():
                (output / "nvim.debug").write_text(Path(str(report) + '.debug').read_text())
            assert child.returncode == 0, child.stderr.decode(errors="replace")
            assert json.loads(report.read_text())["passed"]
            observations.append("real-neovim-unicode-history-dirty-cursor-reconnect")

            first.close()
            clients.remove(first)
            replacement = Client(endpoint / "workspace.sock", "first")
            clients.append(replacement)
            replacement.until(lambda rows: any(row["state"].get("phase") == "completed" for row in snapshots(rows)))
            assert len([record for record in records(root) if record["kind"] == "accepted"]) == 1
            observations.append("viewer-reconnect-does-not-reexecute")

            before = journals(root)
            local(screen, "/fork")
            until(screen, lambda _: len(list((home / ".ansa/sessions").iterdir())) >= 2 and ready(screen), "fork created")
            second.command("snapshot")
            second.until(lambda rows: len(snapshots(rows)) >= 2)
            assert all(row["session"] == first.welcome["session"] for row in snapshots(second.messages))
            assert journals(root)[next(iter(before))] == before[next(iter(before))]
            observations.append("attachment-remains-on-root-across-fork")

            result = screen.finish(signal.SIGTERM)
            assert result["exit"] == 143 and result["termios_restored"] and result["flags_restored"], result
            assert not (endpoint / "workspace.sock").exists(), "owned socket leaked"
            observations.append("shutdown-cleans-socket-restores-terminal")
        finally:
            for client in clients:
                client.close()
            (output / "carbon.pty").write_bytes(screen.raw)
            (output / "carbon.screen.txt").write_text(visible(screen))
            (output / "journals.json").write_text(json.dumps(journals(root)))
            for index, client in enumerate(clients):
                (output / f"viewer-{index}.json").write_text(json.dumps(client.messages, ensure_ascii=False, indent=2))
            screen.close()
    result = dict(passed=len(observations), cases=observations, duration=time.monotonic() - begin,
                  binary_sha256=hashlib.sha256(Path(binary).read_bytes()).hexdigest())
    (output / "report.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    run(str(args.binary.resolve()), args.output)
