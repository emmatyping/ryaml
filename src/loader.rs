//! Module implement pyyaml compatibility layer for ryaml via libyaml
//! Implements RLoader, which can load YAML 1.1

use base64::Engine as _;
use libyaml_safer::{Event, EventData, Parser};
use pyo3::exceptions::PyNotImplementedError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyBytes, PyDate, PyDateTime, PyDict, PyFloat, PyInt, PyList, PySet, PyString, PyTuple,
    PyTzInfo,
};
use rustc_hash::FxBuildHasher;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use crate::exception;
use crate::resolver;

/// Only mappings with merges need a second construction step. Ordinary mappings
/// are constructed directly without allocating a node graph.
struct PendingMapping {
    dict: Py<PyDict>,
    sources: Vec<Py<PyAny>>,
    set: Option<Py<PySet>>,
}

#[pyclass(name = "_RSafeLoader", subclass)]
pub struct RSafeLoader {
    /// Parser over an in-memory string passed by Python
    parser: Parser<Cursor<String>>,
    /// Event used by internal parser
    parsed_event: Option<Event>,
    /// Anchors mapping anchor name to constructed Python object
    anchors: HashMap<String, Py<PyAny>, FxBuildHasher>,
    /// Anchors placed on a `<<` merge key. PyYAML keeps these as `merge`-tagged
    /// nodes: an alias to one acts as a merge key in key position and is a
    /// ConstructorError anywhere else.
    merge_anchors: HashSet<String, FxBuildHasher>,
    pending_mappings: Vec<PendingMapping>,
    // Sets are represented by mapping nodes in YAML and can be merge sources.
    set_mappings: HashMap<usize, (Py<PySet>, Py<PyDict>), FxBuildHasher>,
    anchored_mapping_lengths: HashMap<usize, usize, FxBuildHasher>,
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
            merge_anchors: HashSet::with_hasher(FxBuildHasher),
            pending_mappings: Vec::new(),
            set_mappings: HashMap::with_hasher(FxBuildHasher),
            anchored_mapping_lengths: HashMap::with_hasher(FxBuildHasher),
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

    /// Register an anchor, rejecting duplicates (pyyaml ComposerError behavior).
    fn register_anchor(&mut self, py: Python, name: String, value: Py<PyAny>) -> PyResult<()> {
        self.check_new_anchor(py, &name)?;
        self.anchors.insert(name, value);
        Ok(())
    }

    fn register_merge_anchor(&mut self, py: Python, name: String) -> PyResult<()> {
        self.check_new_anchor(py, &name)?;
        self.merge_anchors.insert(name);
        Ok(())
    }

    fn check_new_anchor(&self, py: Python, name: &str) -> PyResult<()> {
        if self.anchors.contains_key(name) || self.merge_anchors.contains(name) {
            return Err(exception::composer_error(
                py,
                format!("found duplicate anchor '{}'", name),
            ));
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

        // Resolve merges after all explicit entries, including ancestor entries,
        // exist. Preserve the objects already referenced by aliases.
        self.finish_mappings(py)?;

        // Clear anchors for next document
        self.anchors.clear();
        self.merge_anchors.clear();
        self.set_mappings.clear();
        self.anchored_mapping_lengths.clear();

        Ok(Some(result))
    }

    /// Core single-pass constructor: consume the current event and produce a Python object
    fn construct_from_events(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        let event = self.parsed_event.take().unwrap();
        match event.data {
            EventData::Alias { anchor } => {
                if let Some(obj) = self.anchors.get(&anchor) {
                    Ok(obj.clone_ref(py))
                } else if self.merge_anchors.contains(&anchor) {
                    Err(merge_node_error(py))
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
        // "!" is the non-specific tag — resolve as if untagged
        let resolved_tag: &str = if let Some(ref t) = tag {
            if t == "!" {
                resolver::resolve_scalar_tag(&value, plain_implicit)
            } else {
                t.as_str()
            }
        } else {
            resolver::resolve_scalar_tag(&value, plain_implicit)
        };

        let result = match resolved_tag {
            crate::TAG_NULL => py.None(),
            crate::TAG_BOOL => construct_bool_direct(py, &value)?,
            crate::TAG_INT => construct_int_direct(py, &value)?,
            crate::TAG_FLOAT => construct_float_direct(py, &value)?,
            crate::TAG_TIMESTAMP => {
                construct_timestamp(py, &value, tag.as_deref().is_none_or(|tag| tag == "!"))?
            }
            crate::TAG_BINARY => construct_binary(py, &value)?,
            crate::TAG_STR => PyString::new(py, &value).into_any().unbind(),
            _ => {
                return Err(exception::constructor_error(
                    py,
                    format!(
                        "could not determine a constructor for the tag '{}'",
                        resolved_tag
                    ),
                ));
            }
        };

        if let Some(anchor_name) = anchor {
            self.register_anchor(py, anchor_name, result.clone_ref(py))?;
        }

        Ok(result)
    }

    /// Construct a Python list directly from sequence events
    fn construct_sequence_direct(
        &mut self,
        py: Python,
        anchor: Option<String>,
        tag: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        // Reject unknown tags (pyyaml SafeConstructor behavior)
        if let Some(ref t) = tag
            && !matches!(
                t.as_str(),
                "!" | crate::TAG_SEQ | crate::TAG_OMAP | crate::TAG_PAIRS
            )
        {
            return Err(exception::constructor_error(
                py,
                format!("could not determine a constructor for the tag '{}'", t),
            ));
        }
        let pairs = matches!(tag.as_deref(), Some(crate::TAG_OMAP | crate::TAG_PAIRS));
        let list = PyList::empty(py);
        let list_obj: Py<PyAny> = list.clone().unbind().into_any();

        // Store in anchors BEFORE recursing (handles circular references)
        if let Some(anchor_name) = anchor {
            self.register_anchor(py, anchor_name, list_obj.clone_ref(py))?;
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
            let item = if pairs {
                self.construct_pair_entry(py)?
            } else {
                self.construct_from_events(py)?
            };
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
        let record_length = anchor.is_some();
        let mut length = 0;
        let is_set = tag.as_deref() == Some(crate::TAG_SET);
        // Reject unknown tags (pyyaml SafeConstructor behavior)
        if let Some(ref t) = tag
            && t != "!"
            && t != crate::TAG_MAP
            && t != crate::TAG_SET
        {
            return Err(exception::constructor_error(
                py,
                format!("could not determine a constructor for the tag '{}'", t),
            ));
        }

        let dict = PyDict::new(py);
        let set = if is_set {
            Some(PySet::empty(py)?)
        } else {
            None
        };
        let dict_obj: Py<PyAny> = match &set {
            Some(set) => set.clone().into_any().unbind(),
            None => dict.clone().into_any().unbind(),
        };

        // Store in anchors BEFORE recursing (handles circular references)
        if let Some(anchor_name) = anchor {
            self.register_anchor(py, anchor_name, dict_obj.clone_ref(py))?;
        }

        if let Some(set) = &set {
            self.set_mappings.insert(
                dict_obj.as_ptr() as usize,
                (set.clone().unbind(), dict.clone().unbind()),
            );
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

            length += 1;
            // Check if the key is a merge key BEFORE constructing it
            let is_merge = is_merge_key(&self.parsed_event, &self.merge_anchors);

            if is_merge {
                // Consume the merge key event without constructing, but keep
                // its anchor so duplicates and later aliases behave like PyYAML.
                if let Some(Event {
                    data:
                        EventData::Scalar {
                            anchor: Some(anchor),
                            ..
                        },
                    ..
                }) = self.parsed_event.take()
                {
                    self.register_merge_anchor(py, anchor)?;
                }

                // Parse the value
                self._parse_next_event(py)?;
                let mut value = self.construct_from_events(py)?;
                if let Some((_, mapping)) = self.set_mappings.get(&(value.as_ptr() as usize)) {
                    value = mapping.clone_ref(py).into_any();
                }

                // Collect merge source(s) — must be a mapping or list of mappings
                if value.downcast_bound::<PyDict>(py).is_ok() {
                    merge_sources.push(value);
                } else if let Ok(value_list) = value.downcast_bound::<PyList>(py) {
                    // Flatten each sequence in reverse order so its first
                    // mapping wins, while later << entries still win overall.
                    for item in value_list.iter().rev() {
                        let item = if let Some((_, mapping)) =
                            self.set_mappings.get(&(item.as_ptr() as usize))
                        {
                            mapping.bind(py).clone().into_any()
                        } else {
                            item
                        };
                        if !item.is_instance_of::<PyDict>() {
                            return Err(exception::constructor_error(
                                py,
                                "expected a mapping for merging, but found non-mapping".to_string(),
                            ));
                        }
                        merge_sources.push(item.unbind());
                    }
                } else {
                    return Err(exception::constructor_error(
                        py,
                        "expected a mapping or list of mappings for merging, but found non-mapping"
                            .to_string(),
                    ));
                }
                continue;
            }

            // Rewrite value tag to str (pyyaml's flatten_mapping behavior)
            if let Some(Event {
                data:
                    EventData::Scalar {
                        ref mut tag,
                        ref value,
                        plain_implicit,
                        ..
                    },
                ..
            }) = self.parsed_event
                && (tag.as_deref() == Some(crate::TAG_VALUE)
                    || (matches!(tag.as_deref(), None | Some("!"))
                        && plain_implicit
                        && value == "="))
            {
                *tag = Some(crate::TAG_STR.to_string());
            }

            let key = self.construct_from_events(py)?;

            // Parse the value
            self._parse_next_event(py)?;
            let value = self.construct_from_events(py)?;

            // Reject unhashable keys (dicts, lists) like pyyaml
            if key.bind(py).hash().is_err() {
                return Err(exception::constructor_error(
                    py,
                    "found unhashable key".to_string(),
                ));
            }
            dict.set_item(&key, value)?;
        }

        if record_length {
            self.anchored_mapping_lengths
                .insert(dict_obj.as_ptr() as usize, length);
        }
        if !merge_sources.is_empty() {
            self.pending_mappings.push(PendingMapping {
                dict: dict.clone().unbind(),
                sources: merge_sources,
                set: set.map(Bound::unbind),
            });
        } else if let Some(set) = set {
            for key in dict.keys().iter() {
                set.add(key)?;
            }
        }

        self.parsed_event = None;
        Ok(dict_obj)
    }

    /// !!omap / !!pairs entries are raw single-entry mappings, not constructed
    /// dicts: their keys may be unhashable, and duplicate entries must be rejected
    /// before a dict could collapse them.
    fn construct_pair_entry(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        let event = self.parsed_event.take().unwrap();
        let anchor = match event.data {
            EventData::Alias { anchor } => {
                if self.merge_anchors.contains(&anchor) {
                    return Err(exception::constructor_error(
                        py,
                        "expected a mapping of length 1".to_string(),
                    ));
                }
                let value = self.anchors.get(&anchor).ok_or_else(|| {
                    exception::composer_error(py, format!("found undefined alias '{}'", anchor))
                })?;
                if self
                    .anchored_mapping_lengths
                    .get(&(value.as_ptr() as usize))
                    .is_some_and(|&n| n != 1)
                {
                    return Err(exception::constructor_error(
                        py,
                        "expected a single mapping item".to_string(),
                    ));
                }
                let value =
                    if let Some((_, mapping)) = self.set_mappings.get(&(value.as_ptr() as usize)) {
                        mapping.bind(py).clone().into_any()
                    } else {
                        value.bind(py).clone()
                    };
                if let Ok(mapping) = value.cast::<PyDict>()
                    && mapping.len() == 1
                {
                    let (key, value) = mapping.iter().next().unwrap();
                    return Ok(PyTuple::new(py, [key, value])?.into_any().unbind());
                }
                return Err(exception::constructor_error(
                    py,
                    "expected a single mapping item".to_string(),
                ));
            }
            EventData::MappingStart { anchor, .. } => anchor,
            _ => {
                return Err(exception::constructor_error(
                    py,
                    "expected a mapping of length 1".to_string(),
                ));
            }
        };
        let wrapper = if let Some(anchor) = anchor {
            let mapping = PyDict::new(py);
            self.register_anchor(py, anchor, mapping.clone().into_any().unbind())?;
            Some(mapping)
        } else {
            None
        };
        self._parse_next_event(py)?;
        if matches!(
            self.parsed_event.as_ref().map(|e| &e.data),
            Some(EventData::MappingEnd)
        ) {
            return Err(exception::constructor_error(
                py,
                "expected a single mapping item".to_string(),
            ));
        }
        let key = self.construct_from_events(py)?;
        self._parse_next_event(py)?;
        let value = self.construct_from_events(py)?;
        self._parse_next_event(py)?;
        if !matches!(
            self.parsed_event.as_ref().map(|e| &e.data),
            Some(EventData::MappingEnd)
        ) {
            return Err(exception::constructor_error(
                py,
                "expected a single mapping item".to_string(),
            ));
        }
        self.parsed_event = None;
        if let Some(mapping) = wrapper {
            // The wrapper is a mapping node if referenced elsewhere by an alias.
            mapping.set_item(&key, &value)?;
        }
        Ok(PyTuple::new(py, [key, value])?.into_any().unbind())
    }

    fn finish_mappings(&mut self, py: Python) -> PyResult<()> {
        if self.pending_mappings.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_mappings);
        let indices: HashMap<usize, usize, FxBuildHasher> = pending
            .iter()
            .enumerate()
            .map(|(i, mapping)| (mapping.dict.as_ptr() as usize, i))
            .collect();
        let mut visited = vec![false; pending.len()];
        // Parents are appended after their children. Flatten parents first, as
        // PyYAML does, and break recursive merge edges at the explicit entries.
        for i in (0..pending.len()).rev() {
            finish_mapping(py, i, &pending, &indices, &mut visited)?;
        }
        Ok(())
    }
}

/// Rebuild only mappings which contain merges, preserving insertion order and
/// alias identity. Explicit entries override all merged entries.
fn finish_mapping(
    py: Python,
    index: usize,
    pending: &[PendingMapping],
    indices: &HashMap<usize, usize, FxBuildHasher>,
    visited: &mut [bool],
) -> PyResult<()> {
    if visited[index] {
        return Ok(());
    }
    visited[index] = true;
    let mapping = &pending[index];
    let mut merged: Option<Bound<'_, PyDict>> = None;
    for source in &mapping.sources {
        if let Some(&source_index) = indices.get(&(source.as_ptr() as usize)) {
            finish_mapping(py, source_index, pending, indices, visited)?;
        }
        let source = source.bind(py).cast::<PyDict>()?;
        if let Some(merged) = &merged {
            merged.update(source.as_mapping())?;
        } else {
            merged = Some(source.copy()?);
        }
    }
    let merged = merged.expect("pending mappings have at least one merge source");
    let dict = mapping.dict.bind(py);
    merged.update(dict.as_mapping())?;
    dict.clear();
    dict.update(merged.as_mapping())?;
    if let Some(set) = &mapping.set {
        for key in dict.keys().iter() {
            set.bind(py).add(key)?;
        }
    }
    Ok(())
}

static ISO_DATETIME: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

fn construct_timestamp(py: Python, value: &str, implicit: bool) -> PyResult<Py<PyAny>> {
    let b = value.as_bytes();
    // Common date-only form: avoid regex/group parsing and Python helper calls.
    if b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && (implicit
            || b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()))
    {
        let pair = |i: usize| (b[i] - b'0') * 10 + b[i + 1] - b'0';
        let year = i32::from(pair(0)) * 100 + i32::from(pair(2));
        let month = pair(5);
        let day = pair(8);
        return Ok(PyDate::new(py, year, month, day)?.into_any().unbind());
    }
    // datetime's C implementation handles the common fixed-width ISO form.
    // Check YAML's grammar first: fromisoformat also accepts non-YAML forms.
    if b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[13] == b':'
        && b[16] == b':'
        && (implicit || resolver::is_timestamp(value))
    {
        if let Some(result) = construct_fixed_datetime(py, value)? {
            return Ok(result);
        }
        let constructor = ISO_DATETIME.get_or_try_init(py, || -> PyResult<_> {
            Ok(py
                .import("datetime")?
                .getattr("datetime")?
                .getattr("fromisoformat")?
                .unbind())
        })?;
        match constructor.bind(py).call1((value,)) {
            Ok(result) => return Ok(result.unbind()),
            // YAML also permits an empty fractional part, e.g. ...:00.Z.
            Err(error) if error.is_instance_of::<pyo3::exceptions::PyValueError>(py) => {}
            Err(error) => return Err(error),
        }
    }
    construct_python_scalar(py, &TIMESTAMP, "timestamp", value)
}

/// Construct fixed-width naive/UTC datetimes without a temporary Python string.
/// The caller has already checked YAML's timestamp grammar and field positions.
fn construct_fixed_datetime(py: Python, value: &str) -> PyResult<Option<Py<PyAny>>> {
    let b = value.as_bytes();
    // The grammar check already validated all digits. These positions distinguish
    // two-digit days/hours with one separator from single-digit fields or extra
    // whitespace that happen to put the colons at the same offsets.
    if !b[9].is_ascii_digit() || !b[11].is_ascii_digit() || !b[12].is_ascii_digit() {
        return Ok(None);
    }
    let pair = |i: usize| (b[i] - b'0') * 10 + b[i + 1] - b'0';
    let mut end = 19;
    let mut microsecond = 0;
    if b.get(end) == Some(&b'.') {
        end += 1;
        let start = end;
        while end < b.len() && b[end].is_ascii_digit() {
            if end - start < 6 {
                microsecond = microsecond * 10 + u32::from(b[end] - b'0');
            }
            end += 1;
        }
        for _ in (end - start)..6 {
            microsecond *= 10;
        }
    }
    let zone = match &value[end..] {
        "" => None,
        "Z" => Some(PyTzInfo::utc(py)?),
        // Offset zones and whitespace before the zone use the general path.
        _ => return Ok(None),
    };
    let year = i32::from(pair(0)) * 100 + i32::from(pair(2));
    Ok(Some(
        PyDateTime::new(
            py,
            year,
            pair(5),
            pair(8),
            pair(11),
            pair(14),
            pair(17),
            microsecond,
            zone.as_deref(),
        )?
        .into_any()
        .unbind(),
    ))
}

static BASE64_DECODER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

fn construct_binary(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    // Canonical base64 is decoded in Rust. Python's decoder handles the wider
    // set of non-canonical inputs accepted by PyYAML (whitespace, extra chars,
    // non-zero padding bits), and provides its error semantics.
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(value) {
        return Ok(PyBytes::new(py, &decoded).into_any().unbind());
    }
    if !value.is_ascii() {
        return construct_python_scalar(py, &BINARY, "binary", value);
    }
    let decoder = BASE64_DECODER.get_or_try_init(py, || -> PyResult<_> {
        Ok(py.import("binascii")?.getattr("a2b_base64")?.unbind())
    })?;
    match decoder.bind(py).call1((value,)) {
        Ok(result) => Ok(result.unbind()),
        Err(error) if error.is_instance_of::<pyo3::exceptions::PyValueError>(py) => Err(
            exception::constructor_error(py, format!("failed to decode base64 data: {}", error)),
        ),
        Err(error) => Err(error),
    }
}

static TIMESTAMP: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static BINARY: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

fn construct_python_scalar(
    py: Python,
    cache: &PyOnceLock<Py<PyAny>>,
    name: &str,
    value: &str,
) -> PyResult<Py<PyAny>> {
    let constructor = cache.get_or_try_init(py, || -> PyResult<_> {
        Ok(py.import("ryaml._scalars")?.getattr(name)?.unbind())
    })?;
    Ok(constructor.bind(py).call1((value,))?.unbind())
}

fn merge_node_error(py: Python) -> PyErr {
    exception::constructor_error(
        py,
        format!(
            "could not determine a constructor for the tag '{}'",
            crate::TAG_MERGE
        ),
    )
}

/// Check if the current event is a merge key: plain scalar "<<", an explicit
/// merge tag, or an alias to an anchored merge key.
fn is_merge_key(event: &Option<Event>, merge_anchors: &HashSet<String, FxBuildHasher>) -> bool {
    if let Some(Event {
        data: EventData::Alias { anchor },
        ..
    }) = event
    {
        return merge_anchors.contains(anchor);
    }
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
            // Explicit merge tag, or non-specific "!" on plain "<<"
            return t == crate::TAG_MERGE || (t == "!" && *plain_implicit && value == "<<");
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
        _ if ["yes", "true", "on"]
            .iter()
            .any(|s| value.eq_ignore_ascii_case(s)) =>
        {
            true
        }
        _ if ["no", "false", "off"]
            .iter()
            .any(|s| value.eq_ignore_ascii_case(s)) =>
        {
            false
        }
        _ => return Err(pyo3::exceptions::PyKeyError::new_err(value.to_lowercase())),
    };
    Ok(PyBool::new(py, bool_val).as_any().clone().unbind())
}

/// Construct a Python int from a scalar value
fn construct_int_direct(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    // Fast path: standard decimal parse (covers 90%+ of real-world ints)
    // Skip for values with leading zero after optional sign — those are octal/hex/binary
    // in YAML 1.1 and must go through the fallback path.
    let digits = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    if !(digits.starts_with('0') && digits.len() > 1)
        && let Ok(v) = value.parse::<i64>()
    {
        return Ok(PyInt::new(py, v).into_any().unbind());
    }
    construct_int_fallback(py, value)
}

/// Mirrors PyYAML's `construct_yaml_int`: strip underscores, strip one sign,
/// then dispatch on prefix in the same order (a leading `0` means octal even if
/// the value contains `:`), with Python's `int()` as the fallback.
fn construct_int_fallback(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    let cleaned;
    let value = if value.contains('_') {
        cleaned = value.replace('_', "");
        cleaned.as_str()
    } else {
        value
    };
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
        match parse_int_skip_underscores(bin, 2) {
            Ok(v) => v,
            Err(()) => return construct_bigint_with_base(py, sign, bin, 2),
        }
    } else if let Some(hex) = remaining.strip_prefix("0x") {
        match parse_int_skip_underscores(hex, 16) {
            Ok(v) => v,
            Err(()) => return construct_bigint_with_base(py, sign, hex, 16),
        }
    } else if remaining.starts_with('0') {
        match parse_int_skip_underscores(remaining, 8) {
            Ok(v) => v,
            Err(()) => return construct_bigint_with_base(py, sign, remaining, 8),
        }
    } else if remaining.contains(':') {
        match parse_sexagesimal_int(remaining) {
            Ok(v) => v,
            Err(()) => return construct_sexagesimal_bigint(py, sign, remaining),
        }
    } else {
        match parse_int_skip_underscores(remaining, 10) {
            Ok(v) => v,
            // Overflow or parse failure: PyYAML calls int() on the unsigned
            // remainder, which also accepts e.g. surrounding whitespace.
            Err(()) => return construct_bigint_with_base(py, sign, remaining, 10),
        }
    };

    Ok(PyInt::new(py, sign * result).into_any().unbind())
}

/// Construct a Python big integer from digits in a given base (for octal/hex/binary overflow).
fn construct_bigint_with_base(
    py: Python,
    sign: i64,
    digits: &str,
    base: u32,
) -> PyResult<Py<PyAny>> {
    let cleaned: String = digits.chars().filter(|&c| c != '_').collect();
    let builtins = py.import("builtins")?;
    let py_int = builtins.call_method1("int", (&cleaned, base))?;
    if sign < 0 {
        let neg = py_int.call_method1("__mul__", (-1i64,))?;
        Ok(neg.unbind())
    } else {
        Ok(py_int.unbind())
    }
}

/// Construct a Python big integer from sexagesimal notation (e.g. "1:30" = 90)
/// using Python's arbitrary-precision arithmetic when i64 overflows.
fn construct_sexagesimal_bigint(py: Python, sign: i64, s: &str) -> PyResult<Py<PyAny>> {
    let builtins = py.import("builtins")?;
    let sixty = PyInt::new(py, 60i64);
    let mut result = PyInt::new(py, 0i64).into_any();
    for part in s.split(':') {
        let cleaned: String = part.chars().filter(|&c| c != '_').collect();
        let segment = builtins.call_method1("int", (&cleaned,))?;
        result = result
            .call_method1("__mul__", (&sixty,))?
            .call_method1("__add__", (&segment,))?;
    }
    if sign < 0 {
        result = result.call_method1("__mul__", (-1i64,))?;
    }
    Ok(result.unbind())
}

/// Construct a Python float from a scalar value
fn construct_float_direct(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    // Fast path: standard f64 parse
    if let Ok(v) = value.parse::<f64>() {
        return Ok(PyFloat::new(py, v).into_any().unbind());
    }
    construct_float_fallback(py, value)
}

static NAN: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

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
        // PyYAML returns one shared NaN object, so repeated `.nan` mapping keys
        // collapse via dict's identity check; mirror that.
        let nan = NAN.get_or_init(py, || PyFloat::new(py, f64::NAN).into_any().unbind());
        return Ok(nan.clone_ref(py));
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
