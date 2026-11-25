"""Carbon paste acceptance using a synthetic Wayland clipboard and provider.

Use a Cargo-built frontend-test-adapter, which finds wl-paste on PATH.
"""
import base64
import os
from pathlib import Path
import shlex
import shutil
import struct
import sys
import tempfile
import zlib

from carbon_visual_pty import StyledTerminal, completed, finish, records, roots, until, visible
from parity.frontend_io import Pty


def png_chunk(name, body):
    return struct.pack('>I', len(body)) + name + body + struct.pack('>I', zlib.crc32(name + body))


def main(adapter):
    with tempfile.TemporaryDirectory(prefix='ansa-m4-', dir='/tmp') as directory:
        root = Path(directory)
        home, cwd = roots(root)
        cwd.chmod(0o700)
        home.chmod(0o700)
        (root / "outbound-messages.jsonl").touch()
        (root / "fixture-image-model").touch()
        image = root / 'clipboard.png'
        image.write_bytes(b'\x89PNG\r\n\x1a\n'
                          + png_chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 2, 0, 0, 0))
                          + png_chunk(b'IDAT', zlib.compress(b'\0\xd4\x21\x47'))
                          + png_chunk(b'IEND', b''))
        tool = root / 'wl-paste'
        text = root / 'clipboard-text'
        tool.write_text(f'#!{shutil.which("sh")}\n'
                        f'if [ -f {shlex.quote(str(text))} ]; then\n'
                        '  [ "$3" = text ] || exit 1\n'
                        f'  {shutil.which("sleep")} 0.25\n'
                        f'  exec {shutil.which("cat")} {shlex.quote(str(text))}\n'
                        'fi\n'
                        '[ "$3" = image/png ] || exit 1\n'
                        f'exec {shutil.which("cat")} {shlex.quote(str(image))}\n')
        tool.chmod(0o700)
        env = {'PATH': f'{root}:{os.environ.get("PATH", os.defpath)}', 'WAYLAND_DISPLAY': 'test-clipboard'}
        screen = None
        label = '[Image #1]'
        try:
            size = (112, 40)
            env.update(HOME=str(home), XDG_STATE_HOME=str(root / 'xdg'), TERM='xterm', LC_ALL='C')
            screen = Pty(str(adapter), root, home, cwd, 'openrouter-carbon', size=size, base_env=env)
            screen.vt = StyledTerminal(*size)
            screen.snapshots = {}
            until(screen, lambda text: 'ANSA' in text and cwd.name in screen.vt.line(-2)
                  and 'Opening session' not in text, 'open the vision fixture')
            text.write_text('BUFFER')
            screen.send(b'A\x16B')
            until(screen, lambda frame: 'ABUFFERB' in frame, 'slow clipboard preserves typing order')
            screen.send(b'\x03\x16\x03')
            screen.drain(0.5)
            visible(screen)
            assert 'BUFFER' not in screen.vt.line(-4), 'cancelled paste reappeared in the draft'
            text.unlink()
            screen.send(b'A\x16B')
            until(screen, lambda frame: f'A{label}B' in frame,
                  'Ctrl+V stages a PNG at the cursor while typing')
            assert label not in screen.vt.line(-1)
            assert not any(row['kind'] == 'accepted' for row in records(root))
            screen.send(b'\x1b[D\x7f')
            until(screen, lambda frame: label not in frame and 'AB' in screen.vt.line(-4),
                  'Backspace removes only the attachment')
            assert not any(row['kind'] == 'accepted' for row in records(root))
            screen.send(b'\x03')
            for number in range(1, 7):
                screen.send(b'\x16')
                until(screen, lambda frame: f'[Image #{number}]' in frame, f'PNG {number} stages')
            screen.send(b'\x01\x1b[3~')
            until(screen, lambda frame: '[Image #5]' in frame and '[Image #6]' not in frame,
                  'Delete removes only the first attachment')
            screen.send(b'\x05')
            payload = 'я' * 1001
            screen.send(f'start \x1b[200~{payload}\x1b[201~ end')
            until(screen, lambda text: 'start [pasted text · 1001 chars] end' in text,
                  'long paste collapses in place')
            assert not any(row['kind'] == 'accepted' for row in records(root))
            screen.send(b'\r')
            until(screen, lambda text: completed(root, f'start {payload} end') and label not in text,
                  'Enter submits original text and consumes the attachment')
            captured = (root / 'outbound-messages.jsonl').read_bytes()
            assert b'[Image #' not in captured
            assert captured.count(b'data:image/png;base64,' + base64.b64encode(image.read_bytes())) == 5
            finish(screen)
        finally:
            if screen is not None and not getattr(screen, 'closed', False):
                screen.close()
    print('Carbon clipboard: PNG staged, removed and sent; long paste submitted unchanged')


if __name__ == '__main__':
    main(Path(sys.argv[1]).resolve(strict=True))
