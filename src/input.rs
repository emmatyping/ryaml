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
                let chunk_ends = utf8_chunk_ends(text.len());
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

/// Chunk ends for UTF-8 text: every `RAW_CHUNK` bytes.
fn utf8_chunk_ends(len: usize) -> Vec<usize> {
    (1..=len / RAW_CHUNK)
        .map(|k| k * RAW_CHUNK)
        .filter(|&end| end < len)
        .collect()
}

/// Where the parser must be before an error at raw byte `offset` is reported,
/// for UTF-8 text whose bytes are the raw bytes: the start of its chunk.
fn utf8_chunk_start(offset: usize) -> usize {
    offset / RAW_CHUNK * RAW_CHUNK
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
            Ok(text) => (text.to_owned(), utf8_chunk_ends(text.len()), None),
            Err(err) => {
                let offset = err.valid_up_to();
                let prefix = std::str::from_utf8(&bytes[..offset])
                    .expect("valid_up_to marks a valid prefix")
                    .to_owned();
                let error = match err.error_len() {
                    Some(_) => DecodeError {
                        message: format!(
                            "invalid UTF-8 (offset {offset}, value {})",
                            bytes[offset]
                        ),
                        reported_at: utf8_chunk_start(offset),
                    },
                    // An incomplete sequence at the end waits for more input,
                    // so it is only reported once everything before it is used.
                    None => DecodeError {
                        message: format!("incomplete UTF-8 octet sequence (offset {offset})"),
                        reported_at: prefix.len(),
                    },
                };
                let chunk_ends = utf8_chunk_ends(prefix.len());
                (prefix, chunk_ends, Some(error))
            }
        }
    }
}

fn decode_utf16_bytes(
    bytes: &[u8],
    unit: fn([u8; 2]) -> u16,
) -> (String, Vec<usize>, Option<DecodeError>) {
    let chunks = bytes.chunks_exact(2);
    let odd_length = !chunks.remainder().is_empty();
    let unit_count = bytes.len() / 2;
    let units = chunks.map(|pair| unit([pair[0], pair[1]]));
    let mut text = String::with_capacity(unit_count);
    let mut chunk_ends = Vec::new();
    // Text offset at the start of each raw chunk, by chunk index.
    let mut chunk_starts = vec![0];
    let mut byte_offset = 0;
    for ch in decode_utf16(units) {
        match ch {
            Ok(ch) => {
                // A character belongs to the chunk holding its last byte, as
                // libyaml carries a split surrogate pair into the next chunk.
                let end = byte_offset + ch.len_utf16() * 2;
                while (end - 1) / RAW_CHUNK >= chunk_starts.len() {
                    chunk_ends.push(text.len());
                    chunk_starts.push(text.len());
                }
                text.push(ch);
                byte_offset = end;
            }
            Err(err) => {
                let surrogate = err.unpaired_surrogate();
                // A high surrogate as the very last unit is an incomplete pair,
                // reported once everything before it is used; any other
                // unpaired surrogate is invalid, reported with its chunk.
                let at_end =
                    (0xd800..0xdc00).contains(&surrogate) && byte_offset / 2 + 1 == unit_count;
                let chunk = byte_offset / RAW_CHUNK;
                let reported_at = if at_end {
                    text.len()
                } else {
                    chunk_starts.get(chunk).copied().unwrap_or(text.len())
                };
                let message = format!(
                    "{} UTF-16 surrogate (offset {byte_offset}, value {surrogate:#06x})",
                    if at_end { "incomplete" } else { "invalid" },
                );
                return (
                    text,
                    chunk_ends,
                    Some(DecodeError {
                        message,
                        reported_at,
                    }),
                );
            }
        }
    }
    let error = odd_length.then(|| DecodeError {
        message: format!("incomplete UTF-16 character (offset {})", bytes.len() - 1),
        reported_at: text.len(),
    });
    (text, chunk_ends, error)
}
