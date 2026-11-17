"""Color workspace acceptance over real PTYs and effective per-cell rendition."""
import argparse
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
                if next_char == ']':
                    ends = [(self.pending.find(ending, at + 2), len(ending)) for ending in ('\x07', '\x1b\\')]
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
    command = command.rstrip()
    screen.send(command + ('\r' if any(char.isspace() for char in command) else '\r\r'))


def mouse(screen, button, column, row, release=False):
    screen.send(f'\x1b[<{button};{column + 1};{row + 1}{"m" if release else "M"}')


def click(screen, column, row):
    mouse(screen, 0, column, row); mouse(screen, 0, column, row, True)


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
    until(screen, lambda text: 'ANSA' in text and 'готов' in screen.vt.line(-2), 'first color workspace')
    return screen


def snapshot(screen, label):
    visible(screen)
    screen.snapshots[label] = {'through': len(screen.raw), 'text': screen.vt.text(), 'attrs': screen.vt.attrs}
    # Freeze lists independently of the next partial repaint.
    screen.snapshots[label] = json.loads(json.dumps(screen.snapshots[label]))


def finish(screen):
    snapshot(screen, 'final')
    result = screen.finish(signal.SIGTERM)
    screen.finished = {key: value for key, value in result.items() if key != 'stdout_hex'}
    assert result['exit'] == 143 and result['termios_restored'] and result['flags_restored'], result
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
    screen.send('VISUAL_TASK\r')
    until(screen, lambda text: completed(root, 'VISUAL_TASK') and 'get_test_value' in text and 'OMH_TOOL_OK:OMH_FIXED_VALUE_42' in text and 'готов' in screen.vt.line(-2), 'committed colored tool and final text')
    headers = [screen.vt.line(row) for row in range(3, screen.vt.rows - 5) if 'get_test_value' in screen.vt.line(row)]
    assert len(headers) == 1 and '▸' in headers[0] and 'готово' in headers[0], headers
    target_fg = screen.vt.attribute('get_test_value')[0]
    answer_fg = screen.vt.attribute('OMH_TOOL_OK:OMH_FIXED_VALUE_42')[0]
    status_fg = screen.vt.attribute('готово')[0]
    assert target_fg is not None and status_fg is not None and target_fg != answer_fg
    user_bg = screen.vt.attribute('VISUAL_TASK', last=True)[1]
    assert screen.vt.attribute('VISUAL_TASK', last=True)[2], 'user text lost its visual emphasis'
    native_background(screen, 'VISUAL_TASK', 'get_test_value', 'OMH_TOOL_OK:OMH_FIXED_VALUE_42')
    screen.send('VISUAL_DRAFT Ж')
    until(screen, lambda text: 'VISUAL_DRAFT Ж' in screen.vt.line(-4), 'pinned colored draft')
    input_bg = screen.vt.attribute('VISUAL_DRAFT Ж')[1]
    native_background(screen, 'VISUAL_TASK', 'VISUAL_DRAFT Ж')
    snapshot(screen, 'compact')
    row, column = screen.vt.location('get_test_value')
    click(screen, column, row)
    until(screen, lambda text: 'Аргументы' in text and 'Вывод' in text and '▾' in text, 'colored inline expand')
    assert screen.vt.attribute('get_test_value')[0] == target_fg
    assert screen.vt.attribute('VISUAL_TASK')[1] == user_bg
    assert screen.vt.attribute('VISUAL_DRAFT Ж')[1] == input_bg
    snapshot(screen, 'expanded')
    row, column = screen.vt.location('get_test_value')
    click(screen, column, row)
    until(screen, lambda text: 'Аргументы' not in text and '▸' in text, 'colored inline collapse')
    before = journals(root)
    row, column = screen.vt.location('get_test_value')
    mouse(screen, 0, column, row)
    mouse(screen, 32, column + len('get_test_value') - 1, row)
    until(screen, lambda text: attribute_is(screen, 'готово', 0, status_fg), 'selection retained unselected status color')
    until(screen, lambda text: screen.vt.attribute('get_test_value')[1] is not None, 'selection highlight painted')
    selected_bg = screen.vt.attribute('get_test_value')[1]
    assert selected_bg is not None, 'local text selection has no visible background'
    assert screen.vt.attribute('VISUAL_TASK')[1] == user_bg
    assert screen.vt.attribute('VISUAL_DRAFT Ж')[1] == input_bg
    snapshot(screen, 'selected')
    at = len(screen.raw)
    screen.resize(72, 40)
    screen.wait(b'\x1b[H\x1b[2J', since=at)
    screen.vt = StyledTerminal(72, 40)
    until(screen, lambda text: 'get_test_value' in text and 'VISUAL_DRAFT Ж' in screen.vt.line(-4), 'colored frozen resize')
    assert screen.vt.attribute('готово')[0] == status_fg
    assert screen.vt.attribute('VISUAL_DRAFT Ж')[1] == input_bg
    assert screen.vt.attribute('get_test_value')[1] == selected_bg, 'resize lost local selection color'
    snapshot(screen, 'resized-selection')
    mouse(screen, 0, column + len('get_test_value') - 1, row, True)
    assert copied(screen, at) == 'get_test_value'
    until(screen, lambda text: attribute_is(screen, 'get_test_value', 1, None)
          and 'VISUAL_DRAFT Ж' in screen.vt.line(-4), 'release cleared selection and restored composer')
    native_background(screen, 'get_test_value', 'VISUAL_TASK', 'VISUAL_DRAFT Ж')
    assert journals(root) == before, 'color interactions changed canonical journal'
    finish(screen)
    return {'effective_target_fg': target_fg, 'effective_status_fg': status_fg,
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
    until(screen, lambda text: 'Предпросмотр темы' in text, 'theme picker')
    screen.send(b'\x1b[B')
    until(screen, lambda text: 'ANSA' in text and not attribute_is(screen, 'ANSA', 0, baseline), 'Pink live preview')
    snapshot(screen, 'pink-preview')
    escape(screen)
    until(screen, lambda text: 'Предпросмотр темы' not in text and attribute_is(screen, 'ANSA', 0, baseline), 'Esc rolled back theme')
    assert preferences(home) is None, 'preview wrote preferences before Enter'
    local(screen, '/theme')
    until(screen, lambda text: 'Предпросмотр темы' in text, 'theme picker for commit')
    screen.send(b'\x1b[B')
    until(screen, lambda text: 'ANSA' in text and not attribute_is(screen, 'ANSA', 0, baseline), 'Pink second preview')
    pink = screen.vt.attribute('ANSA')[0]
    screen.send(b'\r')
    until(screen, lambda text: preferences(home) == 'pink' and 'Предпросмотр темы' not in text, 'Enter persisted Pink')
    assert screen.vt.attribute('ANSA')[0] == pink
    choices = []
    last = pink
    for name in ['red', 'blue', 'amber']:
        local(screen, f'/theme {name}')
        until(screen, lambda text: 'ANSA' in text and 'Загрузка' not in text and not attribute_is(screen, 'ANSA', 0, last), f'rapid {name} accepted')
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
    until(restarted, lambda text: 'Предпросмотр темы' in text, 'DMS theme picker')
    restarted.send(b'\x1b[A')
    until(restarted, lambda text: 'ANSA' in text and not attribute_is(restarted, 'ANSA', 0, (240, 136, 186)), 'DMS picker preview')
    escape(restarted)
    until(restarted, lambda text: 'Предпросмотр темы' not in text and attribute_is(restarted, 'ANSA', 0, (240, 136, 186)), 'DMS preview rollback')
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

        def activity():
            return [(row, screen.vt.line(row)[0]) for row in range(3, screen.vt.rows - 5)
                    if screen.vt.line(row)[:1] in spinners and 'ожидание модели' in screen.vt.line(row)]

        until(screen, lambda text: 'EDIT_VISUAL_TASK' in screen.vt.line(3) and bool(activity()),
              'accepted user body and generating activity under fixture hold', timeout=3)
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
    until(screen, lambda text: completed(root, 'EDIT_VISUAL_TASK') and 'EDIT_DONE' in text and 'Изменён' in text, 'acknowledged edit inline diff')
    assert (cwd / 'edit-target.rs').read_text() == 'fn main() {\n    new();\n}\n'
    assert '(+1 -1)' in visible(screen)
    old_row, _ = screen.vt.location('old();')
    new_row, _ = screen.vt.location('new();')
    assert re.match(r'^\s*2\s+-\s+old\(\);', screen.vt.line(old_row)), screen.vt.line(old_row)
    assert re.match(r'^\s*2\s+\+\s+new\(\);', screen.vt.line(new_row)), screen.vt.line(new_row)
    assert screen.vt.line(old_row).index('2') < screen.vt.line(new_row).index('2'), 'old/new gutters share a column'
    assert screen.vt.line(old_row).index('-') == screen.vt.line(new_row).index('+'), 'diff markers misaligned'
    removed = screen.vt.attribute('old();')[1]
    added = screen.vt.attribute('new();')[1]
    context = screen.vt.attribute('fn main()')[1]
    assert removed is not None and added is not None and removed != added
    assert context is None and removed != context and added != context, 'diff colors must stay local against native context rows'
    native_background(screen, 'EDIT_VISUAL_TASK')
    screen.send('EDIT_DRAFT Ж')
    until(screen, lambda text: 'EDIT_DRAFT Ж' in screen.vt.line(-4), 'edit draft')
    native_background(screen, 'EDIT_DRAFT Ж')
    before = journals(root)
    snapshot(screen, 'inline-diff')
    row, column = screen.vt.location('Изменён')
    mouse(screen, 0, column, row)
    escape(screen)
    mouse(screen, 0, column, row, True)
    for _ in range(8): screen.drain(.01)
    assert 'Diff правки' not in visible(screen), 'Esc did not cancel the pending click'
    row, column = screen.vt.location('Изменён')
    click(screen, column, row)
    until(screen, lambda text: 'Diff правки' in text and 'j/k строки' in text, 'complete per-edit Diff inspector')
    assert screen.vt.attribute('old();')[1] == removed and screen.vt.attribute('new();')[1] == added
    assert screen.vt.attribute('fn main()')[1] is None
    assert 'EDIT_DRAFT Ж' in screen.vt.line(-4)
    snapshot(screen, 'full-diff')
    escape(screen)
    until(screen, lambda text: 'Diff правки' not in text and 'Изменён' in text and 'EDIT_DONE' in text, 'Esc returned from edit diff')
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
        try:
            quiet.send('EDIT_REDUCED_MOTION_TASK\r')

            def quiet_activity():
                return [row for row in range(3, quiet.vt.rows - 5)
                        if quiet.vt.line(row).startswith('◷ ') and 'ожидание модели' in quiet.vt.line(row)]

            until(quiet, lambda text: 'EDIT_REDUCED_MOTION_TASK' in quiet.vt.line(3) and bool(quiet_activity()),
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
        finally:
            try:
                finish(quiet)
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
        until(paused, lambda text: 'terminal_item_count' in text and 'пауза' in paused.vt.line(-2),
              'provider failure paused the request')
        assert '/resume' in paused.vt.line(-1) and 'Ctrl+C' in paused.vt.line(-1)
        paused.send('NEXT_TURN Ж\r')
        until(paused, lambda text: paused.vt.line(-4) == '› NEXT_TURN Ж', 'paused draft preserved')
        assert sum(row['kind'] == 'accepted' for row in records(failed_root)) == 1
        snapshot(paused, 'paused')
        rejection.unlink()
        paused.child.send_signal(signal.SIGINT)
        until(paused, lambda text: 'отменено' in paused.vt.line(-2), 'Ctrl+C cancelled the paused request')
        assert paused.vt.line(-4) == '› NEXT_TURN Ж'
        paused.send(b'\r')
        until(paused, lambda text: completed(failed_root, 'NEXT_TURN Ж') and 'EDIT_DONE' in text,
              'preserved draft sent after cancellation')
        assert sum(row['kind'] == 'accepted' and row['text'] == 'NEXT_TURN Ж'
                   for row in records(failed_root)) == 1
        finish(paused)
    return {'added': 1, 'removed': 1, 'effective_add_bg': added, 'effective_remove_bg': removed,
            'paused_draft_sent_after_cancel': True,
            'idle_animation': {'row': next(iter(observed))[0], 'spinner_frames': len(observed),
                               'elapsed': idle_elapsed, 'delta_bytes': len(idle_delta)},
            'reduced_motion': quiet_evidence}


def main():
    inventory = [('color-tool-selection-resize', colored_lifecycle),
                 ('theme-preview-persistence-dms', theme_preferences),
                 ('acknowledged-inline-edit-diff', inline_edit)]
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
        uses_fixture = case is inline_edit
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
