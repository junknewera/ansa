//! Raw child input with bounded escape framing and isolated mouse/paste parsing.
use super::input::{MouseEvent, MouseKind};

const BEGIN: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";
const SEQUENCE_LIMIT: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Bytes(Vec<u8>),
    Mouse {
        event: MouseEvent,
        button: u16,
        release: bool,
    },
    Escape,
}

#[derive(Debug, Default)]
pub struct TerminalInput {
    sequence: Vec<u8>,
    paste: bool,
    paste_end: Vec<u8>,
    pass_delimiters: bool,
}

fn bytes(events: &mut Vec<TerminalEvent>, value: &[u8]) {
    if value.is_empty() {
        return;
    }
    if let Some(TerminalEvent::Bytes(previous)) = events.last_mut() {
        previous.extend_from_slice(value);
    } else {
        events.push(TerminalEvent::Bytes(value.into()));
    }
}

impl TerminalInput {
    pub fn pending_escape(&self) -> bool {
        !self.sequence.is_empty()
    }

    /// The caller supplies the short idle deadline; incomplete CSI stays raw.
    pub fn flush_escape(&mut self) -> Option<TerminalEvent> {
        if self.sequence.is_empty() {
            return None;
        }
        let sequence = std::mem::take(&mut self.sequence);
        Some(if sequence == b"\x1b" {
            TerminalEvent::Escape
        } else {
            TerminalEvent::Bytes(sequence)
        })
    }

    /// Return all bytes not yet delivered before leaving terminal focus.
    pub fn take_pending(&mut self) -> Vec<u8> {
        let mut pending = std::mem::take(&mut self.sequence);
        pending.append(&mut self.paste_end);
        self.paste = false;
        self.pass_delimiters = false;
        pending
    }

    pub fn feed(&mut self, input: &[u8], bracketed_paste: bool) -> Vec<TerminalEvent> {
        let mut events = Vec::new();
        for &byte in input {
            if self.paste {
                self.paste_end.push(byte);
                if self.paste_end == END {
                    if self.pass_delimiters {
                        bytes(&mut events, END);
                    }
                    self.paste_end.clear();
                    self.paste = false;
                } else {
                    while !END.starts_with(&self.paste_end) {
                        bytes(&mut events, &self.paste_end[..1]);
                        self.paste_end.remove(0);
                    }
                }
                continue;
            }
            if self.sequence.is_empty() {
                if byte == 0x1b {
                    self.sequence.push(byte);
                } else {
                    bytes(&mut events, &[byte]);
                }
                continue;
            }
            self.sequence.push(byte);
            let framed = self
                .sequence
                .get(1)
                .is_some_and(|byte| matches!(*byte, b'[' | b'O'));
            let complete = !framed || (self.sequence.len() > 2 && (0x40..=0x7e).contains(&byte));
            if !complete && self.sequence.len() < SEQUENCE_LIMIT {
                continue;
            }
            let sequence = std::mem::take(&mut self.sequence);
            if sequence == BEGIN {
                self.paste = true;
                self.pass_delimiters = bracketed_paste;
                if bracketed_paste {
                    bytes(&mut events, &sequence);
                }
            } else if let Some(event) = mouse(&sequence) {
                events.push(event);
            } else {
                bytes(&mut events, &sequence);
            }
        }
        events
    }
}

fn mouse(sequence: &[u8]) -> Option<TerminalEvent> {
    let packet = sequence.strip_prefix(b"\x1b[<")?;
    let (&end, coordinates) = packet.split_last()?;
    if !matches!(end, b'M' | b'm') {
        return None;
    }
    let text = std::str::from_utf8(coordinates).ok()?;
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b';')
    {
        return None;
    }
    let mut fields = text.split(';');
    let button = fields.next()?.parse::<u16>().ok()?;
    let column = usize::from(fields.next()?.parse::<u16>().ok()?.checked_sub(1)?);
    let row = usize::from(fields.next()?.parse::<u16>().ok()?.checked_sub(1)?);
    if fields.next().is_some() {
        return None;
    }
    let release = end == b'm';
    let kind = if release {
        MouseKind::Release
    } else if button & 64 != 0 && button & 3 == 0 {
        MouseKind::WheelUp
    } else if button & 64 != 0 && button & 3 == 1 {
        MouseKind::WheelDown
    } else if button & 32 != 0 {
        MouseKind::Drag
    } else {
        MouseKind::Press
    };
    Some(TerminalEvent::Mouse {
        event: MouseEvent { column, row, kind },
        button,
        release,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw(events: &[TerminalEvent]) -> Vec<u8> {
        events
            .iter()
            .flat_map(|event| match event {
                TerminalEvent::Bytes(bytes) => bytes.as_slice(),
                _ => panic!("unexpected action: {event:?}"),
            })
            .copied()
            .collect()
    }
    #[test]
    fn keyboard_alt_utf8_controls_and_unknown_csi_are_exact_and_coalesced() {
        let input = b"abc\x03\t\x1bx\x1b\xc3\xa9\x1b[1;5D\x1bOP\x1b[999u\xff";
        for split in 0..=input.len() {
            let mut decoder = TerminalInput::default();
            let mut events = decoder.feed(&input[..split], false);
            events.extend(decoder.feed(&input[split..], false));
            if let Some(event) = decoder.flush_escape() {
                events.push(event);
            }
            assert_eq!(raw(&events), input);
        }
        assert_eq!(
            TerminalInput::default().feed(input, false),
            vec![TerminalEvent::Bytes(input.to_vec())]
        );
    }
    #[test]
    fn only_standalone_escape_is_an_action_and_partial_sequences_are_raw() {
        let mut decoder = TerminalInput::default();
        assert!(decoder.feed(b"\x1b", false).is_empty());
        assert!(decoder.pending_escape());
        assert_eq!(decoder.flush_escape(), Some(TerminalEvent::Escape));
        assert_eq!(decoder.flush_escape(), None);
        assert!(decoder.feed(b"\x1b[<0;12", false).is_empty());
        assert_eq!(
            decoder.flush_escape(),
            Some(TerminalEvent::Bytes(b"\x1b[<0;12".to_vec()))
        );
        let input = [b"\x1b[".as_slice(), &[b'1'; 200]].concat();
        let events = decoder.feed(&input, false);
        let mut returned = raw(&events);
        returned.extend(decoder.take_pending());
        assert_eq!(returned, input);
    }
    #[test]
    fn embedded_mouse_preserves_button_modifiers_and_separates_adjacent_text() {
        let input = b"before\x1b[<52;12;7Mafter\x1b[<2;4;3mtail";
        for split in 0..=input.len() {
            let mut decoder = TerminalInput::default();
            let mut events = decoder.feed(&input[..split], false);
            events.extend(decoder.feed(&input[split..], false));
            assert!(events.contains(&TerminalEvent::Mouse {
                event: MouseEvent {
                    column: 11,
                    row: 6,
                    kind: MouseKind::Drag
                },
                button: 52,
                release: false
            }));
            assert!(events.contains(&TerminalEvent::Mouse {
                event: MouseEvent {
                    column: 3,
                    row: 2,
                    kind: MouseKind::Release
                },
                button: 2,
                release: true
            }));
            let text = events
                .iter()
                .filter_map(|event| {
                    if let TerminalEvent::Bytes(value) = event {
                        Some(value.as_slice())
                    } else {
                        None
                    }
                })
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            assert_eq!(text, b"beforeaftertail");
        }
        assert_eq!(TerminalInput::default().feed(input, false).len(), 5);
    }
    #[test]
    fn paste_body_never_becomes_mouse_or_escape_and_delimiters_follow_child_mode() {
        let body = b"text\x1b[<0;1;1M\x1b\x03\t\x1b[20x\x1b[201";
        let input = [BEGIN, body, END, b"tail"].concat();
        for enabled in [false, true] {
            for split in 0..=input.len() {
                let mut decoder = TerminalInput::default();
                let mut events = decoder.feed(&input[..split], enabled);
                events.extend(decoder.feed(&input[split..], enabled));
                assert!(!decoder.pending_escape());
                assert_eq!(decoder.flush_escape(), None);
                assert_eq!(
                    raw(&events),
                    if enabled {
                        input.clone()
                    } else {
                        [body.as_slice(), b"tail"].concat()
                    }
                );
            }
        }
        let mut decoder = TerminalInput::default();
        assert!(decoder.feed(BEGIN, false).is_empty());
        assert!(decoder.feed(b"\x1b[20", false).is_empty());
        assert!(!decoder.pending_escape());
        assert_eq!(decoder.take_pending(), b"\x1b[20");
        assert_eq!(
            decoder.feed(b"fresh", false),
            vec![TerminalEvent::Bytes(b"fresh".to_vec())]
        );
    }
    #[test]
    fn malformed_mouse_is_preserved_instead_of_inventing_an_action() {
        for packet in [
            b"\x1b[<0;0;1M".as_slice(),
            b"\x1b[<65536;1;1M",
            b"\x1b[<0;1;1;2M",
            b"\x1b[<0;65536;1M",
            b"\x1b[<+0;1;1M",
        ] {
            assert_eq!(
                TerminalInput::default().feed(packet, false),
                vec![TerminalEvent::Bytes(packet.to_vec())]
            );
        }
    }
}
