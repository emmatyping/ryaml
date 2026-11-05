//! YAML input from Python: `str`, or `bytes` decoded like PyYAML and LibYAML.

use std::io::{self, BufRead, Cursor, Read};

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
    /// (little or big endian); anything else is UTF-8. A BOM is kept as U+FEFF,
    /// which the parser skips.
    pub fn into_input(self) -> Input {
        match self {
            Source::Text(text) => Input::new(text, None),
            Source::Bytes(bytes) => {
                let (text, error) = decode(&bytes);
                Input::new(text, error)
            }
        }
    }
}

/// Decoded text for the parser, plus the decode error (if any) after it.
///
/// libyaml decodes as it reads, so an error earlier in the stream (a scanner
/// error, say) wins over invalid bytes later on. Reading the valid prefix and
/// failing only when the parser asks for more reproduces that order; the
/// parser reports the failure as an I/O error, raised as `ReaderError`.
pub struct Input {
    text: Cursor<String>,
    error: Option<String>,
}

impl Input {
    fn new(text: String, error: Option<String>) -> Self {
        Self {
            text: Cursor::new(text),
            error,
        }
    }

    /// No input at all: the native `loads_all("")` convention returns None.
    pub fn is_empty(&self) -> bool {
        self.text.get_ref().is_empty() && self.error.is_none()
    }

    fn exhausted(&self) -> bool {
        self.text.position() as usize >= self.text.get_ref().len()
    }

    fn decode_error(&self) -> Option<io::Error> {
        let message = self.error.as_ref()?;
        Some(io::Error::new(io::ErrorKind::InvalidData, message.clone()))
    }
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.text.read(buf)?;
        if n == 0 && !buf.is_empty() {
            if let Some(err) = self.decode_error() {
                return Err(err);
            }
        }
        Ok(n)
    }
}

impl BufRead for Input {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.exhausted()
            && let Some(err) = self.decode_error()
        {
            return Err(err);
        }
        self.text.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.text.consume(amount);
    }
}

/// The longest decodable prefix of `bytes`, and the error that ends it.
fn decode(bytes: &[u8]) -> (String, Option<String>) {
    if bytes.starts_with(&BOM_UTF16LE) {
        decode_utf16_bytes(bytes, u16::from_le_bytes)
    } else if bytes.starts_with(&BOM_UTF16BE) {
        decode_utf16_bytes(bytes, u16::from_be_bytes)
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => (text.to_owned(), None),
            Err(err) => {
                let offset = err.valid_up_to();
                let prefix = std::str::from_utf8(&bytes[..offset])
                    .expect("valid_up_to marks a valid prefix")
                    .to_owned();
                let message = match err.error_len() {
                    Some(_) => format!("invalid UTF-8 (offset {offset}, value {})", bytes[offset]),
                    None => format!("incomplete UTF-8 octet sequence (offset {offset})"),
                };
                (prefix, Some(message))
            }
        }
    }
}

fn decode_utf16_bytes(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> (String, Option<String>) {
    let chunks = bytes.chunks_exact(2);
    let odd_length = !chunks.remainder().is_empty();
    let units = chunks.map(|pair| unit([pair[0], pair[1]]));
    let mut text = String::with_capacity(bytes.len() / 2);
    let mut unit_offset = 0;
    for ch in decode_utf16(units) {
        match ch {
            Ok(ch) => {
                text.push(ch);
                unit_offset += ch.len_utf16();
            }
            Err(err) => {
                let message = format!(
                    "invalid UTF-16 surrogate (offset {}, value {:#06x})",
                    unit_offset * 2,
                    err.unpaired_surrogate()
                );
                return (text, Some(message));
            }
        }
    }
    let error =
        odd_length.then(|| format!("incomplete UTF-16 character (offset {})", bytes.len() - 1));
    (text, error)
}
