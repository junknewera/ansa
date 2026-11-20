"""Exercise Carbon bridge v1 line frames on real, connected AF_UNIX sockets."""

import hashlib
import json
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from workspace import FRAME_LIMIT, WorkspaceFeed


IDENTITY = {"session": "s_fixture", "incarnation": "i_fixture"}
CAPS = ["snapshot", "history", "artifact_catalog", "artifact_read", "document_read"]


def line(value):
    return json.dumps(value, ensure_ascii=False).encode() + b"\n"


def welcome(caps):
    return dict(type="welcome", version=1, role="viewer", max_frame_bytes=131072,
                capabilities=caps, **IDENTITY)


def snapshot(cursor):
    return dict(type="snapshot", version=1, through=cursor, **IDENTITY, state={
        "phase": "completed", "turn": "t_1", "request": "req_1", "submit": "c_1",
        "durable": True, "busy": False, "input_mode": "basic", "mode": "basic",
        "transcript_total": 2, "timeline_total": 4, "diagnostic": "",
        "text": "Готово", "preview": None,
    })


class WorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.directory = Path(self.tmp.name)
        self.finished = threading.Event()
        self.errors = []
        self.seen = []
        self.events = []
        self.feed = WorkspaceFeed(self.directory)

    def serve(self, callback):
        client, server = socket.socketpair()
        connector = patch("workspace._connect", return_value=client)
        connector.start()
        self.addCleanup(connector.stop)

        def run():
            try:
                with server as conn:
                    conn.settimeout(4)
                    with conn.makefile("rb") as reader:
                        def receive():
                            data = json.loads(reader.readline())
                            self.seen.append(data)
                            return data
                        callback(conn, receive)
                    self.finished.wait(5)
            except Exception as error:
                self.errors.append(error)

        worker = threading.Thread(target=run, daemon=True)
        worker.start()
        self.addCleanup(worker.join, 1)
        self.addCleanup(self.finished.set)
        self.feed.start()
        self.addCleanup(self.feed.close)

    def until(self, kind, predicate=lambda payload: True):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.errors:
                raise self.errors[0]
            self.events.extend(self.feed.poll())
            for name, payload in reversed(self.events):
                if name == kind and predicate(payload):
                    return payload
            time.sleep(.01)
        self.fail(f"Missing {kind}, received {self.events!r}")

    def test_history_proposal_and_artifact_use_only_granted_viewer_reads(self):
        content = "month,total\n12,190\n"
        digest = hashlib.sha256(content.encode()).hexdigest()
        artifact = {
            "artifact_id": "a_0000000000000000_table", "revision": "b" * 64,
            "session_id": IDENTITY["session"], "producer_operation_id": "op_python",
            "mime": "text/csv", "size_bytes": len(content), "sha256": digest,
            "title": "table.csv", "storage_ref": "b" * 64 + "/table.csv",
        }
        document = {
            "document_id": "d_" + hashlib.sha256(b"op_edit").hexdigest(),
            "session_id": IDENTITY["session"], "kind": "diff",
            "path": "src/week.py", "filetype": "python", "first_line": 42,
        }
        full_document = dict(document, producer_operation_id="op_edit", before="old()\n",
                             after="new()\n", client="editor1", editor_incarnation="e1",
                             buffer=2, changedtick=7, sequence=1)
        history = [dict(index=n, role=role, text=text, request="req_1",
                        turn="t_1", state="committed") for n, (role, text) in enumerate(
                            [("user", "Проверь отчёт"), ("assistant", "Готово")])]
        def protocol(conn, receive):
            self.assertEqual(receive(), {"version": 1, "role": "viewer", "client": "form_zh3"})
            data = line(welcome(CAPS)) + line(snapshot(5))
            conn.sendall(data[:47])
            conn.sendall(data[47:] + line(snapshot(4)) + line(dict(snapshot(6), session="old")))
            self.assertEqual(receive()["kind"], "snapshot")
            read = receive()
            self.assertEqual(read["kind"], "history")
            conn.sendall(line(dict(type="posted", kind="history", id=read["id"], status="QueueFull")))
            read = receive()
            self.assertEqual(read["kind"], "history")
            self.assertEqual((read["before"], read["forward"], read["limit"]), (0, False, 64))
            page = dict(type="history", id=read["id"], begin=0, end=2, total=2, **IDENTITY, entries=history)
            conn.sendall(line(dict(page, incarnation="old", entries=[])) + line(page))
            cat = receive()
            self.assertEqual(cat["kind"], "artifact_catalog")
            conn.sendall(line(dict(type="artifact_catalog", id=cat["id"], **IDENTITY, begin=0,
                                   end=4, total=4, documents=[document], artifacts=[artifact])))
            doc_req = receive()
            self.assertEqual((doc_req["kind"], doc_req["document_id"]), ("document_read", document["document_id"]))
            conn.sendall(line(dict(type="document", id=doc_req["id"], document=full_document)))
            art_req = receive()
            self.assertEqual((art_req["kind"], art_req["revision"]), ("artifact_read", artifact["revision"]))
            conn.sendall(line(dict(type="artifact", id=art_req["id"], descriptor=artifact,
                                   availability="available", path="/opaque/fixture", content=content)))
        self.serve(protocol)
        doc = self.until("document")
        self.assertEqual(doc["source"], "old()\n")
        self.assertIn("+new()", doc["diff"])
        self.assertIn("@@ -42 +42 @@", doc["diff"])
        self.assertIn("op_edit", doc["evidence"])
        self.assertEqual(self.until("snapshot")["through"], 5)
        self.assertEqual(self.until("history")["entries"], history)
        self.assertEqual(self.until("connection")["capabilities"], CAPS)
        self.assertFalse(self.feed.read_document("ungranted"))
        self.assertFalse(self.feed.read_artifact(artifact["artifact_id"], "older"))
        self.assertTrue(self.feed.read_artifact(artifact["artifact_id"], artifact["revision"]))
        csv = self.until("document", lambda d: d["kind"] == "text/csv")
        self.assertEqual(csv["source"], content)
        self.assertIn(digest, csv["evidence"])
        self.assertTrue(all(frame.get(k) == v for frame in self.seen[1:] for k, v in IDENTITY.items()))

    def test_oversized_history_falls_back_to_smaller_page_without_catalog(self):
        def protocol(conn, receive):
            receive()
            conn.sendall(line(welcome(["snapshot", "history"])) + line(snapshot(1)))
            self.assertEqual(receive()["kind"], "snapshot")
            self.assertEqual(receive()["limit"], 64)
            conn.sendall(line({"type": "error", "code": "workspace_output_limit"}))
            retry = receive()
            self.assertEqual(retry["limit"], 16)
            conn.sendall(line(dict(type="history", id=retry["id"], **IDENTITY, begin=0, end=2,
                                   total=2, entries=[])))
        self.serve(protocol)
        self.until("history")
        self.assertFalse(self.feed.read_document("d_123"))
        self.assertNotIn("artifact_catalog", [r.get("kind") for r in self.seen])

    def test_rejects_oversized_frame(self):
        with patch("workspace.RETRY_DELAYS", ()):
            def protocol(conn, receive):
                receive()
                conn.sendall(b"x" * FRAME_LIMIT)
            self.serve(protocol)
            self.assertIn("512 KiB", self.until("connection", lambda e: e["status"] == "disconnected")["detail"])

    def test_rejects_public_endpoint(self):
        with patch("workspace.RETRY_DELAYS", ()):
            self.directory.chmod(0o755)
            self.feed.start()
            self.addCleanup(self.feed.close)
            self.assertIn("owner-private", self.until("connection", lambda e: e["status"] == "disconnected")["detail"])


if __name__ == "__main__":
    unittest.main()
