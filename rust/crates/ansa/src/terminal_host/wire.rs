use super::protocol::VERSION;
use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

const MAX_FRAME: usize = 64 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u16,
    sequence: u64,
    value: T,
}

pub(super) fn write<T: Serialize>(out: &mut impl Write, sequence: u64, value: T) -> io::Result<()> {
    write_limited(out, sequence, value, MAX_FRAME)
}

pub(super) fn write_limited<T: Serialize>(
    out: &mut impl Write,
    sequence: u64,
    value: T,
    limit: usize,
) -> io::Result<()> {
    let envelope = Envelope {
        version: VERSION,
        sequence,
        value,
    };
    let body = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
    if body.len() > limit {
        return Err(io::Error::other("terminal_frame_size"));
    }
    out.write_all(&(body.len() as u32).to_be_bytes())?;
    out.write_all(&body)?;
    out.flush()
}

pub(super) fn read<T: DeserializeOwned>(input: &mut impl Read, sequence: u64) -> io::Result<T> {
    read_limited(input, sequence, MAX_FRAME)
}

pub(super) fn read_limited<T: DeserializeOwned>(
    input: &mut impl Read,
    sequence: u64,
    limit: usize,
) -> io::Result<T> {
    let mut header = [0; 4];
    input.read_exact(&mut header)?;
    let mut body = vec![0; length(header, limit)?];
    input.read_exact(&mut body)?;
    decode(&body, sequence)
}

#[derive(Default)]
pub(super) struct Reader {
    bytes: Vec<u8>,
}

impl Reader {
    /// Process at most one bounded read per worker tick. Nonblocking output
    /// draining and lifetime enforcement continue during a partial frame.
    pub(super) fn poll<T: DeserializeOwned>(
        &mut self,
        input: &mut impl Read,
        sequence: u64,
    ) -> io::Result<Option<T>> {
        let mut chunk = [0; 4096];
        let next = if self.bytes.len() < 4 {
            4
        } else {
            4 + length(
                self.bytes[..4].try_into().expect("length prefix"),
                MAX_FRAME,
            )?
        };
        let capacity = (next - self.bytes.len()).min(chunk.len());
        match input.read(&mut chunk[..capacity]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => self.bytes.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let len = length(
            self.bytes[..4].try_into().expect("length prefix"),
            MAX_FRAME,
        )?;
        if self.bytes.len() < len + 4 {
            return Ok(None);
        }
        let value = decode(&self.bytes[4..], sequence)?;
        self.bytes.clear();
        Ok(Some(value))
    }
}

fn length(header: [u8; 4], limit: usize) -> io::Result<usize> {
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > limit {
        return Err(io::Error::other("terminal_frame_size"));
    }
    Ok(size)
}

fn decode<T: DeserializeOwned>(body: &[u8], sequence: u64) -> io::Result<T> {
    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(io::Error::other)?;
    if envelope.version != VERSION || envelope.sequence != sequence {
        return Err(io::Error::other("terminal_protocol_sequence"));
    }
    Ok(envelope.value)
}
