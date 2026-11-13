//! Bounded byte editor. Overflow stays sticky until the next submit boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Submit,
    Overflow,
    ToggleMode,
    ToggleHistory,
    PageUp,
    PageDown,
    Eof,
}
#[derive(Default, Debug)]
pub struct Editor {
    pub bytes: Vec<u8>,
    pub cursor: usize,
    escape: Vec<u8>,
    pub overflow: bool,
}
impl Editor {
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.cursor = 0;
        self.overflow = false;
    }
    pub fn text(&self) -> super::Result<&str> {
        std::str::from_utf8(&self.bytes).map_err(|_| "json_string_invalid")
    }
    pub fn feed(&mut self, ch: u8, terminal: bool) -> Option<Action> {
        if terminal {
            if self.escape == b"\x1b" && ch != b'[' && ch != 27 {
                self.escape.clear();
            }
            if !self.escape.is_empty() || ch == 27 {
                self.escape.push(ch);
                let action = match self.escape.as_slice() {
                    b"\x1b[D" => {
                        if self.cursor > 0 {
                            self.cursor -= 1;
                            while self.cursor > 0 && self.bytes[self.cursor] & 0xc0 == 0x80 {
                                self.cursor -= 1;
                            }
                        }
                        None
                    }
                    b"\x1b[C" => {
                        if self.cursor < self.bytes.len() {
                            self.cursor += 1;
                            while self.cursor < self.bytes.len()
                                && self.bytes[self.cursor] & 0xc0 == 0x80
                            {
                                self.cursor += 1;
                            }
                        }
                        None
                    }
                    b"\x1b[5~" => Some(Action::PageUp),
                    b"\x1b[6~" => Some(Action::PageDown),
                    b"\x1b[Z" => Some(Action::ToggleMode),
                    v => {
                        if v.len() > 5 || (v.len() >= 3 && !matches!(v[2], b'5' | b'6' | b'[')) {
                            self.escape.clear();
                        }
                        return None;
                    }
                };
                self.escape.clear();
                return action;
            }
            match ch {
                6 => return Some(Action::ToggleHistory),
                9 => return Some(Action::ToggleMode),
                4 => return self.bytes.is_empty().then_some(Action::Eof),
                127 | 8 => {
                    if !self.overflow && self.cursor > 0 {
                        let mut previous = self.cursor - 1;
                        while previous > 0 && self.bytes[previous] & 0xc0 == 0x80 {
                            previous -= 1;
                        }
                        self.bytes.drain(previous..self.cursor);
                        self.cursor = previous;
                    }
                    return None;
                }
                b'\r' | b'\n' => {
                    return Some(if self.overflow {
                        Action::Overflow
                    } else {
                        Action::Submit
                    });
                }
                0..=31 => return None,
                _ => {}
            }
        } else if ch == b'\n' {
            return Some(if self.overflow {
                Action::Overflow
            } else {
                Action::Submit
            });
        }
        if !self.overflow && self.bytes.len() < 16 * 1024 {
            self.bytes.insert(self.cursor, ch);
            self.cursor += 1;
        } else {
            self.overflow = true;
        }
        None
    }
}
