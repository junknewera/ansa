"""Real PTY acceptance of input recall, task timing and live tool projection."""
import argparse
import base64
import codecs
import json
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata

sys.path.insert(0, str(Path(__file__).resolve().parent / "parity"))
from frontend_io import Pty, environment, journals


class Terminal:
    """Independent minimal VT screen for the cursor/erase sequences emitted here."""
    def __init__(self, cols=80, rows=24):
        self.cols,self.rows=cols,rows; self.cells=[[' ']*cols for _ in range(rows)]
        self.x=self.y=0; self.saved=(0,0); self.pending=''; self.offset=0
        self.decoder=codecs.getincrementaldecoder('utf-8')('replace')
    def update(self, raw):
        self.pending+=self.decoder.decode(raw[self.offset:]);self.offset=len(raw)
        index=0
        while index<len(self.pending):
            ch=self.pending[index]
            if ch=='\x1b':
                if index+1>=len(self.pending):break
                if self.pending[index+1]=='[':
                    match=re.match(r'\x1b\[([0-9;?]*)([A-Za-z~])',self.pending[index:])
                    if not match:break
                    args=[int(p) if p.isdigit() else 0 for p in match[1].split(';')];n=args[0] or 1
                    cmd=match[2]
                    if cmd=='A':self.y=max(0,self.y-n)
                    elif cmd=='B':self.y=min(self.rows-1,self.y+n)
                    elif cmd=='C':self.x=min(self.cols-1,self.x+n)
                    elif cmd=='D':self.x=max(0,self.x-n)
                    elif cmd in ('H','f'):self.y=min(self.rows-1,n-1);self.x=min(self.cols-1,(args[1] if len(args)>1 else 1)-1)
                    elif cmd=='J' and args[0]==2:self.cells=[[' ']*self.cols for _ in range(self.rows)]
                    elif cmd=='K':
                        begin,end=(0,self.cols) if args[0]==2 else (0,self.x+1) if args[0]==1 else (self.x,self.cols)
                        self.cells[self.y][begin:end]=[' ']*(end-begin)
                    index+=len(match[0]);continue
                if self.pending[index+1]==']':
                    end=self.pending.find('\x07',index+2)
                    if end<0:break
                    index=end+1;continue
                if self.pending[index+1]=='7':self.saved=(self.x,self.y)
                elif self.pending[index+1]=='8':self.x,self.y=self.saved
                index+=2;continue
            if ch=='\r':self.x=0
            elif ch=='\n':
                self.y+=1
                if self.y>=self.rows:self.cells.pop(0);self.cells.append([' ']*self.cols);self.y=self.rows-1
            elif ord(ch)>=32:
                width=0 if unicodedata.combining(ch) else 2 if unicodedata.east_asian_width(ch) in ('W','F') else 1
                if width and self.x<self.cols:
                    self.cells[self.y][self.x]=ch
                    if width==2 and self.x+1<self.cols:self.cells[self.y][self.x+1]=''
                    self.x=min(self.cols-1,self.x+width)
            index+=1
        self.pending=self.pending[index:]
    def line(self, row):return ''.join(self.cells[row]).rstrip()


def editor(screen, text, since=0):
    if not hasattr(screen,'vt'):screen.vt=Terminal()
    deadline=time.monotonic()+4
    while True:
        screen.vt.update(screen.raw)
        if screen.vt.line(-2)==('> '+text).rstrip():return
        assert time.monotonic()<deadline and screen.child.poll() is None, ('editor',text,screen.vt.line(-2))
        screen.drain(.01)

def copied(screen, since):
    until=time.monotonic()+4
    while True:
        match=re.search(rb'\x1b\]52;c;([A-Za-z0-9+/=]*)\x07',screen.raw[since:])
        if match:return base64.b64decode(match[1],validate=True).decode()
        assert time.monotonic()<until and screen.child.poll() is None,'clipboard readiness'
        screen.drain(.01)

def frozen_body(terminal):
    return tuple(tuple(row) for row in terminal.cells[1:-4])


def save(screen, output, name, expected=(0,)):
    result = screen.finish()
    (output / (name + ".pty")).write_bytes(screen.raw)
    (output / (name + ".json")).write_text(json.dumps({k:v for k,v in result.items() if k != 'stdout_hex'},indent=2)+'\n')
    assert result["exit"] in expected, result["exit"]
    assert result["termios_restored"] and result["flags_restored"]
    return {"case": name, "passed": True, "exit": result["exit"],
            "termios_restored": True, "flags_restored": True}


def roots(root):
    (root / "fixture-owner").write_bytes(b"ansa-m4-synthetic-v1\n")
    (root / "fixture-owner").chmod(0o600)
    home = root / "home"
    cwd = home / "workspace"
    cwd.mkdir(parents=True)
    return home, cwd


def recall(binary, output):
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as name:
        root = Path(name)
        home, cwd = roots(root)
        session = None
        for index in range(40):
            args = [str(binary), "--offline", "--cwd", str(cwd)]
            if session:
                args += ["--session", session]
            result = subprocess.run([*args, f"seed-{index:02} Ж界😀"],
                                    env=environment(home, root, True), capture_output=True, timeout=5)
            assert result.returncode == 0 and result.stdout == b"OMH_TOOL_OK:OMH_FIXED_VALUE_42\n"
            session = re.search(rb"ansa session ([a-zA-Z0-9_-]+) ", result.stderr)[1].decode()
        argv = ["--offline", "--cwd", str(cwd), "--session", session]
        screen = Pty(str(binary), root, home, cwd, "", True, argv=argv)
        try:
            screen.wait("basic")
            screen.send("DRAFT Ж界😀")
            editor(screen, "DRAFT Ж界😀")
            assert screen.vt.line(0).startswith('ansa') and screen.vt.line(-1).startswith('basic')
            assert b'\x1b]52;' not in screen.raw
            at=len(screen.raw)
            before=journals(root)
            screen.send(b'\x19')
            expected=''.join(f'Вы:\nseed-{i:02} Ж界😀\n\nansa:\nOMH_TOOL_OK:OMH_FIXED_VALUE_42\n\n' for i in range(40))
            assert copied(screen,at)==expected
            editor(screen,'DRAFT Ж界😀')
            assert journals(root)==before,'copy changed canonical journal'
            # Browse every canonical page; input remains pinned and unchanged.
            for _ in range(50):
                screen.send(b'\x1b[5~')
                for _ in range(3):screen.drain(.01)
                screen.vt.update(screen.raw)
                if 'seed-00' in '\n'.join(screen.vt.line(i) for i in range(1,20)):break
            else:raise AssertionError('first canonical message is unreachable')
            editor(screen,'DRAFT Ж界😀')
            # Return through forward pages, then wait beyond periodic refresh.
            for _ in range(60):
                screen.send(b'\x1b[6~')
                for _ in range(3):screen.drain(.01)
                screen.vt.update(screen.raw)
                if 'seed-39' in '\n'.join(screen.vt.line(i) for i in range(1,20)):
                    screen.send(b'\x1b[6~');break
            else:raise AssertionError('tail is unreachable')
            until=time.monotonic()+.7
            while time.monotonic()<until:screen.drain(.01)
            screen.vt.update(screen.raw)
            assert 'seed-39' in '\n'.join(screen.vt.line(i) for i in range(1,20)),'polling lost tail'
            screen.send(b'\x06')
            screen.resize(100,32)
            screen.vt=Terminal(100,32)
            editor(screen,'DRAFT Ж界😀')
            assert screen.vt.line(0).startswith('ansa') and screen.vt.line(-1).startswith('basic')
            screen.resize(80,24)
            screen.vt=Terminal()
            editor(screen,'DRAFT Ж界😀')
            screen.send(b"\x1b[D\x1b[D")
            for index in reversed(range(40)):
                at = len(screen.raw)
                screen.send(b"\x1b[A")
                editor(screen, f"seed-{index:02} Ж界😀", at)
            for index in range(1, 40):
                at = len(screen.raw)
                screen.send(b"\x1b[B")
                editor(screen, f"seed-{index:02} Ж界😀", at)
            at = len(screen.raw)
            screen.send(b"\x1b[BX")
            editor(screen, "DRAFT ЖX界😀", at)
            # Overflow is a clear boundary, including any saved recall draft.
            screen.send(b"\x1b[A")
            editor(screen, "seed-39 Ж界😀", at)
            at = len(screen.raw)
            screen.send(b"x" * (17 * 1024) + b"\r")
            screen.wait("submit_size", at)
            editor(screen, "", at)
            at = len(screen.raw)
            screen.send(b"\x1b[B")
            for _ in range(8):
                screen.drain(.01)
            assert b"DRAFT" not in screen.raw[at:]
            screen.send(b"\x04")
            row = save(screen, output, "recall-pages-overflow", (1,))
        finally:
            (output / "recall-live.pty").write_bytes(screen.raw)
            screen.close()
        screen = Pty(str(binary), root, home, cwd, "", True, argv=argv)
        try:
            screen.wait("basic")
            screen.send(b"\x1b[A")
            editor(screen, "seed-39 Ж界😀")
            screen.send(b"\x1b[B\x04")
            reopened = save(screen, output, "recall-restart")
        finally:
            (output / "recall-reopened.pty").write_bytes(screen.raw)
            screen.close()
        (output / "recall-journals.json").write_text(json.dumps(journals(root)) + "\n")
        return [row, reopened]


def busy(adapter, output):
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as name:
        root = Path(name)
        home, cwd = roots(root)
        screen = Pty(str(adapter), root, home, cwd, "terminal", True)
        try:
            screen.wait("basic")
            screen.send("first\r")
            screen.wait("предварительный ответ")
            screen.send("SECOND_DRAFT")
            editor(screen, "SECOND_DRAFT")
            at = len(screen.raw)
            screen.send(b"\x1b[A")
            editor(screen, "first", at)
            at = len(screen.raw)
            screen.send(b"\r")
            screen.wait("busy", at)
            editor(screen, "first", at)
            at = len(screen.raw)
            screen.send(b"\x1b[B")
            editor(screen, "SECOND_DRAFT", at)
            screen.child.send_signal(signal.SIGINT)
            screen.wait("отменено")
            screen.send(b"\x7f" * len("SECOND_DRAFT") + b"\x04")
            return save(screen, output, "busy-rejected-draft-preview", (1,))
        finally:
            (output / "busy-live.pty").write_bytes(screen.raw)
            screen.close()


def tools(adapter, output, failed=False, plain=True):
    with tempfile.TemporaryDirectory(prefix="ansa-m4-", dir="/tmp") as name:
        root = Path(name)
        home, cwd = roots(root)
        case = "tool-error" if failed else "tools-plain" if plain else "tools-color"
        screen = Pty(str(adapter), root, home, cwd, "progress-error" if failed else "progress", plain)
        try:
            screen.wait("basic")
            at = len(screen.raw)
            screen.send("question\r")
            screen.wait("get_test_value: выполняется", at)
            screen.wait("get_test_value: ошибка" if failed else "get_test_value: готово", at)
            screen.wait("TOOL_FAILURE" if failed else "TOOL_RESULT", at)
            screen.wait("done", at, timeout=6)
            screen.wait("tools 1/1", at)
            elapsed = re.findall(rb"basic  [^\r\n]*? ([0-9]+) " + "с  tools".encode(), screen.raw[at:])
            assert elapsed and int(elapsed[-1]) >= 2, elapsed
            if not failed:
                at = len(screen.raw)
                screen.send(b"\x14")
                for _ in range(12):
                    screen.drain(.01)
                assert screen.raw[at:].count(b"details") > 100
                assert b"\x1b]0;INJECTED" not in screen.raw and b"\x07" not in screen.raw
                screen.send(b"\x14")
            if plain:
                assert b"\x1b[38;" not in screen.raw and b"\x1b[0m" not in screen.raw
            screen.send(b"\x04")
            return save(screen, output, case)
        finally:
            (output / (case + "-live.pty")).write_bytes(screen.raw)
            screen.close()

def reading_failure(adapter, output):
    with tempfile.TemporaryDirectory(prefix='ansa-m4-',dir='/tmp') as name:
        root=Path(name);home,cwd=roots(root)
        screen=Pty(str(adapter),root,home,cwd,'fail-timeout',True)
        try:
            screen.wait('basic');screen.send('question\r');screen.wait('> question')
            screen.send('DRAFT Ж界😀');editor(screen,'DRAFT Ж界😀')
            screen.send(b'\x1b[5~');screen.wait('чтение')
            screen.vt.update(screen.raw)
            body=frozen_body(screen.vt)
            screen.wait('http_timeout');screen.wait('пауза')
            editor(screen,'DRAFT Ж界😀');screen.vt.update(screen.raw)
            assert screen.vt.line(-4).startswith('http_timeout'),screen.vt.line(-4)
            assert body==frozen_body(screen.vt),'failure moved the frozen conversation'
            at=len(screen.raw);screen.send(b'\x19')
            assert copied(screen,at)=='Вы:\nquestion\n\n'
            screen.wait('Диалог отправлен',at);editor(screen,'DRAFT Ж界😀')
            screen.vt.update(screen.raw)
            assert screen.vt.line(-4).startswith('http_timeout')
            assert screen.vt.line(-3).startswith('Диалог отправлен')
            screen.send(b'\x7f'*len('DRAFT Ж界😀')+b'\x04')
            return save(screen,output,'reading-failure-copy',(1,))
        finally:
            (output/'reading-failure-live.pty').write_bytes(screen.raw);screen.close()


def main():
    # Prove that the independent checker rejects in-place dialogue repaint.
    terminal=Terminal();raw=b'\x1b[2;1Horiginal';terminal.update(raw)
    body=frozen_body(terminal)
    terminal.update(raw+b'\x1b[2;1H\x1b[2Kmutated')
    assert body!=frozen_body(terminal),'frozen-body checker accepted repaint mutant'
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    binary, adapter = args.binary.resolve(strict=True), args.fixture_binary.resolve(strict=True)
    rows = recall(binary, args.output)
    rows += [busy(adapter, args.output), tools(adapter, args.output),
             tools(adapter, args.output, plain=False), tools(adapter, args.output, failed=True),
             reading_failure(adapter,args.output)]
    report = {"passed": len(rows), "cases": rows}
    (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
