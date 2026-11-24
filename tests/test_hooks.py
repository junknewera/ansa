"""Narrow tests for the opt-in JSONL hooks observer; no ANSA binary or provider."""

from contextlib import closing
import io
import json
import os
from pathlib import Path
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "integrations" / "ansa_hooks.py"
sys.path.insert(0, str(ADAPTER.parent))
import ansa_hooks as hooks


def event(kind, seq, **fields):
    return dict(dict(schema_version=1, type=kind, stream_sequence=seq,
                     session="SECRET_session", incarnation="SECRET_incarnation",
                     turn="SECRET_turn", request_id="request_1"), **fields)


def lines(*events):
    return b"".join(json.dumps(e).encode() + b"\n" for e in events)


class HooksTests(unittest.TestCase):
    def setUp(self):
        with_dir = tempfile.TemporaryDirectory(prefix="ansa-hooks-")
        self.addCleanup(with_dir.cleanup)
        self.destination = str(Path(with_dir.name) / "sink.sock")
        self.state = Path(with_dir.name) / "cursor.sqlite"
        self.sink = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.addCleanup(self.sink.close)

    def run_adapter(self, data, subscriptions=hooks.HOOKS, *, state=None):
        argv = [sys.executable, "-B", str(ADAPTER), "--socket", self.destination,
                "--state", str(self.state if state is None else state)]
        for subscription in subscriptions:
            argv += ["--subscribe", subscription]
        result = subprocess.run(argv, input=data, capture_output=True, timeout=8)
        self.assertEqual(result.stdout, b"")
        return result

    def receive(self):
        self.sink.setblocking(False)
        hooks_received = []
        while True:
            try:
                raw = self.sink.recv(1024)
            except BlockingIOError:
                return hooks_received
            self.assertLess(len(raw), 512)
            for canary in (b"SECRET_", b"SOURCE_VALUE", b"E2E_OK", b"read_file", b"request_1"):
                self.assertNotIn(canary, raw)
            hooks_received.append(json.loads(raw))

    def test_production_wire_shapes_and_payload_allowlist(self):
        self.sink.bind(self.destination)
        fixture = ROOT / "tests" / "parity" / "fixtures" / "e2e-checker-controls.json"
        recorded = json.loads(fixture.read_text(encoding="utf-8"))["pipeline-basic-1"]["value"]["events"]
        # Keep one captured tool pair so the whole batch fits the Unix queue.
        recorded = [e for e in recorded if e["stream_sequence"] not in {13, 14, 19, 20}]
        for e in recorded:
            e.update(prompt="SECRET_prompt", args={"item": "SECRET_argument"}, token="SECRET_token")
        result = self.run_adapter(lines(*recorded))
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        opened, accepted, started, finished, done = self.receive()
        self.assertEqual([e["event"] for e in (opened, accepted, started, finished, done)],
                         ["session_opened", "command_accepted", "tool_started", "tool_finished", "done"])
        base = {"hook_version", "event", "stream_id", "session_id", "stream_sequence"}
        self.assertEqual(set(opened), base)
        self.assertEqual(set(accepted), base | {"turn_id"})
        self.assertEqual(set(done), set(accepted))
        self.assertEqual(set(started), base | {"turn_id", "operation_id"})
        self.assertEqual(set(finished), set(started) | {"execution", "effects"})
        self.assertEqual((finished["execution"], finished["effects"]), ("succeeded", "known_result"))
        self.assertEqual(started["operation_id"], finished["operation_id"])
        self.assertEqual(accepted["turn_id"], done["turn_id"])
        self.assertEqual(opened["session_id"], done["session_id"])
        self.assertEqual([e["stream_sequence"] for e in (opened, accepted, started, finished, done)],
                         [1, 2, 7, 8, 25])

    def test_replay_across_restarts_and_new_incarnation(self):
        self.sink.bind(self.destination)
        start = event("snapshot", 1, through_sequence=1, command_id=None, state="idle", request_id=None)
        accepted = event("command_accepted", 2, command_id="submit")
        done = event("done", 4)
        data = lines(start, accepted, done, done, accepted)
        first = self.run_adapter(data)
        self.assertEqual(first.returncode, 0, first.stderr)
        original = self.receive()
        self.assertEqual([e["stream_sequence"] for e in original], [1, 2, 4])
        replay = self.run_adapter(data + lines(event("command_replayed", 5, state="completed")))
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertEqual(self.receive(), [])
        opened = event("snapshot", 1, through_sequence=1, state="completed", command_id=None,
                       incarnation="restarted", tools=[{"output": "SECRET_old_result"}])
        next_data = lines(opened, event("command_replayed", 2, incarnation="restarted"),
                          event("snapshot", 3, through_sequence=3, incarnation="restarted",
                                command_id="resync", state="completed"),
                          event("cancelled", 4, incarnation="restarted", request_id="request_2"))
        result = self.run_adapter(next_data)
        self.assertEqual(result.returncode, 0, result.stderr)
        new_start, cancelled = self.receive()
        self.assertEqual((new_start["event"], cancelled["event"]), ("session_opened", "cancelled"))
        self.assertEqual(new_start["session_id"], original[0]["session_id"])
        self.assertNotEqual(new_start["stream_id"], original[0]["stream_id"])
        self.assertNotIn(b"SECRET_", self.state.read_bytes())

    def test_explicit_subscription_filter_and_no_backfill(self):
        self.sink.bind(self.destination)
        data = lines(event("command_accepted", 2), event("done", 3))
        self.assertEqual(self.run_adapter(data, ()).returncode, 2)
        self.assertFalse(self.state.exists())
        self.assertEqual(self.run_adapter(data, ("done",)).returncode, 0)
        self.assertEqual([e["event"] for e in self.receive()], ["done"])
        self.assertEqual(self.run_adapter(data, ("command_accepted",)).returncode, 0)
        self.assertEqual(self.receive(), [])

    def test_bad_frames_are_bounded_and_do_not_prevent_later_delivery(self):
        self.sink.bind(self.destination)
        # The suffix of the over-limit line looks like a valid frame.
        data = b" " * (hooks.MAX_FRAME + 1) + lines(event("done", 20))
        data += b'{"not-json":"SECRET_canary"\n\xff\n'
        data += b'{"schema_version":1,"schema_version":1,"type":"done"}\n'
        data += lines(event("done", 2, schema_version=True), event("done", 3, schema_version=2),
                      event("done", True), event("done", 0),
                      event("tool_finished", 4, operation_id="op:1", execution="SECRET_bad",
                            effects="known_result"),
                      event("resync_required", 5, oldest_available=6), event("done", 4),
                      event("done", 6))
        result = self.run_adapter(data + b'{"type":"partial"')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.splitlines(), [b"ansa-hooks: frame_limit_or_partial",
                                                      b"ansa-hooks: invalid_frame",
                                                      b"ansa-hooks: stream_gap"])
        self.assertEqual([e["stream_sequence"] for e in self.receive()], [6])

    def test_delivery_failure_is_not_retried_after_receiver_starts(self):
        failed = self.run_adapter(lines(event("done", 4), event("done", 5)))
        self.assertEqual((failed.returncode, failed.stderr), (1, b"ansa-hooks: delivery_failed\n"))
        self.sink.bind(self.destination)
        result = self.run_adapter(lines(event("done", 4), event("done", 5), event("done", 6)))
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        self.assertEqual([e["stream_sequence"] for e in self.receive()], [6])

    def test_timeout_drops_only_the_claimed_event(self):
        sink = Mock()
        sink.sendto.side_effect = [TimeoutError("SECRET_timeout"), OSError("SECRET_error")]
        report = Mock()
        with closing(sqlite3.connect(":memory:", isolation_level=None)) as db:
            source = lines(event("done", 4), event("done", 4), event("done", 5))
            key = hooks.create_state(db)
            hooks.observe(io.BytesIO(source), db, sink, self.destination, {"done"}, report, key=key)
            self.assertEqual(sink.sendto.call_count, 2)
            self.assertEqual([c.args for c in report.call_args_list],
                             [("delivery_timeout",), ("delivery_failed",)])

    def assert_state_rejected(self, *, state=None):
        result = self.run_adapter(lines(event("done", 4)), state=state)
        self.assertEqual((result.returncode, result.stderr), (1, b"ansa-hooks: state_unavailable\n"))
        self.assertEqual(self.receive(), [])

    def assert_no_sidecars(self, path):
        for suffix in ("-journal", "-wal", "-shm"):
            self.assertFalse(os.path.lexists(str(path) + suffix))

    def test_state_failure_prevents_delivery(self):
        self.sink.bind(self.destination)
        self.state.write_bytes(b"not sqlite")
        self.state.chmod(0o600)
        self.assert_state_rejected()
        self.assertEqual(self.state.read_bytes(), b"not sqlite")
        self.assert_no_sidecars(self.state)

    def test_state_symlink_and_hardlink_do_not_change_target(self):
        self.sink.bind(self.destination)
        target = self.state.with_name("other.sqlite")
        result = self.run_adapter(b"", state=target)
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        before = target.read_bytes()
        for make_link in (os.symlink, os.link):
            with self.subTest(link=make_link.__name__):
                make_link(target, self.state)
                inode = self.state.lstat().st_ino
                self.assert_state_rejected()
                self.assertEqual(self.state.lstat().st_ino, inode)
                self.assertEqual(target.read_bytes(), before)
                self.assert_no_sidecars(target)
                self.assert_no_sidecars(self.state)
                self.state.unlink()

    def test_state_directory_symlink_is_rejected_at_any_depth(self):
        self.sink.bind(self.destination)
        result = self.run_adapter(b"")
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        before = self.state.read_bytes()
        directory = self.state.parent / "private"
        directory.mkdir(0o700)
        parent_link = self.state.parent / "parent-link"
        parent_link.symlink_to(self.state.parent, target_is_directory=True)
        self.assert_state_rejected(state=parent_link / self.state.name)
        self.assertEqual(self.state.read_bytes(), before)
        self.assert_no_sidecars(self.state)
        self.assert_state_rejected(state=parent_link / directory.name / self.state.name)
        self.assertEqual(list(directory.iterdir()), [])

    def test_state_permissions_and_writable_ancestor_are_rejected(self):
        self.sink.bind(self.destination)
        result = self.run_adapter(b"")
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        self.assertEqual(stat.S_IMODE(self.state.stat().st_mode), 0o600)
        before = self.state.read_bytes()
        self.state.chmod(0o644)
        self.assert_state_rejected()
        self.assertEqual(stat.S_IMODE(self.state.stat().st_mode), 0o644)
        self.state.chmod(0o600)

        parent = self.state.parent
        child = parent / "private"
        child.mkdir(0o700)
        self.addCleanup(parent.chmod, 0o700)
        parent.chmod(0o755)
        self.assert_state_rejected()
        parent.chmod(0o777)
        self.assert_state_rejected(state=child / self.state.name)
        self.assertEqual(list(child.iterdir()), [])
        self.assertEqual(self.state.read_bytes(), before)
        self.assert_no_sidecars(self.state)

    def test_unrelated_existing_sqlite_is_not_adopted(self):
        self.sink.bind(self.destination)
        for setup in (None, "VACUUM", "CREATE TABLE unrelated (value TEXT)"):
            with self.subTest(setup=setup):
                self.state.touch(mode=0o600)
                if setup is not None:
                    with closing(sqlite3.connect(str(self.state), isolation_level=None)) as db:
                        db.execute(setup)
                before = self.state.read_bytes()
                self.assert_state_rejected()
                self.assertEqual(self.state.read_bytes(), before)
                self.assert_no_sidecars(self.state)
                self.state.unlink()

    def test_matching_marker_with_extra_schema_is_rejected(self):
        self.sink.bind(self.destination)
        result = self.run_adapter(b"")
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        with closing(sqlite3.connect(str(self.state), isolation_level=None)) as db:
            db.execute("CREATE TABLE unrelated (value TEXT)")
        before = self.state.read_bytes()
        self.assert_state_rejected()
        self.assertEqual(self.state.read_bytes(), before)
        self.assert_no_sidecars(self.state)

    def test_existing_sidecars_are_not_touched(self):
        self.sink.bind(self.destination)
        result = self.run_adapter(b"")
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        before = self.state.read_bytes()
        target = self.state.with_name("sidecar-target")
        target.write_bytes(b"leave this alone")
        for suffix in ("-journal", "-wal", "-shm"):
            with self.subTest(suffix=suffix):
                sidecar = Path(str(self.state) + suffix)
                sidecar.symlink_to(target)
                self.assert_state_rejected()
                self.assertEqual(Path(os.readlink(sidecar)), target)
                self.assertEqual(target.read_bytes(), b"leave this alone")
                self.assertEqual(self.state.read_bytes(), before)
                self.assertEqual(list(self.state.parent.glob(self.state.name + "-*")), [sidecar])
                sidecar.unlink()

    def test_existing_journal_blocks_creation_of_missing_state(self):
        self.sink.bind(self.destination)
        sidecar = Path(str(self.state) + "-journal")
        sidecar.write_bytes(b"existing journal")
        self.assert_state_rejected()
        self.assertFalse(os.path.lexists(self.state))
        self.assertEqual(sidecar.read_bytes(), b"existing journal")

    def test_legacy_state_key_and_cursor_are_preserved(self):
        self.sink.bind(self.destination)
        key = b"k" * 32
        first = event("done", 4)
        existing = hooks.project(first, key)
        self.state.touch(mode=0o600)
        with closing(sqlite3.connect(str(self.state), isolation_level=None)) as db:
            db.execute("CREATE TABLE hook_key (id INTEGER PRIMARY KEY, key BLOB NOT NULL)")
            db.execute("CREATE TABLE cursor (stream TEXT PRIMARY KEY, seq BLOB NOT NULL)")
            db.execute("INSERT INTO hook_key VALUES (1, ?)", (key,))
            db.execute("INSERT INTO cursor VALUES (?, ?)", (existing["stream_id"], b"\0" * 7 + b"\x04"))
            db.execute("PRAGMA application_id=1095652168")  # ANSH, as written by v1
        next_event = event("done", 5)
        result = self.run_adapter(lines(first, next_event))
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        self.assertEqual(self.receive(), [hooks.project(next_event, key)])

    def test_state_filename_with_uri_metacharacters(self):
        self.sink.bind(self.destination)
        state = self.state.with_name("cursor%23?mode=memory#v1.sqlite")
        data = lines(event("done", 4))
        result = self.run_adapter(data, state=state)
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        self.assertEqual([e["stream_sequence"] for e in self.receive()], [4])
        self.assertTrue(state.is_file())
        result = self.run_adapter(data, state=state)
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        self.assertEqual(self.receive(), [])

    def test_framing_allows_limit_plus_newline_and_resumes_after_drain(self):
        with patch.object(hooks, "MAX_FRAME", 4), patch.object(hooks, "DRAIN_CHUNK", 3):
            source = io.BytesIO(b"1234\nabcdefg\n123\nend")
            self.assertEqual(list(hooks.frames(source)), [b"1234\n", None, b"123\n", None])


if __name__ == "__main__":
    unittest.main()
