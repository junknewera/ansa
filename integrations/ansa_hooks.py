#!/usr/bin/env python3
"""Opt-in observer for trusted ANSA v1 JSONL stdout (Linux, stdlib only)."""

import argparse
from contextlib import ExitStack, closing, contextmanager
import hmac
import json
import os
import re
import socket
import sqlite3
import stat
import sys
from urllib.parse import quote_from_bytes

# The Rust wire codec uses Json::FRAME_BYTES; allow an additional newline.
MAX_FRAME = 16 * 1024 * 1024
DRAIN_CHUNK = 64 * 1024
SEND_TIMEOUT = 0.1
STATE_TIMEOUT = 0.25
STATE_ID = 0x414E5348  # SQLite application_id: ANSH
STATE_MAX_PAGES = 4096
STATE_SIDECARS = ("-journal", "-wal", "-shm")
KEY_SCHEMA = "CREATE TABLE hook_key (id INTEGER PRIMARY KEY, key BLOB NOT NULL)"
CURSOR_SCHEMA = "CREATE TABLE cursor (stream TEXT PRIMARY KEY, seq BLOB NOT NULL)"
UINT64_MAX = (1 << 64) - 1
ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
OPERATION = re.compile(r"[A-Za-z0-9_:-]{1,256}")
TURN_EVENTS = ("command_accepted", "done", "cancelled", "tool_started", "tool_finished")
HOOKS = ("session_opened", *TURN_EVENTS)
EXECUTIONS = ("succeeded", "failed", "cancelled", "denied")
EFFECTS = ("not_started", "known_result", "effect_unknown")


def frames(source):
    """Yield newline-terminated frames; None marks an oversized or partial line."""
    while line := source.readline(MAX_FRAME + 1):
        if line.endswith(b"\n"):
            yield line
            del line
        else:
            # Do not parse the suffix as a separate event after an oversized line.
            del line
            while chunk := source.readline(DRAIN_CHUNK):
                if chunk.endswith(b"\n"):
                    break
            yield None


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError
        obj[key] = value
    return obj


def parse_int(token):
    if len(token) > 20:
        raise ValueError
    return int(token)


def reject_constant(_token):
    raise ValueError


def is_id(value, pattern=ID):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def alias(key, domain, *parts):
    # Only validated ASCII IDs (no NUL) are used as components.
    data = "\0".join((domain, *parts)).encode("ascii")
    return hmac.digest(key, data, "sha256").hex()


def project(event, key):
    """Select lifecycle edges only, with an explicit outbound field allowlist."""
    if not isinstance(event, dict):
        raise ValueError
    if type(event.get("schema_version")) is not int or event["schema_version"] != 1:
        raise ValueError
    kind = event.get("type")
    if not isinstance(kind, str):
        raise ValueError
    if kind not in (*TURN_EVENTS, "snapshot", "resync_required"):
        # Includes command_replayed, history pages, previews, and diagnostics.
        return None
    seq = event.get("stream_sequence")
    session, incarnation, turn, request = (
        event.get(field) for field in ("session", "incarnation", "turn", "request_id")
    )
    if (type(seq) is not int or not 1 <= seq <= UINT64_MAX
            or not all(is_id(v) for v in (session, incarnation, turn))
            or "request_id" not in event or (request is not None and not is_id(request))):
        raise ValueError
    if kind == "snapshot":
        if (seq != 1 or event.get("command_id") is not None
                or type(event.get("through_sequence")) is not int
                or event["through_sequence"] != seq):
            return None
        # Opening an incarnation is observable even when it restores an old session.
        # Never read the snapshot's retained turns, preview or tool activity.
        kind = "session_opened"
    elif kind != "resync_required" and request is None:
        raise ValueError

    hook = {
        "hook_version": 1,
        "event": kind,
        "stream_id": alias(key, "stream", session, incarnation),
        "session_id": alias(key, "session", session),
        "stream_sequence": seq,
    }
    if kind in TURN_EVENTS:
        hook["turn_id"] = alias(key, "turn", session, turn, request)
    if kind in ("tool_started", "tool_finished"):
        operation = event.get("operation_id")
        if not is_id(operation, OPERATION):
            raise ValueError
        hook["operation_id"] = alias(key, "operation", session, operation)
    if kind == "tool_finished":
        execution, effects = event.get("execution"), event.get("effects")
        if execution not in EXECUTIONS or effects not in EFFECTS:
            raise ValueError
        hook.update(execution=execution, effects=effects)
    return hook


@contextmanager
def state_parent(path):
    parts = path.split("/")
    if (parts[0] or any(part in ("", ".", "..") for part in parts[1:])
            or parts[-1].lower().endswith(STATE_SIDECARS)):
        raise sqlite3.DatabaseError
    uid = os.geteuid()
    flags = os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW
    fd = os.open("/", flags)
    try:
        # SQLite reopens by pathname, so every entry must be protected against
        # replacement by other users. In a trusted sticky directory (e.g. /tmp),
        # the next directory is protected by checking its owner as well.
        for index, part in enumerate(parts[:-1]):
            if index:
                child = os.open(part, flags, dir_fd=fd)
                os.close(fd)
                fd = child
            info = os.fstat(fd)
            if (not stat.S_ISDIR(info.st_mode) or info.st_uid not in (0, uid)
                    or (info.st_mode & 0o022 and not info.st_mode & stat.S_ISVTX)):
                raise sqlite3.DatabaseError
        if info.st_uid != uid or stat.S_IMODE(info.st_mode) != 0o700:
            raise sqlite3.DatabaseError
        yield fd, parts[-1]
    finally:
        os.close(fd)


def reject_sidecars(parent, name):
    for suffix in STATE_SIDECARS:
        try:
            os.stat(name + suffix, dir_fd=parent, follow_symlinks=False)
        except FileNotFoundError:
            continue
        # Do not open, recover, or remove any pre-existing journal/WAL/SHM.
        raise sqlite3.DatabaseError


def check_state_file(info):
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
            or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1):
        raise sqlite3.DatabaseError


def reserve_or_check_state(parent, name):
    try:
        info = os.stat(name, dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError:
        fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=parent)
        try:
            check_state_file(os.fstat(fd))
        finally:
            os.close(fd)
        return True

    check_state_file(info)
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
    try:
        info = os.fstat(fd)
        check_state_file(info)
        header = os.read(fd, 100)
    finally:
        os.close(fd)
    # Require a bounded, marked SQLite rollback-mode file before invoking SQLite.
    # In SQLite's header, a stored page size of 1 encodes 65536 bytes.
    page_size = int.from_bytes(header[16:18], "big")
    if page_size == 1:
        page_size = 65536
    if (len(header) != 100 or header[:16] != b"SQLite format 3\0"
            or header[18:20] != b"\x01\x01"
            or int.from_bytes(header[68:72], "big") != STATE_ID
            or not 512 <= page_size <= 65536 or page_size & (page_size - 1)
            or not page_size <= info.st_size <= page_size * STATE_MAX_PAGES
            or info.st_size % page_size):
        raise sqlite3.DatabaseError
    return False


def state_key(db):
    rows = db.execute("SELECT id, CASE WHEN typeof(key)='blob' AND length(key)=32"
                      " THEN key END FROM main.hook_key LIMIT 2").fetchall()
    if len(rows) != 1 or rows[0][0] != 1 or rows[0][1] is None:
        raise sqlite3.DatabaseError
    return rows[0][1]


def create_state(db):
    key = os.urandom(32)
    with db:
        db.execute("BEGIN IMMEDIATE")
        db.execute(KEY_SCHEMA)
        db.execute("INSERT INTO hook_key VALUES (1, ?)", (key,))
        db.execute(CURSOR_SCHEMA)
        db.execute(f"PRAGMA application_id={STATE_ID}")
    return key


@contextmanager
def open_state(path):
    uri = "file:" + quote_from_bytes(os.fsencode(path))
    options = dict(uri=True, timeout=STATE_TIMEOUT, isolation_level=None)
    with ExitStack() as stack:
        try:
            parent, name = stack.enter_context(state_parent(path))
            reject_sidecars(parent, name)
            new = reserve_or_check_state(parent, name)
            if not new:
                # Immutable read-only probing cannot create sidecars or recover a
                # journal. Check the exact v1 schema (including absence of triggers).
                with closing(sqlite3.connect(uri + "?mode=ro&immutable=1", **options)) as probe:
                    (pages,) = probe.execute("PRAGMA page_count").fetchone()
                    if not 1 <= pages <= STATE_MAX_PAGES:
                        raise sqlite3.DatabaseError
                    rows = probe.execute(
                        "SELECT CASE"
                        " WHEN type='table' AND name='hook_key' AND tbl_name=name AND sql=? THEN 1"
                        " WHEN type='table' AND name='cursor' AND tbl_name=name AND sql=? THEN 2"
                        " WHEN type='index' AND name='sqlite_autoindex_cursor_1'"
                        " AND tbl_name='cursor' AND sql IS NULL THEN 3 ELSE 0 END"
                        " FROM main.sqlite_master LIMIT 4",
                        (KEY_SCHEMA, CURSOR_SCHEMA),
                    ).fetchall()
                    if sorted(rows) != [(1,), (2,), (3,)]:
                        raise sqlite3.DatabaseError
                    checked_key = state_key(probe)
            # An observer may have started a transaction during the probe.
            reject_sidecars(parent, name)
            # rw, not rwc: only the exclusive reservation above may create a file.
            db = stack.enter_context(closing(sqlite3.connect(uri + "?mode=rw", **options)))
            db.execute("PRAGMA synchronous=FULL")
            # Fail closed when this reaches its limit instead of evicting replay cursors.
            db.execute(f"PRAGMA max_page_count={STATE_MAX_PAGES}")
            key = create_state(db) if new else state_key(db)
            if not new and key != checked_key:
                raise sqlite3.DatabaseError
        except OSError as exc:
            raise sqlite3.DatabaseError from exc
        yield db, key


def consume(db, stream, seq):
    # Fixed-width unsigned big endian encoding preserves the u64 order in SQLite.
    # A single autocommitted statement claims the sequence even with two readers.
    return db.execute(
        "INSERT INTO cursor VALUES (?, ?) ON CONFLICT(stream) DO UPDATE SET seq=excluded.seq"
        " WHERE cursor.seq < excluded.seq",
        (stream, seq.to_bytes(8, "big")),
    ).rowcount == 1


def observe(source, db, sink, destination, subscriptions, report, *, key):
    for line in frames(source):
        if line is None:
            report("frame_limit_or_partial")
            continue
        try:
            event = json.loads(line.decode("utf-8"), object_pairs_hook=unique_object,
                               parse_int=parse_int, parse_constant=reject_constant)
            hook = project(event, key)
        except (ValueError, RecursionError):
            report("invalid_frame")
            continue
        finally:
            # Only the allowlisted projection is used below.
            line = event = None
        if hook is None or not consume(db, hook["stream_id"], hook["stream_sequence"]):
            continue
        if hook["event"] == "resync_required":
            report("stream_gap")
        if hook["event"] not in subscriptions:
            continue
        data = json.dumps(hook, separators=(",", ":")).encode("ascii")
        try:
            # One local datagram per event. Never wait for a response or retry.
            if sink.sendto(data, destination) != len(data):
                report("delivery_failed")
        except (socket.timeout, TimeoutError):
            report("delivery_timeout")
        except OSError:
            report("delivery_failed")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--subscribe", action="append", choices=HOOKS, required=True,
                        help="repeat for each hook event to deliver")
    parser.add_argument("--socket", required=True, help="absolute Unix datagram socket path")
    parser.add_argument("--state", required=True, help="absolute path to a private SQLite cursor file")
    args = parser.parse_args(argv)
    if not os.path.isabs(args.socket) or len(os.fsencode(args.socket)) > 107:
        parser.error("--socket must be an absolute filesystem path of at most 107 bytes")
    if not os.path.isabs(args.state):
        parser.error("--state must be an absolute file path")
    if sys.stdin.isatty():
        parser.error("connect ANSA v1 JSONL stdout to stdin")
    reported = set()

    def report(code):
        if code not in reported:
            reported.add(code)
            print(f"ansa-hooks: {code}", file=sys.stderr)

    os.umask(0o077)
    try:
        with open_state(args.state) as (db, key):
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as sink:
                sink.settimeout(SEND_TIMEOUT)
                observe(sys.stdin.buffer, db, sink, args.socket, set(args.subscribe), report, key=key)
    except sqlite3.Error:
        report("state_unavailable")
    except OSError:
        report("io_unavailable")
    except KeyboardInterrupt:
        return 130
    return int(bool(reported))


if __name__ == "__main__":
    sys.exit(main())
