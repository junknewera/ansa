"""Color workspace acceptance over real PTYs and effective per-cell rendition."""
import argparse
import base64
import codecs
import hashlib
import json
from pathlib import Path
import re
import signal
import tempfile
import time
import unicodedata

from cli_progress_pty import copied, roots
from parity.frontend_io import Pty, journals
from parity import session_cases


class StyledTerminal:
    """Independent VT oracle: retained cells carry the rendition of each write.

    Erasure also records the current rendition, so an old RGB sequence in the
    byte history cannot make a later reset or default-background repaint pass.
    """
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.x = self.y = self.offset = 0
        self.fg = self.bg = None
        self.bold = self.italic = self.underline = self.reverse = False
        self.saved = (0, 0)
        self.pending = ''
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')
        self.cells = [[' '] * cols for _ in range(rows)]
        self.attrs = [[self.style()] * cols for _ in range(rows)]

    def style(self):
        return self.fg, self.bg, self.bold, self.italic, self.underline, self.reverse

    def sgr(self, values):
        at = 0
        while at < len(values):
            value = values[at]
            if value == 0:
                self.fg = self.bg = None
                self.bold = self.italic = self.underline = self.reverse = False
            elif value in (38, 48) and values[at + 1:at + 2] == [2] and len(values) >= at + 5:
                color = tuple(values[at + 2:at + 5])
                assert all(0 <= channel <= 255 for channel in color), 'invalid truecolor emission'
                if value == 38:
                    self.fg = color
                else:
                    self.bg = color
                at += 4
            elif value in (38, 48) and values[at + 1:at + 2] == [5] and len(values) >= at + 3:
                color = ('index', values[at + 2])
                if value == 38:
                    self.fg = color
                else:
                    self.bg = color
                at += 2
            elif value == 39:
                self.fg = None
            elif value == 49:
                self.bg = None
            elif value in (1, 22):
                self.bold = value == 1
            elif value in (3, 23):
                self.italic = value == 3
            elif value in (4, 24):
                self.underline = value == 4
            elif value in (7, 27):
                self.reverse = value == 7
            elif 30 <= value <= 37 or 90 <= value <= 97:
                self.fg = ('ansi', value)
            elif 40 <= value <= 47 or 100 <= value <= 107:
                self.bg = ('ansi', value)
            at += 1

    def erase(self, row, begin, end):
        self.cells[row][begin:end] = [' '] * (end - begin)
        self.attrs[row][begin:end] = [self.style()] * (end - begin)

    def update(self, raw):
        assert len(raw) >= self.offset, 'terminal byte history moved backwards'
        self.pending += self.decoder.decode(raw[self.offset:])
        self.offset = len(raw)
        at = 0
        while at < len(self.pending):
            ch = self.pending[at]
            if ch == '\x1b':
                if at + 1 == len(self.pending):
                    break
                next_char = self.pending[at + 1]
                if next_char == '[':
                    match = re.match(r'\x1b\[([^\x40-\x7e]*)([\x40-\x7e])', self.pending[at:])
                    if not match:
                        break
                    parameters, command = match.groups()
                    values = [int(v) if v.isdigit() else 0 for v in parameters.split(';')]
                    n = values[0] or 1
                    if command == 'A': self.y = max(0, self.y - n)
                    elif command == 'B': self.y = min(self.rows - 1, self.y + n)
                    elif command == 'C': self.x = min(self.cols - 1, self.x + n)
                    elif command == 'D': self.x = max(0, self.x - n)
                    elif command in ('H', 'f'):
                        self.y = min(self.rows - 1, n - 1)
                        self.x = min(self.cols - 1, (values[1] or 1 if len(values) > 1 else 1) - 1)
                    elif command == 'J' and values[0] in (0, 2):
                        first = 0 if values[0] == 2 else self.y
                        for row in range(first, self.rows):
                            self.erase(row, 0 if values[0] == 2 or row != self.y else self.x, self.cols)
                    elif command == 'K':
                        begin, end = ((0, self.cols) if values[0] == 2 else
                                      (0, self.x + 1) if values[0] == 1 else (self.x, self.cols))
                        self.erase(self.y, begin, end)
                    elif command == 'm': self.sgr(values)
                    at += len(match[0])
                    continue
                if next_char in (']', '_'):
                    delimiters = ('\x07', '\x1b\\') if next_char == ']' else ('\x1b\\',)
                    ends = [(self.pending.find(ending, at + 2), len(ending)) for ending in delimiters]
                    ends = [(index, length) for index, length in ends if index >= 0]
                    if not ends:
                        break
                    end, length = min(ends)
                    at = end + length
                    continue
                if next_char == '7': self.saved = self.x, self.y
                elif next_char == '8': self.x, self.y = self.saved
                at += 2
                continue
            if ch == '\r': self.x = 0
            elif ch == '\n':
                self.y += 1
                if self.y >= self.rows:
                    self.cells.pop(0); self.cells.append([' '] * self.cols)
                    self.attrs.pop(0); self.attrs.append([self.style()] * self.cols)
                    self.y = self.rows - 1
            elif ord(ch) >= 32:
                width = 0 if unicodedata.combining(ch) else 2 if unicodedata.east_asian_width(ch) in ('W', 'F') else 1
                if width:
                    self.cells[self.y][self.x] = ch
                    self.attrs[self.y][self.x] = self.style()
                    if width == 2 and self.x + 1 < self.cols:
                        self.cells[self.y][self.x + 1] = ''
                        self.attrs[self.y][self.x + 1] = self.style()
                    self.x = min(self.cols - 1, self.x + width)
            at += 1
        self.pending = self.pending[at:]

    def line(self, row): return ''.join(self.cells[row]).rstrip()
    def text(self): return '\n'.join(self.line(row) for row in range(self.rows))
    def locations(self, text):
        found = []
        for row in range(self.rows):
            line = self.line(row)
            start = 0
            while (at := line.find(text, start)) >= 0:
                found.append((row, at)); start = at + len(text)
        return found
    def location(self, text, last=False):
        found = self.locations(text)
        assert found, ('missing visible text', text, self.text())
        return found[-1] if last else found[0]
    def attribute(self, text, last=False):
        row, column = self.location(text, last)
        return self.attrs[row][column]


def oracle_controls():
    vt = StyledTerminal(20, 4)
    raw = b'\x1b[38;2;10;20;30mABC\r\x1b[C\x1b[38;2;40;50;60mZ'
    for end in range(1, len(raw) + 1): vt.update(raw[:end])
    assert vt.line(0) == 'AZC'
    assert vt.attrs[0][0][0] == vt.attrs[0][2][0] == (10, 20, 30)
    assert vt.attrs[0][1][0] == (40, 50, 60)
    raw += b'\x1b[48;2;70;80;90m\x1b[K'
    vt.update(raw)
    assert vt.attrs[0][2][1] == (70, 80, 90) and vt.line(0) == 'AZ'
    raw += b'\x1b[0m\r\x1b[KX'
    vt.update(raw)
    assert vt.line(0) == 'X' and vt.attrs[0][0][:2] == (None, None)
    return ['partial-sgr-retains-untouched-cells', 'erase-records-current-background',
            'reset-clears-prior-styles']


def visible(screen):
    screen.vt.update(screen.raw)
    return screen.vt.text()


def until(screen, predicate, label, timeout=6):
    deadline = time.monotonic() + timeout
    while not predicate(visible(screen)):
        assert screen.child.poll() is None and time.monotonic() < deadline, (label, visible(screen))
        screen.drain(.01)


def escape(screen):
    screen.send(b'\x1b')
    for _ in range(8): screen.drain(.01)


def local(screen, command):
    assert command.startswith('/') and '\n' not in command and '\r' not in command
    # Bare commands first insert their completion, then submit it; arguments
    # supply the token delimiter. The workspace suite checks each Enter alone.
    screen.send(command.rstrip() + '\r')


def mouse(screen, button, column, row, release=False):
    screen.send(f'\x1b[<{button};{column + 1};{row + 1}{"m" if release else "M"}')


def click(screen, column, row):
    mouse(screen, 0, column, row); mouse(screen, 0, column, row, True)


def click_text(screen, text):
    row, column = screen.vt.location(text)
    click(screen, column, row)


def open_activity(screen):
    screen.send(b'\x14')
    until(screen, lambda text: 'Activity' in text, 'Agents panel opened')
    click_text(screen, 'Activity')
    until(screen, lambda text: '↑/↓ select · Enter open' in text, 'Activity tab opened')


def close_activity(screen):
    escape(screen)
    until(screen, lambda text: 'Activity' not in text, 'Terminals Activity closed')


def records(root):
    result = []
    for directory in (root / 'home/.ansa/sessions').iterdir():
        if directory.is_dir() and not directory.is_symlink():
            result.extend(session_cases.decode({path.name: path.read_bytes() for path in directory.glob('part-*.log')}))
    return result


def completed(root, text):
    rows = records(root)
    turns = {row['turn'] for row in rows if row['kind'] == 'accepted' and row['text'] == text}
    return any(row['kind'] == 'done' and row['turn'] in turns for row in rows)


def start(binary, root, home, cwd, screens, label, adapter=False, size=(112, 40), extra_env=None):
    extra = {'XDG_CACHE_HOME': str(root / 'cache')}
    extra.update(extra_env or {})
    argv = None if adapter else ['--offline', '--cwd', str(cwd)]
    screen = Pty(str(binary), root, home, cwd, 'workspace-edit' if adapter else '', False,
                 size=size, argv=argv, extra_env=extra)
    screen.vt = StyledTerminal(*size)
    screen.label = label
    screen.finished = None
    screen.snapshots = {}
    screens.append(screen)
    until(screen, lambda text: 'ANSA' in text and 'gpt-6.1-sol' in screen.vt.line(-2)
          and screen.vt.line(-4).startswith('  ›'), 'first color workspace')
    return screen


def snapshot(screen, label):
    visible(screen)
    screen.snapshots[label] = {'through': len(screen.raw), 'text': screen.vt.text(), 'attrs': screen.vt.attrs}
    # Freeze lists independently of the next partial repaint.
    screen.snapshots[label] = json.loads(json.dumps(screen.snapshots[label]))


def finish(screen, shutdown=signal.SIGTERM):
    snapshot(screen, 'final')
    result = screen.finish(shutdown)
    screen.finished = {key: value for key, value in result.items() if key != 'stdout_hex'}
    assert result['exit'] == (143 if shutdown is not None else 0) and result['termios_restored'] and result['flags_restored'], result
    screen.close()
    screen.closed = True


def body_background(screen):
    # Stable empty body cell, outside message and editor surfaces.
    return screen.vt.attrs[screen.vt.rows - 7][screen.vt.cols - 3][1]


def attribute_is(screen, text, index, value):
    locations = screen.vt.locations(text)
    return bool(locations) and screen.vt.attrs[locations[0][0]][locations[0][1]][index] == value


def native_background(screen, *labels):
    assert body_background(screen) is None, 'workspace painted over the native terminal background'
    for label in labels:
        locations = screen.vt.locations(label)
        assert locations, ('missing native surface label', label)
        assert all(screen.vt.attrs[row][column][1] is None for row, column in locations), ('non-native surface', label)


def colored_lifecycle(binary, adapter, root, home, cwd, screens):
    screen = start(binary, root, home, cwd, screens, 'lifecycle')
    answer = 'OMH_TOOL_OK:OMH_FIXED_VALUE_42'
    prompt = 'VISUAL_TASK\n\nPARAGRAPH'
    before_submit = journals(root)
    screen.send(b'VISUAL_TASKPARAGRAPH' + b'\x1b[D' * len('PARAGRAPH') + b'\x1b\r\x1b[13;2u')
    until(screen, lambda text: '› VISUAL_TASK\n\n    PARAGRAPH' in text, 'both modified Enter keys insert in the draft')
    assert journals(root) == before_submit
    screen.send(b'\r')
    until(screen, lambda text: completed(root, prompt) and answer in text
          and 'get_test_value · done' in text, 'short tool status and final text in chat')
    answer_fg = screen.vt.attribute(answer)[0]
    user_bg = screen.vt.attribute('VISUAL_TASK', last=True)[1]
    assert user_bg is not None and not screen.vt.attribute('VISUAL_TASK', last=True)[2]
    row, col = screen.vt.location('VISUAL_TASK', last=True)
    assert not screen.vt.line(row + 1) and 'PARAGRAPH' in screen.vt.line(row + 2)
    for message_row in range(row, row + 3):
        assert all(attr[1] == user_bg for attr in screen.vt.attrs[message_row][col - 2:screen.vt.cols - 1])
        assert screen.vt.attrs[message_row][-1][1] is None
    assert screen.vt.attrs[row + 3][col - 2][1] is None
    native_background(screen, answer)
    screen.send('VISUAL_DRAFT Ж')
    until(screen, lambda text: 'VISUAL_DRAFT Ж' in screen.vt.line(-4), 'pinned colored draft')
    input_bg = screen.vt.attribute('VISUAL_DRAFT Ж')[1]
    native_background(screen, 'VISUAL_DRAFT Ж')
    before = journals(root)
    open_activity(screen)
    until(screen, lambda text: '▸ get_test_value' in text and 'done' in text, 'committed tool in Activity')
    headers = [screen.vt.line(row) for row in range(3, screen.vt.rows - 5) if '▸ get_test_value' in screen.vt.line(row)]
    assert len(headers) == 1 and '▸' in headers[0] and 'done' in headers[0], headers
    target_fg = screen.vt.attribute('get_test_value')[0]
    status_fg = screen.vt.attribute('done')[0]
    assert target_fg is not None and status_fg is not None and target_fg != answer_fg
    snapshot(screen, 'compact')
    click_text(screen, '▸ get_test_value')
    until(screen, lambda text: 'Arguments' in text and 'Output' in text and '▾' in text, 'colored Activity expansion')
    assert screen.vt.attribute('VISUAL_DRAFT Ж')[1] == input_bg
    snapshot(screen, 'expanded')
    click_text(screen, '▾ get_test_value')
    until(screen, lambda text: 'Arguments' not in text and '▸' in text, 'colored inline collapse')
    close_activity(screen)
    assert 'get_test_value · done' in visible(screen)
    row, column = screen.vt.location(answer)
    mouse(screen, 0, column, row)
    mouse(screen, 32, column + len(answer) - 1, row)
    until(screen, lambda text: screen.vt.attribute(answer)[1] is not None, 'selection highlight painted')
    selected_bg = screen.vt.attribute(answer)[1]
    assert selected_bg is not None, 'local text selection has no visible background'
    assert screen.vt.attribute('VISUAL_TASK', last=True)[1] == user_bg
    assert screen.vt.attribute('VISUAL_DRAFT Ж')[1] == input_bg
    snapshot(screen, 'selected')
    at = len(screen.raw)
    screen.resize(72, 40)
    screen.wait(b'\x1b[H\x1b[2J', since=at)
    screen.vt = StyledTerminal(72, 40)
    until(screen, lambda text: answer in text and 'VISUAL_DRAFT Ж' in screen.vt.line(-4), 'colored frozen resize')
    assert screen.vt.attribute('VISUAL_DRAFT Ж')[1] == input_bg
    assert screen.vt.attribute(answer)[1] == selected_bg, 'resize lost local selection color'
    snapshot(screen, 'resized-selection')
    mouse(screen, 0, column + len(answer) - 1, row, True)
    assert copied(screen, at) == answer
    until(screen, lambda text: attribute_is(screen, answer, 1, None)
          and 'VISUAL_DRAFT Ж' in screen.vt.line(-4), 'release cleared selection and restored composer')
    native_background(screen, answer, 'VISUAL_DRAFT Ж')
    assert journals(root) == before, 'color interactions changed canonical journal'
    screen.send(b'\x15')
    local(screen, '/btw SIDE_VISUAL')
    until(screen, lambda text: 'Main conversation' in text and '/btw · read-only' in text
          and completed(root, 'SIDE_VISUAL'), 'side chat focus visible in two panes')
    main_fg = screen.vt.attribute('Main conversation')[0]
    side_fg = screen.vt.attribute('/btw · read-only')[0]
    assert main_fg is not None and side_fg is not None and main_fg != side_fg
    assert 'Esc → main chat' in visible(screen)
    snapshot(screen, 'focused-side-chat')
    escape(screen)
    until(screen, lambda text: '/btw · read-only' not in text and 'VISUAL_TASK' in text,
          'escape returned to the main conversation')
    finish(screen)
    return {'effective_main_fg': main_fg, 'effective_side_fg': side_fg,
            'effective_target_fg': target_fg, 'effective_status_fg': status_fg,
            'effective_user_bg': user_bg, 'effective_input_bg': input_bg,
            'effective_selection_bg': selected_bg, 'effective_body_bg': body_background(screen)}


def preferences(home):
    path = home / '.ansa/ui.json'
    try: return json.loads(path.read_text())['theme']
    except (FileNotFoundError, json.JSONDecodeError, KeyError): return None


def material(primary, surface='#18191A', input_color='#242526'):
    colors = {'primary': primary, 'on_surface': '#EFEAE4', 'surface': surface,
              'on_surface_variant': '#B6AEA4', 'outline_variant': '#4A4540',
              'error': '#F18E96', 'surface_container_high': input_color,
              'secondary_container': '#493847'}
    return {'mode': 'dark', 'colors': {'dark': colors, 'light': colors},
            'dank16': {'color2': {'dark': '#85DCB1', 'light': '#207844', 'default': '#85DCB1'},
                       'color3': {'dark': '#E8BD75', 'light': '#896000', 'default': '#E8BD75'}}}


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    pending = path.with_name(path.name + '.fixture-pending')
    pending.write_text(json.dumps(value))
    pending.replace(path)


def theme_preferences(binary, adapter, root, home, cwd, screens):
    screen = start(binary, root, home, cwd, screens, 'theme-first')
    baseline = screen.vt.attribute('ANSA')[0]
    before = journals(root)
    local(screen, '/theme')
    until(screen, lambda text: 'Theme preview' in text, 'theme picker')
    screen.send(b'\x1b[B')
    until(screen, lambda text: 'ANSA' in text and not attribute_is(screen, 'ANSA', 0, baseline), 'Pink live preview')
    snapshot(screen, 'pink-preview')
    escape(screen)
    until(screen, lambda text: 'Theme preview' not in text and attribute_is(screen, 'ANSA', 0, baseline), 'Esc rolled back theme')
    assert preferences(home) is None, 'preview wrote preferences before Enter'
    local(screen, '/theme')
    until(screen, lambda text: 'Theme preview' in text, 'theme picker for commit')
    screen.send(b'\x1b[B')
    until(screen, lambda text: 'ANSA' in text and not attribute_is(screen, 'ANSA', 0, baseline), 'Pink second preview')
    pink = screen.vt.attribute('ANSA')[0]
    screen.send(b'\r')
    until(screen, lambda text: preferences(home) == 'pink' and 'Theme preview' not in text, 'Enter persisted Pink')
    assert screen.vt.attribute('ANSA')[0] == pink
    choices = []
    last = pink
    for name in ['red', 'blue', 'amber']:
        local(screen, f'/theme {name}')
        until(screen, lambda text: 'ANSA' in text and 'Loading' not in text and not attribute_is(screen, 'ANSA', 0, last), f'rapid {name} accepted')
        last = screen.vt.attribute('ANSA')[0]
        native_background(screen, 'ANSA', '›')
        choices.append(name)
    until(screen, lambda text: preferences(home) == 'amber', 'latest accepted theme persisted')
    assert journals(root) == before, 'theme selection dispatched model/runtime changes'
    finish(screen)
    restarted = start(binary, root, home, cwd, screens, 'theme-restart')
    until(restarted, lambda text: attribute_is(restarted, 'ANSA', 0, last), 'theme restored after restart')
    custom = home / '.ansa/themes/rose.json'
    atomic_json(custom, {'name': 'Fixture Rose', 'colors': {'accent': '#CB6D99', 'background': '#111219', 'input': '#271A2A'}})
    local(restarted, '/theme custom:rose')
    until(restarted, lambda text: preferences(home) == 'custom:rose' and attribute_is(restarted, 'ANSA', 0, (203, 109, 153)), 'custom palette applied')
    restarted.send('THEME_DRAFT')
    until(restarted, lambda text: 'THEME_DRAFT' in restarted.vt.line(-4), 'custom accent with native composer')
    native_background(restarted, 'ANSA', 'THEME_DRAFT')
    restarted.send(b'\x15')
    palette = root / 'cache/dms/dms-colors.json'
    atomic_json(palette, material('#AACC99'))
    local(restarted, '/theme dms')
    until(restarted, lambda text: preferences(home) == 'dms' and attribute_is(restarted, 'ANSA', 0, (170, 204, 153)), 'Auto DMS applied')
    atomic_json(palette, material('#F088BA', '#201723', '#2A1C2D'))
    until(restarted, lambda text: attribute_is(restarted, 'ANSA', 0, (240, 136, 186)), 'Auto DMS followed atomic cache update', timeout=5)
    native_background(restarted, 'ANSA', '›')
    snapshot(restarted, 'dms-updated')
    prefs_before = (home / '.ansa/ui.json').read_bytes()
    local(restarted, '/theme')
    until(restarted, lambda text: 'Theme preview' in text, 'DMS theme picker')
    restarted.send(b'\x1b[A')
    until(restarted, lambda text: 'ANSA' in text and not attribute_is(restarted, 'ANSA', 0, (240, 136, 186)), 'DMS picker preview')
    escape(restarted)
    until(restarted, lambda text: 'Theme preview' not in text and attribute_is(restarted, 'ANSA', 0, (240, 136, 186)), 'DMS preview rollback')
    assert (home / '.ansa/ui.json').read_bytes() == prefs_before
    restarted.send('/')
    until(restarted, lambda text: attribute_is(restarted, '/model', 1, (73, 56, 71)), 'DMS colors the local completion selection')
    native_background(restarted, 'ANSA', '›')
    snapshot(restarted, 'dms-local-selection-native-body')
    escape(restarted)
    restarted.send(b'\x15')
    assert all(row['kind'] != 'accepted' for row in records(root)), 'UI-only themes submitted a turn'
    finish(restarted)
    return {'rapid_choices': choices, 'restored_accent': last, 'dms_updated_accent': [240, 136, 186]}


def inline_edit(binary, adapter, root, home, cwd, screens):
    screen = start(adapter, root, home, cwd, screens, 'edit', adapter=True, size=(96, 40))
    hold = root / 'hold-generation'
    hold.touch()
    try:
        screen.send('EDIT_VISUAL_TASK\r')
        spinners = set('⠋⠙⠹⠸⠼⠴⠦⠧')
        until(screen, lambda text: 'EDIT_VISUAL_TASK' in text, 'accepted user message under fixture hold', timeout=3)
        until(screen, lambda text: re.search(r'waiting for model · [1-9]\d* s', text),
              'main chat shows the elapsed turn time while the provider is silent', timeout=4)
        open_activity(screen)

        def activity():
            return [(row, glyph) for row in range(3, screen.vt.rows - 5)
                    if 'waiting for model' in screen.vt.line(row)
                    for glyph in screen.vt.line(row) if glyph in spinners]

        until(screen, lambda _: bool(activity()),
              'generating activity under fixture hold', timeout=3)
        assert not (root / 'generated').exists(), 'fixture did not hold the first generation'
        since = len(screen.raw)
        began = time.monotonic()
        observed = set(activity())
        # No user input, provider reply or file change drives these repaints.
        # The owner must schedule animation beyond its one-second clock.
        while time.monotonic() - began < .65:
            screen.drain(.01)
            visible(screen)
            current = activity()
            assert current and screen.child.poll() is None, ('activity vanished during idle animation', visible(screen))
            observed.update(current)
        idle_elapsed = time.monotonic() - began
        assert idle_elapsed < .95, ('animation observation overscheduled', idle_elapsed)
        idle_delta = screen.raw[since:]
        assert len({row for row, _ in observed}) == 1, ('activity moved screen rows', observed)
        assert len({glyph for _, glyph in observed}) >= 3, ('activity did not animate independently of the one-second clock', observed)
        assert len(idle_delta) < 8 * 1024 and b'\x1b[2J' not in idle_delta, ('animation repainted the canvas', len(idle_delta))
        assert not (root / 'generated').exists(), 'provider fixture completed during idle observation'
        snapshot(screen, 'idle-animation')
    finally:
        hold.unlink(missing_ok=True)
    until(screen, lambda text: completed(root, 'EDIT_VISUAL_TASK') and 'Edited' in text, 'acknowledged edit in Activity')
    assert (cwd / 'edit-target.rs').read_text() == 'fn main() {\n    new();\n}\n'
    assert '(+1 -1)' in visible(screen)
    old_row, _ = screen.vt.location('old();')
    new_row, _ = screen.vt.location('new();')
    old_gutter = re.search(r'(2)\s+(-)\s+old\(\);', screen.vt.line(old_row))
    new_gutter = re.search(r'(2)\s+(\+)\s+new\(\);', screen.vt.line(new_row))
    assert old_gutter and new_gutter, (screen.vt.line(old_row), screen.vt.line(new_row))
    assert old_gutter.start(1) < new_gutter.start(1), 'old/new gutters share a column'
    assert old_gutter.start(2) == new_gutter.start(2), 'diff markers misaligned'
    removed = screen.vt.attribute('old();')[1]
    added = screen.vt.attribute('new();')[1]
    context = screen.vt.attribute('fn main()')[1]
    assert removed is not None and added is not None and removed != added
    assert context is None and removed != context and added != context, 'diff colors must stay local against native context rows'
    snapshot(screen, 'inline-diff')
    close_activity(screen)
    until(screen, lambda text: 'EDIT_DONE' in text, 'edit response in main chat')
    assert screen.vt.attribute('EDIT_VISUAL_TASK', last=True)[1] is not None
    assert all(label not in visible(screen) for label in ('Edited', 'old();', 'new();'))
    screen.send('EDIT_DRAFT Ж')
    until(screen, lambda text: 'EDIT_DRAFT Ж' in screen.vt.line(-4), 'edit draft')
    native_background(screen, 'EDIT_DRAFT Ж')
    before = journals(root)
    open_activity(screen)
    until(screen, lambda text: 'Edited' in text, 'edit visible with pinned draft')
    row, column = screen.vt.location('Edited')
    mouse(screen, 0, column, row)
    escape(screen)
    mouse(screen, 0, column, row, True)
    for _ in range(8): screen.drain(.01)
    assert 'Edit diff' not in visible(screen), 'Esc did not cancel the pending click'
    if 'Activity' not in visible(screen):
        open_activity(screen)
    until(screen, lambda text: 'Edited' in text, 'review Activity restored')
    click_text(screen, 'Edited')
    until(screen, lambda text: 'Edit diff' in text and 'j/k lines' in text, 'complete per-edit Diff inspector')
    assert screen.vt.attribute('old();')[1] == removed and screen.vt.attribute('new();')[1] == added
    assert screen.vt.attribute('fn main()')[1] is None
    assert 'EDIT_DRAFT Ж' in screen.vt.line(-4)
    snapshot(screen, 'full-diff')
    escape(screen)
    until(screen, lambda text: 'Edit diff' not in text and 'Edited' in text, 'Esc returned from edit diff to Activity')
    assert 'EDIT_DRAFT Ж' in screen.vt.line(-4) and journals(root) == before
    rows = records(root)
    assert sum(row['kind'] == 'accepted' for row in rows) == 1
    assert sum(row['kind'] == 'intent' for row in rows) == 1
    outcomes = [row['result'] for row in rows if row['kind'] == 'outcome']
    assert len(outcomes) == 1
    assert int(outcomes[0]['execution'].lexeme) == 0 and int(outcomes[0]['effects'].lexeme) == 1
    value = json.loads(outcomes[0]['output'])
    assert value['applied'] is True
    assert value['before'] == 'fn main() {\n    old();\n}\n' and value['after'] == (cwd / 'edit-target.rs').read_text()
    assert value['before_hash'] == hashlib.sha256(value['before'].encode()).hexdigest()
    assert value['after_hash'] == hashlib.sha256(value['after'].encode()).hexdigest()
    assert (root / 'generated').read_bytes() == b'xx', 'unexpected repeated model dispatch'
    finish(screen)
    with tempfile.TemporaryDirectory(prefix='ansa-m4-', dir='/tmp') as directory:
        quiet_root = Path(directory)
        quiet_home, quiet_cwd = roots(quiet_root)
        quiet_hold = quiet_root / 'hold-generation'
        quiet_hold.touch()
        quiet = start(adapter, quiet_root, quiet_home, quiet_cwd, screens, 'edit-reduced-motion',
                      adapter=True, size=(96, 40), extra_env={'ANSA_NO_ANIMATION': '1'})
        eof_interrupted = False
        try:
            quiet.send('EDIT_REDUCED_MOTION_TASK\r')
            until(quiet, lambda text: 'EDIT_REDUCED_MOTION_TASK' in text,
                  'reduced-motion message under fixture hold', timeout=3)
            until(quiet, lambda text: '◷ waiting for model' in text,
                  'main chat shows static progress with reduced motion', timeout=3)
            open_activity(quiet)

            def quiet_activity():
                return [row for row in range(3, quiet.vt.rows - 5)
                        if '◷ waiting for model' in quiet.vt.line(row)]

            until(quiet, lambda _: bool(quiet_activity()),
                  'reduced-motion activity under fixture hold', timeout=3)
            quiet_row = quiet_activity()
            assert len(quiet_row) == 1 and not (quiet_root / 'generated').exists()
            quiet_style = quiet.vt.attrs[quiet_row[0]].copy()
            quiet_since = len(quiet.raw)
            quiet_began = time.monotonic()
            while time.monotonic() - quiet_began < .65:
                quiet.drain(.01)
                text = visible(quiet)
                assert quiet_activity() == quiet_row, ('reduced-motion activity moved', text)
                assert not any(glyph in text for glyph in spinners), ('reduced motion emitted an animated spinner', text)
                assert quiet.vt.attrs[quiet_row[0]] == quiet_style, 'reduced motion retained a foreground shimmer'
                assert quiet.child.poll() is None and not (quiet_root / 'generated').exists()
            quiet_elapsed = time.monotonic() - quiet_began
            quiet_delta_bytes = len(quiet.raw) - quiet_since
            snapshot(quiet, 'reduced-motion-idle')
            close_activity(quiet)
            quiet.send(b'\x04')
            quiet.drain(.04)
            quiet.child.send_signal(signal.SIGINT)
            eof_interrupted = True
        finally:
            try:
                finish(quiet, None if eof_interrupted else signal.SIGTERM)
            finally:
                quiet_hold.unlink(missing_ok=True)
        assert not (quiet_root / 'generated').exists(), 'cancelled held generation replied'
        assert (quiet_cwd / 'edit-target.rs').read_text() == 'fn main() {\n    old();\n}\n'
        quiet_records = records(quiet_root)
        assert sum(row['kind'] == 'accepted' and row['text'] == 'EDIT_REDUCED_MOTION_TASK'
                   for row in quiet_records) == 1
        assert not any(row['kind'] in ('intent', 'outcome') for row in quiet_records), 'reduced-motion probe executed an edit'
        quiet_evidence = {'row': quiet_row[0], 'glyph': '◷', 'elapsed': quiet_elapsed,
                          'delta_bytes': quiet_delta_bytes,
                          'termios_restored': quiet.finished['termios_restored'],
                          'flags_restored': quiet.finished['flags_restored'],
                          'generated': False, 'tool_intents': 0}
    with tempfile.TemporaryDirectory(prefix='ansa-m4-', dir='/tmp') as directory:
        failed_root = Path(directory)
        failed_home, failed_cwd = roots(failed_root)
        rejection = failed_root / 'fail-generation'
        rejection.touch()
        paused = start(adapter, failed_root, failed_home, failed_cwd, screens, 'paused-draft', adapter=True)
        paused.send('FAILED_TURN\r')
        until(paused, lambda text: 'terminal_item_count' in text and 'paused' in paused.vt.line(-2),
              'provider failure paused the request')
        assert 'Continue' in paused.vt.line(-1) and 'Ctrl+C' in paused.vt.line(-1)
        paused.send('NEXT_TURN Ж\r')
        until(paused, lambda text: paused.vt.line(-4) == '  › NEXT_TURN Ж', 'paused draft preserved')
        assert sum(row['kind'] == 'accepted' for row in records(failed_root)) == 1
        snapshot(paused, 'paused')
        paused.child.send_signal(signal.SIGINT)
        until(paused, lambda text: paused.vt.line(-4) == '  ›' and 'paused' in paused.vt.line(-2),
              'Ctrl+C cleared the draft and retained the paused request')
        local(paused, '/resume')
        until(paused, lambda text: 'Project sessions' in text and 'Resume current request' in text,
              'resume opened the saved sessions picker')
        paused.send(b'\r')
        until(paused, lambda text: (failed_root / 'generated').stat().st_size == 2
              and sum(row['kind'] == 'pause' for row in records(failed_root)) >= 2
              and 'paused' in paused.vt.line(-2), 'picker resumed the same request')
        rejection.unlink()
        paused.child.send_signal(signal.SIGINT)
        until(paused, lambda text: 'cancelled' in paused.vt.line(-2), 'Ctrl+C cancelled the paused request')
        paused.send('NEXT_TURN Ж\r')
        until(paused, lambda text: completed(failed_root, 'NEXT_TURN Ж') and 'EDIT_DONE' in text,
              'new message sent after cancellation')
        assert sum(row['kind'] == 'accepted' and row['text'] == 'NEXT_TURN Ж'
                   for row in records(failed_root)) == 1
        finish(paused)
    return {'added': 1, 'removed': 1, 'effective_add_bg': added, 'effective_remove_bg': removed,
            'paused_draft_cleared_and_picker_resumed': True,
            'idle_animation': {'row': next(iter(observed))[0], 'spinner_frames': len(observed),
                               'elapsed': idle_elapsed, 'delta_bytes': len(idle_delta)},
            'reduced_motion': quiet_evidence}


def image_bytes(screen, since):
    packets = re.findall(rb'\x1b_G([^;]+);([A-Za-z0-9+/=]*)\x1b\\', screen.raw[since:])
    for index, (header, payload) in enumerate(packets):
        if header.startswith(b'a=T,f=100,t=d,'):
            params = dict(field.split(b'=', 1) for field in header.split(b','))
            chunks = [payload]
            while header.endswith(b'm=1'):
                index += 1
                if index >= len(packets):
                    return None
                header, payload = packets[index]
                assert header in (b'm=0', b'm=1')
                chunks.append(payload)
            return params[b'i'], base64.b64decode(b''.join(chunks), validate=True)
    return None


def native_chart(binary, adapter, root, home, cwd, screens):
    from PIL import Image, ImageDraw

    (root / 'preview-chart').touch()
    path = cwd / 'chart.png'
    chart = Image.new('RGB', (160, 80), (63, 164, 109))
    draw = ImageDraw.Draw(chart)
    draw.line([(9, 68), (150, 68)], fill='white')
    draw.line([(14, 60), (46, 46), (79, 52), (112, 28), (144, 20)], fill='white', width=2)
    chart.save(path)
    screen = start(adapter, root, home, cwd, screens, 'chart', adapter=True,
                   extra_env={'TERM': 'xterm-kitty'})
    pending = len(screen.raw)
    screen.send('Покажи график\r')
    until(screen, lambda text: 'png_requested' in text and completed(root, 'Покажи график'),
          'assistant confirmed the PNG tool outcome')
    assert 'Open chart' not in visible(screen) and image_bytes(screen, pending) is None
    open_activity(screen)
    until(screen, lambda text: 'Open chart · chart.png' in text, 'validated PNG link in Activity')
    click_text(screen, 'Open chart · chart.png')
    until(screen, lambda text: 'Chart · chart.png' in text and 'PNG · 160 × 80' in text,
          'opened validated PNG from Activity')
    screen.wait(b'\x1b_Ga=T,f=100,t=d,', pending)
    assert screen.vt.location('Chart · chart.png')[1] > screen.vt.cols // 3
    until(screen, lambda _: image_bytes(screen, pending) is not None, 'complete image transfer')
    image_id, data = image_bytes(screen, pending)
    assert data == path.read_bytes()
    snapshot(screen, 'visible-chart')
    started = len(screen.raw)
    until(screen, lambda text: 'Enter copy path' in text, 'graph focused')
    for _ in range(2):
        copied_at = len(screen.raw)
        screen.send(b'\r')
        assert copied(screen, copied_at) == str(path)
    assert b'\x1b_Ga=d' not in screen.raw[started:], 'copying the path removed the picture'
    started = len(screen.raw)
    escape(screen)
    delete = b'\x1b_Ga=d,d=I,i=' + image_id + b',q=2;'
    screen.wait(delete, started)
    until(screen, lambda text: 'PNG · 160 × 80' not in text and 'Open chart · chart.png' in text,
          'Escape returned to Activity')
    snapshot(screen, 'closed-chart')
    started = len(screen.raw)
    click_text(screen, 'Open chart · chart.png')
    until(screen, lambda text: 'Chart · chart.png' in text, 'chart reopened from Activity')
    until(screen, lambda _: image_bytes(screen, started) is not None, 'complete reopened chart')
    snapshot(screen, 'reopened-chart')
    escape(screen)
    until(screen, lambda text: 'Open chart · chart.png' in text, 'Activity after chart reopen')
    close_activity(screen)
    screen.send('chart draft')
    until(screen, lambda _: 'chart draft' in screen.vt.line(-4), 'draft before keyboard navigation')
    screen.send(b'\x07')
    until(screen, lambda text: 'Activity' in text and 'Open chart · chart.png' in text,
          'Ctrl+G opened Activity with the chart link')
    for _ in range(12):
        screen.drain(.03)
        visible(screen)
        if screen.vt.attribute('Open chart · chart.png')[1] is not None:
            break
        screen.send(b'\x1b[B')
    else:
        raise AssertionError('could not focus the chart result')
    screen.send(b'\r')
    until(screen, lambda text: 'PNG · 160 × 80' in text, 'Enter opened selected PNG')
    escape(screen)
    until(screen, lambda text: 'PNG · 160 × 80' not in text and 'chart draft' in screen.vt.line(-4)
          and screen.vt.attribute('Open chart · chart.png')[1] is not None,
          'Esc restored result focus and draft')
    close_activity(screen)
    screen.send(b'\x03')
    other = cwd / 'other.png'
    Image.new('RGB', (240, 120), (40, 90, 190)).save(other)
    (root / 'preview-other').touch()
    screen.send('Покажи второй график\r')
    until(screen, lambda text: completed(root, 'Покажи второй график') and 'png_requested' in text,
          'second chart tool completed')
    assert 'Open chart' not in visible(screen)
    open_activity(screen)
    until(screen, lambda text: 'Open chart · other.png' in text, 'second chart link in Activity')
    click_text(screen, 'Open chart · other.png')
    until(screen, lambda text: 'Chart · other.png' in text
          and completed(root, 'Покажи второй график'), 'second chart opened')
    escape(screen)
    until(screen, lambda text: 'Open chart · chart.png' in text, 'older chart link still in Activity')
    click_text(screen, 'Open chart · chart.png')
    until(screen, lambda text: 'Chart · chart.png' in text, 'first chart still opens')
    escape(screen)
    until(screen, lambda text: 'Open chart · chart.png' in text, 'returned to the chart link')
    close_activity(screen)
    import struct
    import zlib
    good = other.read_bytes()
    start_idat = good.index(b'IDAT') - 4
    end_idat = start_idat + 12 + int.from_bytes(good[start_idat:start_idat + 4], 'big')
    bad = b'IDATnot a zlib stream'
    other.write_bytes(good[:start_idat] + struct.pack('>I', len(bad) - 4) + bad
                      + struct.pack('>I', zlib.crc32(bad)) + good[end_idat:])
    screen.send('Проверь повреждённый PNG\r')
    # The durable Done can precede the frame with the assistant's verdict.
    # Wait for that frame before taking coordinates from the transcript.
    until(screen, lambda text: 'PNG_REJECTED' in text and completed(root, 'Проверь повреждённый PNG'),
          'assistant reported the invalid PNG rejection')
    assert 'preview_png_decode' not in visible(screen)
    open_activity(screen)
    until(screen, lambda text: 'preview_png_decode' in text and 'Open chart · chart.png' in text,
          'invalid PNG diagnostic and earlier validated chart in Activity')
    assert 'Chart · other.png' not in visible(screen)
    reopening = len(screen.raw)
    click_text(screen, 'Open chart · chart.png')
    until(screen, lambda text: 'Chart · chart.png' in text and image_bytes(screen, reopening) is not None,
          'active chart before shutdown')
    started = len(screen.raw)
    finish(screen)
    assert delete in screen.raw[started:]
    assert screen.raw.rfind(delete) > screen.raw.rfind(b'\x1b_Ga=T,')
    return {'opened_from_activity': True, 'placement_id': int(image_id), 'bytes': path.stat().st_size,
            'copies_keep_picture': 2, 'first_chart_reopened': True, 'invalid_png_rejected': True}


def dataset_chart(binary, adapter, root, home, cwd, screens):
    (root / 'preview-dataset').touch()
    (cwd / 'sales.csv').write_text('date,amount\n2026-01-03,400\n2026-02-04,120\n2026-03-03,550\n')
    screen = start(adapter, root, home, cwd, screens, 'dataset', adapter=True,
                   extra_env={'TERM': 'xterm-kitty'})
    pending = len(screen.raw)
    screen.send('Построй график продаж по месяцам\r')
    until(screen, lambda _: completed(root, 'Построй график продаж по месяцам'),
          'CSV analysis finished', timeout=30)
    assert 'Open chart' not in visible(screen) and 'Source · sales.csv' not in visible(screen)
    open_activity(screen)
    until(screen, lambda text: 'Open chart · chart.png' in text and 'Source · sales.csv' in text,
          'analysis chart and source links in Activity')
    click_text(screen, 'Open chart · chart.png')
    until(screen, lambda text: 'Chart · chart.png' in text and image_bytes(screen, pending) is not None,
          'opened the CSV analysis PNG from Activity')
    path, = (home / '.ansa/sessions').glob('*/artifacts/*/chart.png')
    receipt = json.loads(path.with_name('receipt.json').read_text())
    descriptor, = [item for item in receipt['result']['artifacts'] if item['mime'] == 'image/png']
    outcome, = [row['result'] for row in records(root) if row['kind'] == 'outcome']
    assert descriptor in json.loads(outcome['output'])['artifacts']
    digest = hashlib.sha256(image_bytes(screen, pending)[1]).hexdigest()
    assert digest == descriptor['sha256'] == hashlib.sha256(path.read_bytes()).hexdigest()
    snapshot(screen, 'dataset-chart')
    escape(screen)
    until(screen, lambda text: 'Source · sales.csv' in text, 'chart closed to the Activity source link')
    click_text(screen, 'Source · sales.csv')
    until(screen, lambda text: 'sales.csv · read-only' in text and '2026-03-03,550' in text,
          'open the source CSV bound to the completed calculation')
    escape(screen)
    finish(screen)
    return {'canonical_png_sha256': digest, 'bytes': path.stat().st_size}


def retry_after_edit(binary, adapter, root, home, cwd, screens):
    marker = root / 'fail-edit-reply'
    marker.touch()
    screen = start(adapter, root, home, cwd, screens, 'retry', adapter=True)
    screen.send('EDIT_AND_REPLY\r')
    until(screen, lambda _: 'http_transfer' in screen.vt.line(-1),
          'interrupted reply explains the transport failure')
    until(screen, lambda text: 'Work paused' in text and '[Continue]' in text,
          'the main conversation offers a Continue action below the cause')
    assert (cwd / 'edit-target.rs').read_text() == 'fn main() {\n    new();\n}\n'
    marker.unlink()
    click_text(screen, '[Continue]')
    until(screen, lambda text: completed(root, 'EDIT_AND_REPLY') and 'EDIT_DONE' in text,
          'retry delivers the reply after the saved edit')
    rows = records(root)
    assert sum(row['kind'] == 'accepted' for row in rows) == 1
    assert sum(row['kind'] == 'intent' for row in rows) == 1
    assert sum(row['kind'] == 'outcome' for row in rows) == 1
    finish(screen)
    return {'accepted_turns': 1, 'edit_outcomes': 1}


def main():
    inventory = [('color-tool-selection-resize', colored_lifecycle),
                 ('theme-preview-persistence-dms', theme_preferences),
                 ('acknowledged-inline-edit-diff', inline_edit),
                 ('retry-after-saved-edit', retry_after_edit),
                 ('native-carbon-chart', native_chart),
                 ('native-carbon-dataset', dataset_chart)]
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--fixture-binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--case', action='append', choices=[name for name, _ in inventory], dest='selected_cases')
    args = parser.parse_args()
    binary, adapter = args.binary.resolve(strict=True), args.fixture_binary.resolve(strict=True)
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    adapter_hash = hashlib.sha256(adapter.read_bytes()).hexdigest()
    args.output.mkdir(parents=True, exist_ok=False)
    oracle_controls()
    cases = []
    for name, case in inventory:
        if args.selected_cases and name not in args.selected_cases:
            continue
        began = time.monotonic()
        screens = []
        uses_fixture = case in (inline_edit, retry_after_edit, native_chart, dataset_chart)
        record = {'case': name, 'passed': False,
                  'executed_binary': str(adapter if uses_fixture else binary),
                  'executed_binary_sha256': adapter_hash if uses_fixture else binary_hash,
                  'target_kind': 'production-library-fixture' if uses_fixture else 'production-binary'}
        try:
            with tempfile.TemporaryDirectory(prefix='ansa-m4-', dir='/tmp') as directory:
                root = Path(directory)
                home, cwd = roots(root)
                try:
                    record['observations'] = case(binary, adapter, root, home, cwd, screens)
                    record['passed'] = True
                finally:
                    for screen in screens:
                        prefix = args.output / (name + '-' + screen.label)
                        prefix.with_suffix('.pty').write_bytes(screen.raw)
                        prefix.with_suffix('.screen.json').write_text(json.dumps(screen.snapshots, ensure_ascii=False) + '\n')
                        prefix.with_suffix('.terminal.json').write_text(json.dumps(screen.finished) + '\n')
                        if not getattr(screen, 'closed', False): screen.close()
                    (args.output / (name + '.journals.json')).write_text(json.dumps(journals(root)) + '\n')
        except BaseException as error:
            record['error'] = repr(error)
            raise
        finally:
            record['duration_seconds'] = time.monotonic() - began
            cases.append(record)
            (args.output / 'report.json').write_text(json.dumps({
                'passed': sum(item['passed'] for item in cases), 'cases': cases,
                'binary': str(binary), 'binary_sha256': binary_hash,
                'fixture_binary': str(adapter), 'fixture_sha256': adapter_hash,
            }, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'passed': len(cases), 'cases': [row['case'] for row in cases]}))


if __name__ == '__main__':
    main()
