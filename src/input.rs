//! YAML input from Python: `str`, or `bytes` decoded like PyYAML and LibYAML.

use std::io::{self, BufRead, Read};

use pyo3::prelude::*;
use widestring::decode_utf16;

const BOM_UTF16LE: [u8; 2] = [0xff, 0xfe];
const BOM_UTF16BE: [u8; 2] = [0xfe, 0xff];

/// A YAML stream passed from Python.
#[derive(FromPyObject)]
pub enum Source {
    Text(String),
    Bytes(Vec<u8>),
}

impl Source {
    /// The stream for the parser. A UTF-16 byte order mark selects UTF-16
    /// (little or big endian); anything else is UTF-8 (YAML 1.1 section 5.2).
    /// A BOM is kept as U+FEFF, which the parser skips.
    pub fn into_input(self) -> Input {
        match self {
            Source::Text(text) => {
                let starts = utf8_chunk_starts(text.as_bytes(), text.len());
                let chunk_ends = chunk_ends_below(&starts, text.len());
                Input::new(text, chunk_ends, None)
            }
            Source::Bytes(bytes) => {
                let (text, chunk_ends, error) = decode(&bytes);
                Input::new(text, chunk_ends, error)
            }
        }
    }
}

/// libyaml's raw input buffer (`INPUT_RAW_BUFFER_SIZE`): it reads and decodes
/// the stream in chunks of this many bytes, checking every character of a chunk
/// when the parser first needs anything from it.
const RAW_CHUNK: usize = 16384;

/// A decoding error and when libyaml reports it.
struct DecodeError {
    message: String,
    /// Where in the decoded text the parser must be before libyaml reports the
    /// error: the start of the raw chunk holding an invalid sequence, or the end
    /// of the text for an incomplete sequence at the end of the input.
    reported_at: usize,
}

/// Decoded text for the parser, handed over in libyaml's raw chunks, plus the
/// decoding error (if any) after it. The parser reports the error as an I/O
/// error, raised as `ReaderError`.
pub struct Input {
    text: String,
    position: usize,
    /// Text offsets where libyaml's raw chunks end (increasing, last is the end).
    chunk_ends: Vec<usize>,
    error: Option<DecodeError>,
}

impl Input {
    fn new(text: String, chunk_ends: Vec<usize>, error: Option<DecodeError>) -> Self {
        Self {
            text,
            position: 0,
            chunk_ends,
            error,
        }
    }

    /// No input at all: the native `loads_all("")` convention returns None.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.error.is_none()
    }

    fn pending_error(&self) -> Option<io::Error> {
        let error = self.error.as_ref()?;
        (self.position >= error.reported_at)
            .then(|| io::Error::new(io::ErrorKind::InvalidData, error.message.clone()))
    }

    /// The end of the chunk the parser is reading.
    fn chunk_end(&self) -> usize {
        let index = self.chunk_ends.partition_point(|&end| end <= self.position);
        self.chunk_ends
            .get(index)
            .copied()
            .unwrap_or(self.text.len())
    }
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(err) = self.pending_error() {
            return Err(err);
        }
        // Used for the rest of a character split across chunks, so ignore the
        // chunk boundary (libyaml carries such a character into the next chunk).
        let available = &self.text.as_bytes()[self.position..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.position += n;
        Ok(n)
    }
}

impl BufRead for Input {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if let Some(err) = self.pending_error() {
            return Err(err);
        }
        let end = self.chunk_end();
        Ok(&self.text.as_bytes()[self.position..end])
    }

    fn consume(&mut self, amount: usize) {
        self.position += amount;
    }
}

/// The width libyaml gives a UTF-8 sequence from its lead byte (0: invalid).
fn utf8_width(lead: u8) -> usize {
    match lead {
        b if b & 0x80 == 0x00 => 1,
        b if b & 0xe0 == 0xc0 => 2,
        b if b & 0xf0 == 0xe0 => 3,
        b if b & 0xf8 == 0xf0 => 4,
        _ => 0,
    }
}

/// Byte offsets where libyaml's raw chunks start in valid UTF-8 `bytes`, up to
/// `limit`. Each read fills the raw buffer to `RAW_CHUNK` bytes; a character
/// cut off at the end of a chunk is carried into the next one, which therefore
/// starts at that character.
fn utf8_chunk_starts(bytes: &[u8], limit: usize) -> Vec<usize> {
    let mut starts = vec![0];
    let mut start = 0;
    while start + RAW_CHUNK <= limit {
        let end = start + RAW_CHUNK;
        let mut lead = end - 1;
        while lead > start && end - lead < 4 && bytes[lead] & 0xc0 == 0x80 {
            lead -= 1;
        }
        start = if lead + utf8_width(bytes[lead]) > end {
            lead
        } else {
            end
        };
        starts.push(start);
    }
    starts
}

/// Chunk ends (as text offsets) for UTF-8 text, from its chunk starts.
fn chunk_ends_below(starts: &[usize], len: usize) -> Vec<usize> {
    starts[1..]
        .iter()
        .copied()
        .filter(|&end| end < len)
        .collect()
}

/// When libyaml reports a bad sequence starting at raw byte `offset` that
/// needs `need` bytes: once the parser reaches it if the bytes run past the
/// chunk (or the input), as libyaml then waits for more; otherwise as soon as
/// the chunk is read. `chunk` is the chunk's (start, text offset of start).
fn reported_at(
    offset: usize,
    need: usize,
    len: usize,
    chunk: (usize, usize),
    text_offset: usize,
) -> usize {
    let available_end = len.min(chunk.0 + RAW_CHUNK);
    if need > 0 && offset + need > available_end {
        text_offset
    } else {
        chunk.1
    }
}

/// The longest decodable prefix of `bytes`, its chunk ends, and the error that
/// ends it.
fn decode(bytes: &[u8]) -> (String, Vec<usize>, Option<DecodeError>) {
    if bytes.starts_with(&BOM_UTF16LE) {
        decode_utf16_bytes(bytes, u16::from_le_bytes)
    } else if bytes.starts_with(&BOM_UTF16BE) {
        decode_utf16_bytes(bytes, u16::from_be_bytes)
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                let starts = utf8_chunk_starts(bytes, text.len());
                (text.to_owned(), chunk_ends_below(&starts, text.len()), None)
            }
            Err(err) => {
                let offset = err.valid_up_to();
                let prefix = std::str::from_utf8(&bytes[..offset])
                    .expect("valid_up_to marks a valid prefix")
                    .to_owned();
                let starts = utf8_chunk_starts(bytes, offset);
                let chunk_start = *starts.last().expect("at least one chunk");
                let need = utf8_width(bytes[offset]);
                let at = reported_at(
                    offset,
                    need,
                    bytes.len(),
                    (chunk_start, chunk_start),
                    offset,
                );
                let message = if at == offset && need > 0 && offset + need > bytes.len() {
                    format!("incomplete UTF-8 octet sequence (offset {offset})")
                } else {
                    format!("invalid UTF-8 (offset {offset}, value {})", bytes[offset])
                };
                let error = DecodeError {
                    message,
                    reported_at: at,
                };
                (prefix, chunk_ends_below(&starts, offset), Some(error))
            }
        }
    }
}

fn decode_utf16_bytes(
    bytes: &[u8],
    unit: fn([u8; 2]) -> u16,
) -> (String, Vec<usize>, Option<DecodeError>) {
    let units = bytes.as_chunks::<2>().0.iter().map(|&pair| unit(pair));
    let mut text = String::with_capacity(bytes.len() / 2);
    let mut chunk_ends = Vec::new();
    // The current chunk: its first byte and the text offset there.
    let mut chunk = (0, 0);
    let mut byte_offset = 0;
    for ch in decode_utf16(units) {
        match ch {
            Ok(ch) => {
                // A character cut off by the end of the chunk starts the next.
                let end = byte_offset + ch.len_utf16() * 2;
                if end > chunk.0 + RAW_CHUNK {
                    chunk_ends.push(text.len());
                    chunk = (byte_offset, text.len());
                }
                text.push(ch);
                byte_offset = end;
            }
            Err(err) => {
                let surrogate = err.unpaired_surrogate();
                if byte_offset >= chunk.0 + RAW_CHUNK {
                    chunk_ends.push(text.len());
                    chunk = (byte_offset, text.len());
                }
                // A high surrogate needs a following low one (4 bytes); a lone
                // low surrogate is invalid on its own.
                let need = if (0xd800..0xdc00).contains(&surrogate) {
                    4
                } else {
                    2
                };
                let at = reported_at(byte_offset, need, bytes.len(), chunk, text.len());
                let incomplete = byte_offset + need > bytes.len();
                let message = format!(
                    "{} UTF-16 surrogate (offset {byte_offset}, value {surrogate:#06x})",
                    if incomplete { "incomplete" } else { "invalid" },
                );
                let error = DecodeError {
                    message,
                    reported_at: at,
                };
                return (text, chunk_ends, Some(error));
            }
        }
    }
    let error = (bytes.len() % 2 == 1).then(|| DecodeError {
        message: format!("incomplete UTF-16 character (offset {})", bytes.len() - 1),
        reported_at: text.len(),
    });
    (text, chunk_ends, error)
}
