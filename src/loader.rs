//! Module implement pyyaml compatibility layer for ryaml via libyaml
//! Implements RLoader, which can load YAML 1.1

use libyaml_safer::{Event, EventData, Parser};
use pyo3::exceptions::PyNotImplementedError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};
use rustc_hash::FxBuildHasher;
use std::collections::HashMap;
use std::io::Cursor;

use crate::exception;
use crate::resolver;

#[pyclass(name = "_RSafeLoader", subclass)]
pub struct RSafeLoader {
    /// Parser over an in-memory string passed by Python
    parser: Parser<Cursor<String>>,
    /// Event used by internal parser
    parsed_event: Option<Event>,
    /// Anchors mapping anchor name to constructed Python object
    anchors: HashMap<String, Py<PyAny>, FxBuildHasher>,
}

#[pymethods]
impl RSafeLoader {
    #[new]
    pub fn new(source: String) -> Self {
        let mut parser = Parser::new();
        parser.set_input(Cursor::new(source));
        Self {
            parser,
            parsed_event: None,
            anchors: HashMap::with_hasher(FxBuildHasher),
        }
    }

    pub fn peek_token(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Tokenizing is not implemented",
        ))
    }

    pub fn check_token(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Tokenizing is not implemented",
        ))
    }

    pub fn get_token(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Tokenizing is not implemented",
        ))
    }

    /// Check if there's data available
    pub fn check_data(&mut self, py: Python) -> PyResult<bool> {
        self.check_node(py)
    }

    /// Get the next document as a Python object
    pub fn get_data(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        if self.check_node(py)? {
            return self.construct_document(py);
        }
        Ok(None)
    }

    /// Get a single document as a Python object
    pub fn get_single_data(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        // Eat stream start event
        self._parse_next_event(py)?;
        self.parsed_event = None;

        // Get document
        self._parse_next_event(py)?;
        let document = if !matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::StreamEnd,
                ..
            })
        ) {
            self.construct_document(py)?
        } else {
            None
        };

        // Make sure there are no more documents
        self._parse_next_event(py)?;
        if !matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::StreamEnd,
                ..
            })
        ) {
            return Err(exception::composer_error(
                py,
                "expected a single document in the stream, but found another document".to_string(),
            ));
        }

        Ok(document)
    }

    pub fn dispose(&self) {}
}

impl RSafeLoader {
    fn check_node(&mut self, py: Python) -> PyResult<bool> {
        self._parse_next_event(py)?;
        if matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::StreamStart { .. },
                ..
            })
        ) {
            self.parsed_event = None;
            self._parse_next_event(py)?;
        }
        if matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::StreamEnd,
                ..
            })
        ) {
            return Ok(false);
        }
        Ok(true)
    }

    /// Parse the next event if needed
    fn _parse_next_event(&mut self, py: Python) -> PyResult<()> {
        if self.parsed_event.is_none() {
            match self.parser.parse() {
                Ok(event) => {
                    self.parsed_event = Some(event);
                }
                Err(e) => return Err(exception::scanner_error(py, format!("{}", e))),
            }
        }
        Ok(())
    }

    /// Construct a document directly from events
    fn construct_document(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        // Eat document start event
        self.parsed_event = None;

        // Construct the root object directly from events
        self._parse_next_event(py)?;
        let result = self.construct_from_events(py)?;

        // Eat document end event
        self._parse_next_event(py)?;
        self.parsed_event = None;

        // Clear anchors for next document
        self.anchors.clear();

        Ok(Some(result))
    }

    /// Core single-pass constructor: consume the current event and produce a Python object
    fn construct_from_events(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        let event = self.parsed_event.take().unwrap();
        match event.data {
            EventData::Alias { anchor } => {
                if let Some(obj) = self.anchors.get(&anchor) {
                    Ok(obj.clone_ref(py))
                } else {
                    Err(exception::composer_error(
                        py,
                        format!("found undefined alias '{}'", anchor),
                    ))
                }
            }
            EventData::Scalar {
                anchor,
                tag,
                value,
                plain_implicit,
                ..
            } => self.construct_scalar_direct(py, anchor, tag, value, plain_implicit),
            EventData::SequenceStart { anchor, tag, .. } => {
                self.construct_sequence_direct(py, anchor, tag)
            }
            EventData::MappingStart { anchor, tag, .. } => {
                self.construct_mapping_direct(py, anchor, tag)
            }
            _ => Err(exception::composer_error(
                py,
                format!("unexpected event: {:?}", event.data),
            )),
        }
    }

    /// Construct a Python object directly from a scalar event
    fn construct_scalar_direct(
        &mut self,
        py: Python,
        anchor: Option<String>,
        tag: Option<String>,
        value: String,
        plain_implicit: bool,
    ) -> PyResult<Py<PyAny>> {
        // Resolve tag inline — &'static str, no allocation for common case
        let resolved_tag: &str = if let Some(ref t) = tag {
            t.as_str()
        } else {
            resolver::resolve_scalar_tag(&value, plain_implicit)
        };

        let result = match resolved_tag {
            crate::TAG_NULL => py.None(),
            crate::TAG_BOOL => construct_bool_direct(py, &value)?,
            crate::TAG_INT => construct_int_direct(py, &value)?,
            crate::TAG_FLOAT => construct_float_direct(py, &value)?,
            // str, timestamp, value, merge, and unknown tags all produce strings
            _ => PyString::new(py, &value).into_any().unbind(),
        };

        if let Some(anchor_name) = anchor {
            self.anchors.insert(anchor_name, result.clone_ref(py));
        }

        Ok(result)
    }

    /// Construct a Python list directly from sequence events
    fn construct_sequence_direct(
        &mut self,
        py: Python,
        anchor: Option<String>,
        _tag: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let list = PyList::empty(py);
        let list_obj: Py<PyAny> = list.clone().unbind().into_any();

        // Store in anchors BEFORE recursing (handles circular references)
        if let Some(anchor_name) = anchor {
            self.anchors.insert(anchor_name, list_obj.clone_ref(py));
        }

        // Consume child events until SequenceEnd
        loop {
            self._parse_next_event(py)?;
            if matches!(
                &self.parsed_event,
                Some(Event {
                    data: EventData::SequenceEnd,
                    ..
                })
            ) {
                break;
            }
            let item = self.construct_from_events(py)?;
            list.append(item)?;
        }

        self.parsed_event = None;
        Ok(list_obj)
    }

    /// Construct a Python dict directly from mapping events, with inline merge key handling
    fn construct_mapping_direct(
        &mut self,
        py: Python,
        anchor: Option<String>,
        tag: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let is_set = tag.as_deref() == Some(crate::TAG_SET);

        let dict = PyDict::new(py);
        let dict_obj: Py<PyAny> = dict.clone().unbind().into_any();

        // Store in anchors BEFORE recursing (handles circular references)
        if let Some(anchor_name) = anchor {
            self.anchors.insert(anchor_name, dict_obj.clone_ref(py));
        }

        let mut merge_sources: Vec<Py<PyAny>> = Vec::new();

        loop {
            self._parse_next_event(py)?;
            if matches!(
                &self.parsed_event,
                Some(Event {
                    data: EventData::MappingEnd,
                    ..
                })
            ) {
                break;
            }

            // Check if the key is a merge key BEFORE constructing it
            let is_merge = is_merge_key(&self.parsed_event);

            let key = self.construct_from_events(py)?;

            // Parse the value
            self._parse_next_event(py)?;
            let value = self.construct_from_events(py)?;

            if is_set {
                let hashable_key = self.make_hashable(py, key)?;
                dict.set_item(hashable_key, py.None())?;
                continue;
            }

            if is_merge {
                // Collect merge source(s)
                if let Ok(value_list) = value.downcast_bound::<PyList>(py) {
                    for item in value_list.iter() {
                        merge_sources.push(item.unbind());
                    }
                } else {
                    merge_sources.push(value);
                }
                continue;
            }

            let hashable_key = self.make_hashable(py, key)?;
            dict.set_item(hashable_key, value)?;
        }

        // Apply merge sources: explicit keys take precedence, then first merge source wins
        if !merge_sources.is_empty() {
            for source in &merge_sources {
                if let Ok(source_dict) = source.downcast_bound::<PyDict>(py) {
                    for (k, v) in source_dict.iter() {
                        if !dict.contains(&k)? {
                            dict.set_item(&k, v)?;
                        }
                    }
                }
            }
        }

        self.parsed_event = None;
        Ok(dict_obj)
    }

    /// Convert unhashable types (dict, list) to tuples for use as dict keys
    fn make_hashable(&self, py: Python, obj: Py<PyAny>) -> PyResult<Py<PyAny>> {
        if let Ok(dict) = obj.downcast_bound::<PyDict>(py) {
            let mut items = Vec::new();
            for (key, value) in dict.iter() {
                let hashable_key = self.make_hashable(py, key.unbind())?;
                let hashable_value = self.make_hashable(py, value.unbind())?;
                let pair = pyo3::types::PyTuple::new(py, &[hashable_key, hashable_value])?;
                items.push(pair);
            }
            let tuple = pyo3::types::PyTuple::new(py, &items)?;
            return Ok(tuple.unbind().into_any());
        }

        if let Ok(list) = obj.downcast_bound::<PyList>(py) {
            let mut items = Vec::new();
            for item in list.iter() {
                let hashable_item = self.make_hashable(py, item.unbind())?;
                items.push(hashable_item);
            }
            let tuple = pyo3::types::PyTuple::new(py, &items)?;
            return Ok(tuple.unbind().into_any());
        }

        Ok(obj)
    }
}

/// Check if the current event is a merge key (plain scalar "<<" or explicit merge tag)
fn is_merge_key(event: &Option<Event>) -> bool {
    if let Some(Event {
        data:
            EventData::Scalar {
                value,
                plain_implicit,
                tag,
                ..
            },
        ..
    }) = event
    {
        if let Some(t) = tag {
            return t == crate::TAG_MERGE;
        }
        return *plain_implicit && value == "<<";
    }
    false
}

/// Construct a Python bool from a scalar value without allocation
fn construct_bool_direct(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    let bool_val = match value {
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => true,
        "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => false,
        _ => {
            return Err(exception::constructor_error(
                py,
                format!("invalid boolean value: {}", value),
            ));
        }
    };
    Ok(PyBool::new(py, bool_val).as_any().clone().unbind())
}

/// Construct a Python int from a scalar value
fn construct_int_direct(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    // Fast path: standard decimal parse (covers 90%+ of real-world ints)
    if let Ok(v) = value.parse::<i64>() {
        return Ok(PyInt::new(py, v).into_any().unbind());
    }
    construct_int_fallback(py, value)
}

fn construct_int_fallback(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(exception::constructor_error(
            py,
            "invalid integer: empty value".to_string(),
        ));
    }

    let (sign, remaining) = match bytes[0] {
        b'-' => (-1i64, &value[1..]),
        b'+' => (1i64, &value[1..]),
        _ => (1i64, value),
    };

    let result = if remaining == "0" {
        0i64
    } else if let Some(bin) = remaining.strip_prefix("0b") {
        parse_int_skip_underscores(bin, 2).map_err(|_| {
            exception::constructor_error(py, format!("invalid binary integer: {}", value))
        })?
    } else if let Some(hex) = remaining.strip_prefix("0x") {
        parse_int_skip_underscores(hex, 16).map_err(|_| {
            exception::constructor_error(py, format!("invalid hex integer: {}", value))
        })?
    } else if remaining.starts_with('0') && !remaining.contains(':') && remaining.len() > 1 {
        parse_int_skip_underscores(remaining, 8).map_err(|_| {
            exception::constructor_error(py, format!("invalid octal integer: {}", value))
        })?
    } else if remaining.contains(':') {
        parse_sexagesimal_int(remaining).map_err(|_| {
            exception::constructor_error(py, format!("invalid sexagesimal integer: {}", value))
        })?
    } else {
        parse_int_skip_underscores(remaining, 10)
            .map_err(|_| exception::constructor_error(py, format!("invalid integer: {}", value)))?
    };

    Ok(PyInt::new(py, sign * result).into_any().unbind())
}

/// Construct a Python float from a scalar value
fn construct_float_direct(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    // Fast path: standard f64 parse
    if let Ok(v) = value.parse::<f64>() {
        return Ok(PyFloat::new(py, v).into_any().unbind());
    }
    construct_float_fallback(py, value)
}

fn construct_float_fallback(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(exception::constructor_error(
            py,
            "invalid float: empty value".to_string(),
        ));
    }

    let (sign, remaining) = match bytes[0] {
        b'-' => (-1.0f64, &value[1..]),
        b'+' => (1.0f64, &value[1..]),
        _ => (1.0f64, value),
    };

    if remaining.eq_ignore_ascii_case(".inf") {
        return Ok(PyFloat::new(py, sign * f64::INFINITY).into_any().unbind());
    }
    if remaining.eq_ignore_ascii_case(".nan") {
        return Ok(PyFloat::new(py, f64::NAN).into_any().unbind());
    }

    let result = if remaining.contains(':') {
        parse_sexagesimal_float(remaining).map_err(|_| {
            exception::constructor_error(py, format!("invalid sexagesimal float: {}", value))
        })?
    } else {
        parse_float_skip_underscores(remaining)
            .map_err(|_| exception::constructor_error(py, format!("invalid float: {}", value)))?
    };

    Ok(PyFloat::new(py, sign * result).into_any().unbind())
}

// --- Zero-allocation parsing helpers ---

/// Parse integer string in given radix, skipping '_' characters, without heap allocation.
fn parse_int_skip_underscores(s: &str, radix: u32) -> Result<i64, ()> {
    let mut result: i64 = 0;
    let mut has_digit = false;
    for b in s.bytes() {
        if b == b'_' {
            continue;
        }
        has_digit = true;
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as u32,
            b'a'..=b'f' => (b - b'a' + 10) as u32,
            b'A'..=b'F' => (b - b'A' + 10) as u32,
            _ => return Err(()),
        };
        if digit >= radix {
            return Err(());
        }
        result = result
            .checked_mul(radix as i64)
            .ok_or(())?
            .checked_add(digit as i64)
            .ok_or(())?;
    }
    if has_digit { Ok(result) } else { Err(()) }
}

/// Parse sexagesimal integer (e.g. "1:30" = 90), skipping underscores in each segment.
fn parse_sexagesimal_int(s: &str) -> Result<i64, ()> {
    let mut result: i64 = 0;
    for part in s.split(':') {
        let segment = parse_int_skip_underscores(part, 10)?;
        result = result
            .checked_mul(60)
            .ok_or(())?
            .checked_add(segment)
            .ok_or(())?;
    }
    Ok(result)
}

/// Parse float string, skipping '_' characters, without heap allocation for typical values.
fn parse_float_skip_underscores(s: &str) -> Result<f64, ()> {
    // Fast path: no underscores
    if !s.contains('_') {
        return s.parse::<f64>().map_err(|_| ());
    }
    // Stack buffer for typical float strings
    let mut buf = [0u8; 64];
    let mut len = 0;
    for b in s.bytes() {
        if b == b'_' {
            continue;
        }
        if len >= buf.len() {
            // Fallback to heap for very long strings
            let cleaned: String = s.chars().filter(|&c| c != '_').collect();
            return cleaned.parse::<f64>().map_err(|_| ());
        }
        buf[len] = b;
        len += 1;
    }
    let cleaned = std::str::from_utf8(&buf[..len]).map_err(|_| ())?;
    cleaned.parse::<f64>().map_err(|_| ())
}

/// Parse sexagesimal float (e.g. "1:30.5" = 90.5), skipping underscores in each segment.
fn parse_sexagesimal_float(s: &str) -> Result<f64, ()> {
    let mut result: f64 = 0.0;
    for part in s.split(':') {
        let segment = parse_float_skip_underscores(part)?;
        result = result * 60.0 + segment;
    }
    Ok(result)
}

pub fn register_loader(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RSafeLoader>()?;
    Ok(())
}
