//! YAML input from Python: `str`, or `bytes` decoded like PyYAML and LibYAML.

use pyo3::prelude::*;
use widestring::decode_utf16;

use crate::exception;

const BOM_UTF16LE: [u8; 2] = [0xff, 0xfe];
const BOM_UTF16BE: [u8; 2] = [0xfe, 0xff];

/// A YAML stream passed from Python.
#[derive(FromPyObject)]
pub enum Source {
    Text(String),
    Bytes(Vec<u8>),
}

impl Source {
    /// The stream as text. A UTF-16 byte order mark selects UTF-16 (little or
    /// big endian); anything else is UTF-8. A BOM is kept as U+FEFF, which the
    /// parser skips. Bytes that do not decode raise `ReaderError`, a
    /// `yaml.YAMLError`, rather than `UnicodeDecodeError`.
    pub fn into_text(self, py: Python) -> PyResult<String> {
        match self {
            Source::Text(text) => Ok(text),
            Source::Bytes(bytes) => decode(&bytes).map_err(|message| {
                exception::yaml_error(py, "ReaderError", format!("Reader error: {message}"))
            }),
        }
    }
}

fn decode(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&BOM_UTF16LE) {
        decode_utf16_bytes(bytes, u16::from_le_bytes)
    } else if bytes.starts_with(&BOM_UTF16BE) {
        decode_utf16_bytes(bytes, u16::from_be_bytes)
    } else {
        String::from_utf8(bytes.to_vec()).map_err(|err| {
            let offset = err.utf8_error().valid_up_to();
            match err.utf8_error().error_len() {
                Some(_) => format!("invalid UTF-8 (offset {offset}, value {})", bytes[offset]),
                None => format!("incomplete UTF-8 octet sequence (offset {offset})"),
            }
        })
    }
}

fn decode_utf16_bytes(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, String> {
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err(format!(
            "incomplete UTF-16 character (offset {})",
            bytes.len() - 1
        ));
    }
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
                return Err(format!(
                    "invalid UTF-16 surrogate (offset {}, value {:#06x})",
                    unit_offset * 2,
                    err.unpaired_surrogate()
                ));
            }
        }
    }
    Ok(text)
}
