"""Read-only Carbon v1 Unix-socket viewer. No Carbon session is started or modified.

WorkspaceFeed.poll() returns (kind, dict) pairs. Replacements coalesce between
polls. 'connection' reports transport status, detail, and on attach the session,
incarnation and capabilities. 'snapshot' is the v1 snapshot; 'history' is the
latest history page (user/assistant entries); 'artifacts' is the latest committed
catalog page (descriptors and prepared-document metadata). 'document' carries
key, name, kind, source, diff, evidence, session and incarnation from a granted
read. The latest prepared document or text artifact is read automatically.
read_document() and read_artifact() select another grant. 'notice' carries a
read error in 'detail'. Carbon v1 does not export arbitrary tool or agent rows.
"""

import difflib
import io
import json
import os
from pathlib import Path
import re
import socket
import stat
import struct
import threading
import time


FRAME_LIMIT = 512 * 1024  # Bridge::OUTPUT_BYTES; includes the newline.
PAGE_SIZE = 64
READ_TIMEOUT = 3.0
RETRY_DELAYS = (0.25, 0.75, 2.0)


def _check_endpoint(directory):
    for path, kind, mode in (
        (directory, stat.S_ISDIR, 0o700),
        (directory / "workspace.sock", stat.S_ISSOCK, 0o600),
    ):
        info = path.lstat()
        if (not kind(info.st_mode) or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != mode):
            raise PermissionError(f"Expected an owner-private endpoint: {path}")


def _send(sock, frame):
    sock.sendall(json.dumps(frame, separators=(",", ":")).encode() + b"\n")


def _connect(directory):
    _check_endpoint(directory)
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.settimeout(0.2)
        sock.connect(os.fspath(directory / "workspace.sock"))
        return sock
    except OSError:
        sock.close()
        raise


def _editor_diff(document):
    start_offset = document["first_line"] - 1
    lines = difflib.unified_diff(
        list(io.StringIO(document["before"])), list(io.StringIO(document["after"])),
        fromfile="original", tofile="proposal",
    )
    result = []
    for line in lines:
        if start_offset and line.startswith("@@ "):
            line = re.sub(r"([+-])([0-9]+)", lambda m: m[1] + str(int(m[2]) + start_offset), line)
        result.append(line)
        if not line.endswith("\n"):
            result.append("\n\\ No newline at end of file\n")
    return "".join(result)


def _number(value):
    return type(value) is int and value >= 0


def _check_page(frame, kind):
    bounds = (frame.get("begin"), frame.get("end"), frame.get("total"))
    if not all(_number(n) for n in bounds) or not bounds[0] <= bounds[1] <= bounds[2]:
        raise ValueError("Invalid Carbon page bounds")
    fields = ("entries",) if kind == "history" else ("artifacts", "documents")
    for name in fields:
        rows = frame.get(name)
        if not isinstance(rows, list) or not all(isinstance(v, dict) for v in rows):
            raise ValueError("Invalid Carbon page rows")
    if kind == "history" and any(
        e.get("role") not in ("user", "assistant") or not isinstance(e.get("text"), str)
        for e in frame["entries"]
    ):
        raise ValueError("Invalid Carbon transcript entry")


class _Attachment:
    """One connection. The bridge allows one outstanding page request per client."""

    def __init__(self, sock, emit, take_selection, set_grants):
        self.sock = sock
        self.emit = emit
        self.take_selection = take_selection
        self.set_grants = set_grants
        self.identity = {}
        self.caps = set()
        self.cursor = -1
        self.sequence = 0
        self.deadline = time.monotonic() + READ_TIMEOUT
        self.requests = ("history", "artifact_catalog")
        self.target = dict.fromkeys(self.requests, -1)
        self.loaded = dict.fromkeys(self.requests, -1)
        self.limits = dict.fromkeys(self.requests, PAGE_SIZE)
        self.pending = None
        self.retry = 0
        self.next_read = 0
        self.grants = {}
        self.auto_key = None
        self.waiting_asset = None
        self.asset = None

    def request(self, kind, **fields):
        self.sequence += 1
        key = f"r_{self.sequence}"
        _send(self.sock, dict(version=1, id=key, kind=kind, **self.identity, **fields))
        return key

    def read_page(self, now):
        if self.cursor < 0 and now >= self.deadline:
            raise TimeoutError("No workspace welcome or initial snapshot")
        if self.asset and now >= self.asset[2]:
            self.emit("notice", {"detail": "Selected workspace document did not arrive"})
            self.asset = None
        if not self.asset:
            selected = self.take_selection() or self.waiting_asset
            self.waiting_asset = None
            if selected in self.grants:
                if selected[0] == "document":
                    request_id = self.request("document_read", document_id=selected[1])
                else:
                    request_id = self.request("artifact_read", artifact_id=selected[1], revision=selected[2])
                self.asset = selected, request_id, now + READ_TIMEOUT
        if self.pending:
            if now >= self.deadline:
                self.failed_page("workspace_history_timeout")
            return
        if now < self.next_read:
            return
        for kind in self.requests:
            if kind in self.caps and self.target[kind] > self.loaded[kind]:
                key = self.request(kind, before=0, limit=self.limits[kind], forward=False)
                self.pending = (kind, key, self.target[kind])
                self.deadline = now + READ_TIMEOUT
                break

    def failed_page(self, reason):
        kind, _, _ = self.pending
        self.pending = None
        if reason == "workspace_output_limit" and self.limits[kind] > 1:
            self.limits[kind] = max(1, self.limits[kind] // 4)
        elif reason in ("QueueFull", "workspace_busy", "workspace_history_timeout") and self.retry < len(RETRY_DELAYS):
            self.next_read = time.monotonic() + RETRY_DELAYS[self.retry]
            self.retry += 1
        else:
            self.loaded[kind] = self.target[kind]
            self.retry = 0
            self.emit("notice", {"detail": f"{kind}: {reason}"})

    def catalog(self, page):
        self.grants.clear()
        if "artifact_read" in self.caps:
            for desc in page["artifacts"]:
                if (isinstance(desc, dict) and desc.get("session_id") == self.identity["session"]
                        and desc.get("mime") in ("text/markdown", "text/csv")
                        and all(isinstance(desc.get(k), str) for k in ("artifact_id", "revision", "title", "sha256"))):
                    self.grants["artifact", desc["artifact_id"], desc["revision"]] = desc
        if "document_read" in self.caps:
            for doc in page["documents"]:
                if (isinstance(doc, dict) and doc.get("session_id") == self.identity["session"]
                        and isinstance(doc.get("document_id"), str) and doc.get("kind") in ("diff", "source")
                        and _number(doc.get("first_line")) and doc["first_line"] > 0
                        and (doc.get("path") is None or isinstance(doc["path"], str))):
                    self.grants["document", doc["document_id"]] = doc
        self.set_grants(set(self.grants))
        latest = next(reversed(self.grants), None)
        if latest != self.auto_key:
            self.auto_key = self.waiting_asset = latest

    def accept_asset(self, frame):
        key = self.asset[0]
        self.asset = None
        descriptor = self.grants.get(key)
        if descriptor is None:
            return
        if frame.get("type") == "error":
            self.emit("notice", {"detail": str(frame.get("code", "workspace_error"))})
            return
        result = dict(self.identity, key="/".join(key), diff="")
        if key[0] == "document":
            doc = frame.get("document")
            if (frame.get("type") != "document" or not isinstance(doc, dict)
                    or any(doc.get(k) != v for k, v in descriptor.items())
                    or not all(isinstance(doc.get(k), str) for k in ("before", "after", "producer_operation_id"))):
                raise ValueError("Document differs from its catalog grant")
            name = doc.get("path") or "Editor selection"
            result.update(name=name, kind=doc["kind"], source=doc["before"], before=doc["before"],
                          after=doc["after"], first_line=doc["first_line"])
            if doc["kind"] == "diff":
                result["diff"] = _editor_diff(doc)
            result["evidence"] = (f"Editor {doc['kind']} · line {doc['first_line']}\n"
                                  f"Operation {doc['producer_operation_id']}\n{doc['document_id']}")
        else:
            if (frame.get("type") != "artifact" or frame.get("availability") != "available"
                    or frame.get("descriptor") != descriptor or not isinstance(frame.get("content"), str)):
                raise ValueError("Artifact differs from its catalog grant")
            result.update(name=descriptor["title"], kind=descriptor["mime"], source=frame["content"], format=descriptor["mime"],
                          evidence=f"Revision {descriptor['revision']}\nSHA-256 {descriptor['sha256']}")
        self.emit("document", result)

    def accept(self, frame):
        if not isinstance(frame, dict):
            raise ValueError("Invalid workspace frame")
        kind = frame.get("type")
        if not self.identity:
            caps = frame.get("capabilities")
            if (kind != "welcome" or frame.get("version") != 1 or frame.get("role") != "viewer"
                    or not isinstance(caps, list) or not all(isinstance(c, str) for c in caps)
                    or not all(isinstance(frame.get(k), str) and frame[k] for k in ("session", "incarnation"))):
                raise ValueError("Invalid Carbon v1 viewer welcome")
            self.identity = {k: frame[k] for k in ("session", "incarnation")}
            self.caps = set(caps)
            if "snapshot" not in self.caps:
                raise ValueError("Workspace has no snapshot capability")
            self.emit("connection", dict(status="connected", detail="", capabilities=caps, **self.identity))
            self.request("snapshot")
            return

        if kind in ("snapshot", "history", "artifact_catalog"):
            if any(frame.get(k) != v for k, v in self.identity.items()):
                return
        if self.asset and frame.get("id") == self.asset[1]:
            self.accept_asset(frame)
            return
        if kind == "snapshot":
            cursor, state = frame.get("through"), frame.get("state")
            if frame.get("version") != 1 or not _number(cursor) or not isinstance(state, dict):
                raise ValueError("Invalid Carbon snapshot")
            if cursor <= self.cursor:
                return
            self.cursor = cursor
            for name, field in (("history", "transcript_total"), ("artifact_catalog", "timeline_total")):
                if _number(state.get(field)):
                    self.target[name] = state[field]
            self.emit("snapshot", frame)
        elif self.pending and frame.get("id") == self.pending[1]:
            expected, _, minimum_total = self.pending
            if kind == "posted" and frame.get("status") != "Accepted":
                self.failed_page(str(frame.get("status")))
            elif kind == "error":
                self.failed_page(str(frame.get("code")))
            elif kind == expected:
                _check_page(frame, expected)
                if frame["total"] < minimum_total:
                    self.failed_page("workspace_history_timeout")
                    return
                self.loaded[expected] = frame["total"]
                self.pending = None
                self.retry = 0
                self.next_read = 0
                self.requests = tuple(k for k in self.requests if k != expected) + (expected,)
                if expected == "artifact_catalog":
                    self.catalog(frame)
                self.emit("history" if expected == "history" else "artifacts", frame)
        elif kind == "error" and self.pending and not self.asset and frame.get("code") == "workspace_output_limit":
            self.failed_page("workspace_output_limit")
        elif kind == "error":
            self.emit("notice", {"detail": str(frame.get("code", "workspace_error"))})


class WorkspaceFeed:
    def __init__(self, directory):
        self.directory = Path(directory).expanduser()
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._latest = {}
        self._thread = None
        self._selection = None
        self._grants = set()

    def start(self):
        if self._thread is None or not self._thread.is_alive():
            self._stop.clear()
            self._thread = threading.Thread(target=self._run, name="carbon-viewer", daemon=True)
            self._thread.start()

    def close(self):
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=1)
        with self._lock:
            self._grants.clear()
            self._selection = None

    def poll(self):
        with self._lock:
            result = list(self._latest.items())
            self._latest.clear()
        return result

    def read_document(self, document_id):
        return self._select(("document", document_id))

    def read_artifact(self, artifact_id, revision):
        return self._select(("artifact", artifact_id, revision))

    def _select(self, key):
        with self._lock:
            if key not in self._grants:
                return False
            self._selection = key
            return True

    def _take_selection(self):
        with self._lock:
            key, self._selection = self._selection, None
            return key

    def _set_grants(self, keys):
        with self._lock:
            self._grants = keys

    def _emit(self, kind, payload):
        with self._lock:
            if kind == "connection":
                self._grants.clear()
                self._selection = None
                if payload["status"] == "connected":
                    self._latest.clear()
            self._latest.pop(kind, None)
            self._latest[kind] = payload

    def _session(self):
        with _connect(self.directory) as sock:
            sock.settimeout(0.2)
            if hasattr(socket, "SO_PEERCRED"):
                _, uid, _ = struct.unpack("3i", sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                if uid != os.getuid():
                    raise PermissionError("Workspace peer belongs to another user")
            _send(sock, {"version": 1, "role": "viewer", "client": "form_zh3"})
            session = _Attachment(sock, self._emit, self._take_selection, self._set_grants)
            buffer = bytearray()
            partial_since = None
            while not self._stop.is_set():
                now = time.monotonic()
                session.read_page(now)
                if partial_since is not None and now - partial_since > READ_TIMEOUT:
                    raise TimeoutError("Incomplete workspace frame")
                try:
                    block = sock.recv(65536)
                except socket.timeout:
                    continue
                if not block:
                    raise ConnectionError("Workspace closed the connection")
                buffer.extend(block)
                while (end := buffer.find(b"\n")) >= 0:
                    if end >= FRAME_LIMIT:
                        raise ValueError("Workspace frame exceeds 512 KiB")
                    frame = json.loads(buffer[:end].decode("utf-8"))
                    del buffer[:end + 1]
                    session.accept(frame)
                    partial_since = None
                if len(buffer) >= FRAME_LIMIT:
                    raise ValueError("Workspace frame exceeds 512 KiB")
                if buffer and partial_since is None:
                    partial_since = now

    def _run(self):
        for attempt in range(len(RETRY_DELAYS) + 1):
            try:
                self._session()
                return
            except (OSError, ValueError, RecursionError) as exc:
                if self._stop.is_set():
                    return
                last = attempt == len(RETRY_DELAYS)
                self._emit("connection", {
                    "status": "disconnected" if last else "reconnecting", "detail": str(exc),
                })
                if last or self._stop.wait(RETRY_DELAYS[attempt]):
                    return
