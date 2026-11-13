//! Persistent blocks retain immutable snapshots and bounded pages.
use crate::provider::contracts::{AgentMode, GetTranscript, RequestId, TurnId};
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub turn: TurnId,
    pub request: RequestId,
    pub role: String,
    pub text: String,
    pub state: String,
    pub mode: AgentMode,
}
#[derive(Clone, Debug)]
struct Block {
    previous: Option<Arc<Block>>,
    entries: Vec<Arc<Entry>>,
}
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    tail: Option<Arc<Block>>,
    size: usize,
}
#[derive(Clone, Debug)]
pub struct Page {
    pub begin: usize,
    pub end: usize,
    pub total: usize,
    pub entries: Vec<Entry>,
    pub query: GetTranscript,
}
impl Transcript {
    pub fn len(&self) -> usize {
        self.size
    }
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
    pub(crate) fn append(&mut self, entry: Entry) {
        let mut block = match &self.tail {
            Some(b) if b.entries.len() < 64 => (**b).clone(),
            _ => Block {
                previous: self.tail.clone(),
                entries: vec![],
            },
        };
        block.entries.push(Arc::new(entry));
        self.tail = Some(Arc::new(block));
        self.size += 1;
    }
    pub(crate) fn set_last(&mut self, state: &str) {
        let Some(tail) = &self.tail else {
            return;
        };
        let mut block = (**tail).clone();
        let mut entry = (**block.entries.last().expect("nonempty transcript block")).clone();
        entry.state = state.into();
        *block.entries.last_mut().expect("nonempty transcript block") = Arc::new(entry);
        self.tail = Some(Arc::new(block));
    }
    pub fn page(&self, query: GetTranscript) -> Page {
        let limit = query.limit.min(64);
        let mut entries = Vec::new();
        let mut bytes = 0;
        let (mut begin, mut end) = if query.forward {
            let begin = query.before.min(self.size);
            (begin, begin)
        } else {
            let end = if query.before == 0 {
                self.size
            } else {
                query.before.min(self.size)
            };
            (end, end)
        };
        let mut blocks = Vec::new();
        let mut cursor = self.size;
        let mut tail = self.tail.as_ref();
        while let Some(block) = tail {
            let start = cursor - block.entries.len();
            if query.forward {
                if cursor <= begin {
                    break;
                }
                if start < begin + limit.min(self.size - begin) {
                    blocks.push((start, block));
                }
            } else if start < end {
                blocks.push((start, block));
            }
            cursor = start;
            tail = block.previous.as_ref();
        }
        if query.forward {
            blocks.reverse();
            for (start, block) in blocks {
                for (i, entry) in block.entries.iter().enumerate() {
                    if start + i < begin {
                        continue;
                    }
                    if entries.len() == limit
                        || !entries.is_empty() && bytes + entry.text.len() > 256 * 1024
                    {
                        return Page {
                            begin,
                            end,
                            total: self.size,
                            entries,
                            query,
                        };
                    }
                    bytes += entry.text.len();
                    entries.push((**entry).clone());
                    end += 1;
                }
            }
        } else {
            for (start, block) in blocks {
                for (i, entry) in block.entries.iter().enumerate().rev() {
                    if start + i >= end {
                        continue;
                    }
                    if entries.len() == limit
                        || !entries.is_empty() && bytes + entry.text.len() > 256 * 1024
                    {
                        entries.reverse();
                        return Page {
                            begin,
                            end,
                            total: self.size,
                            entries,
                            query,
                        };
                    }
                    bytes += entry.text.len();
                    entries.push((**entry).clone());
                    begin -= 1;
                }
            }
            entries.reverse();
        }
        Page {
            begin,
            end,
            total: self.size,
            entries,
            query,
        }
    }
}
