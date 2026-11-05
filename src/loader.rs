//! PyYAML-compatible loading for ryaml (YAML 1.1).
//!
//! Like PyYAML, each document is loaded in two phases. The composer turns
//! parser events into a node graph (resolving tags, registering anchors and
//! raising composer errors), then the constructor builds Python objects from it
//! in PyYAML's order: scalars at once, collections created empty and filled
//! later from a queue (PyYAML's generator-based `state_generators`). Matching
//! both phases keeps results and errors identical to PyYAML's SafeLoader.

use std::collections::{HashMap, VecDeque};

use base64::Engine as _;
use libyaml_safer::{ErrorKind, Event, EventData, INVALID_UTF8_ESCAPE, Parser};
use pyo3::exceptions::{PyIndexError, PyNotImplementedError, PyRecursionError, PyTypeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyBytes, PyDate, PyDateTime, PyDict, PyFloat, PyInt, PyList, PySet, PyString, PyTuple,
    PyTzInfo,
};
use rustc_hash::FxBuildHasher;

use crate::exception;
use crate::input::{Input, Source};
use crate::resolver::{self, Tag};

type NodeId = usize;

/// A composed node, as in PyYAML's `yaml.nodes`.
struct Node {
    /// Resolved tag. Standard tags are variants, so most nodes do not allocate.
    tag: Tag,
    /// Anchored, so aliases can reach it more than once.
    anchored: bool,
    data: NodeData,
}

enum NodeData {
    Scalar {
        /// The value's text: this range of `RSafeLoader::text`.
        value: std::ops::Range<usize>,
        /// The tag was resolved implicitly (no tag, or `!`) from the whole
        /// value, so it matches the resolver's grammar exactly. ryaml's
        /// timestamp constructor uses this to take a fast path. False for a
        /// value ending in a newline, which PyYAML's regexes also accept.
        implicit: bool,
    },
    Sequence(Vec<NodeId>),
    /// Key/value pairs in document order. `flatten_mapping` rewrites them.
    Mapping(Vec<(NodeId, NodeId)>),
}

impl NodeData {
    /// PyYAML's `node.id`, used in error messages.
    fn kind(&self) -> &'static str {
        match self {
            NodeData::Scalar { .. } => "scalar",
            NodeData::Sequence(_) => "sequence",
            NodeData::Mapping(_) => "mapping",
        }
    }
}

#[pyclass(name = "_RSafeLoader", subclass)]
pub struct RSafeLoader {
    parser: Parser<Input>,
    /// The next event, if it has been parsed but not used yet.
    parsed_event: Option<Event>,
    /// Nodes of the document being loaded.
    nodes: Vec<Node>,
    /// The text of the document's scalars, one after another. Copying values
    /// here lets the parser reuse each value's string for the next scalar.
    text: String,
    /// Composer anchors: name to node.
    anchors: HashMap<String, NodeId, FxBuildHasher>,
    /// Constructed object of each node (PyYAML's `constructed_objects`). Every
    /// node needs one: besides aliases, merges copy entries that point to
    /// unanchored nodes, including the mapping being filled.
    objects: Vec<Option<Py<PyAny>>>,
    /// The document has a `<<` key. Without one, only anchored nodes can be
    /// reached twice, so other objects need not be remembered.
    has_merge: bool,
    /// Collections created empty and still to be filled, in PyYAML's order.
    pending: VecDeque<(NodeId, Py<PyAny>)>,
}

#[pymethods]
impl RSafeLoader {
    #[new]
    pub fn new(source: Source) -> Self {
        Self::from_input(source.into_input())
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
            let root = self.compose_document(py)?;
            return self.construct_document(py, root).map(Some);
        }
        Ok(None)
    }

    /// Get a single document as a Python object
    pub fn get_single_data(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        // PyYAML's get_single_node: drop STREAM-START, compose a document if
        // there is one, and check the stream ends before constructing anything.
        self.parse_next_event(py)?;
        self.parsed_event = None;
        self.parse_next_event(py)?;
        let root = if self.at(|data| matches!(data, EventData::StreamEnd)) {
            None
        } else {
            Some(self.compose_document(py)?)
        };
        self.parse_next_event(py)?;
        if !self.at(|data| matches!(data, EventData::StreamEnd)) {
            return Err(single_document_error(py));
        }
        match root {
            Some(root) => self.construct_document(py, root).map(Some),
            None => Ok(None),
        }
    }

    pub fn dispose(&self) {}
}

/// Parsing and composing (PyYAML's `Composer`).
impl RSafeLoader {
    pub fn from_input(input: Input) -> Self {
        let mut parser = Parser::new();
        parser.set_input(input);
        Self {
            parser,
            parsed_event: None,
            nodes: Vec::new(),
            text: String::new(),
            anchors: HashMap::with_hasher(FxBuildHasher),
            objects: Vec::new(),
            has_merge: false,
            pending: VecDeque::new(),
        }
    }

    fn parse_next_event(&mut self, py: Python) -> PyResult<()> {
        if self.parsed_event.is_none() {
            match self.parser.parse() {
                Ok(event) => self.parsed_event = Some(event),
                Err(e) => return Err(parse_error(py, &e)),
            }
        }
        Ok(())
    }

    fn at(&self, test: impl Fn(&EventData) -> bool) -> bool {
        self.parsed_event.as_ref().is_some_and(|e| test(&e.data))
    }

    fn check_node(&mut self, py: Python) -> PyResult<bool> {
        self.parse_next_event(py)?;
        if self.at(|data| matches!(data, EventData::StreamStart { .. })) {
            self.parsed_event = None;
            self.parse_next_event(py)?;
        }
        Ok(!self.at(|data| matches!(data, EventData::StreamEnd)))
    }

    /// Compose the document at the current DOCUMENT-START event.
    fn compose_document(&mut self, py: Python) -> PyResult<NodeId> {
        self.parsed_event = None;
        self.nodes.clear();
        self.text.clear();
        self.anchors.clear();
        self.has_merge = false;
        let root = self.compose_node(py)?;
        // Drop the DOCUMENT-END event.
        self.parse_next_event(py)?;
        self.parsed_event = None;
        self.anchors.clear();
        Ok(root)
    }

    /// PyYAML's `compose_node`, with an explicit stack instead of recursion so
    /// deeply nested input cannot overflow the native stack.
    fn compose_node(&mut self, py: Python) -> PyResult<NodeId> {
        // Open collections, with a mapping's key waiting for its value.
        let mut open: Vec<(NodeId, Option<NodeId>)> = Vec::new();
        loop {
            let event = match self.parsed_event.take() {
                Some(event) => event,
                None => self.parser.parse().map_err(|e| parse_error(py, &e))?,
            };
            let complete = match event.data {
                EventData::Alias { anchor } => match self.anchors.get(&anchor) {
                    Some(&node) => Some(node),
                    None => {
                        return Err(exception::composer_error(
                            py,
                            format!("found undefined alias '{anchor}'"),
                        ));
                    }
                },
                EventData::Scalar {
                    anchor,
                    tag,
                    value,
                    plain_implicit,
                    ..
                } => {
                    self.check_anchor(py, anchor.as_deref())?;
                    check_tag_utf8(py, tag.as_deref())?;
                    let implicit = tag.as_deref().is_none_or(|t| t == "!");
                    let tag = if implicit {
                        resolver::resolve_scalar(&value, plain_implicit)
                    } else {
                        Tag::from_string(tag.expect("explicit tag"))
                    };
                    let implicit = implicit && !value.ends_with('\n');
                    let start = self.text.len();
                    self.text.push_str(&value);
                    self.parser.recycle(value);
                    let value = start..self.text.len();
                    let node = self.add_node(anchor, tag, NodeData::Scalar { value, implicit });
                    Some(node)
                }
                EventData::SequenceStart { anchor, tag, .. } => {
                    self.check_anchor(py, anchor.as_deref())?;
                    check_tag_utf8(py, tag.as_deref())?;
                    let tag = collection_tag(tag, Tag::Seq);
                    let node = self.add_node(anchor, tag, NodeData::Sequence(Vec::new()));
                    open.push((node, None));
                    None
                }
                EventData::MappingStart { anchor, tag, .. } => {
                    self.check_anchor(py, anchor.as_deref())?;
                    check_tag_utf8(py, tag.as_deref())?;
                    let tag = collection_tag(tag, Tag::Map);
                    let node = self.add_node(anchor, tag, NodeData::Mapping(Vec::new()));
                    open.push((node, None));
                    None
                }
                EventData::SequenceEnd | EventData::MappingEnd => {
                    Some(open.pop().expect("end event closes an open collection").0)
                }
                other => {
                    return Err(exception::composer_error(
                        py,
                        format!("unexpected event: {other:?}"),
                    ));
                }
            };
            let Some(node) = complete else { continue };
            let Some((parent, key)) = open.last_mut() else {
                return Ok(node);
            };
            match &mut self.nodes[*parent].data {
                NodeData::Sequence(items) => items.push(node),
                NodeData::Mapping(entries) => match key.take() {
                    None => *key = Some(node),
                    Some(key) => entries.push((key, node)),
                },
                NodeData::Scalar { .. } => unreachable!("scalars are never open"),
            }
        }
    }

    fn check_anchor(&self, py: Python, anchor: Option<&str>) -> PyResult<()> {
        match anchor {
            Some(anchor) if self.anchors.contains_key(anchor) => Err(exception::composer_error(
                py,
                format!("found duplicate anchor '{anchor}'"),
            )),
            _ => Ok(()),
        }
    }

    fn add_node(&mut self, anchor: Option<String>, tag: Tag, data: NodeData) -> NodeId {
        let node = self.nodes.len();
        self.has_merge |= tag == Tag::Merge;
        self.nodes.push(Node {
            tag,
            anchored: anchor.is_some(),
            data,
        });
        if let Some(anchor) = anchor {
            self.anchors.insert(anchor, node);
        }
        node
    }
}

/// Constructing (PyYAML's `SafeConstructor`).
impl RSafeLoader {
    /// PyYAML's `construct_document`: construct the root, then fill collections
    /// in the order they were created.
    fn construct_document(&mut self, py: Python, root: NodeId) -> PyResult<Py<PyAny>> {
        self.objects.clear();
        self.objects.resize_with(self.nodes.len(), || None);
        self.pending.clear();
        let result = self.construct_object(py, root).and_then(|data| {
            while let Some((node, object)) = self.pending.pop_front() {
                self.fill_collection(py, node, &object.into_bound(py))?;
            }
            Ok(data.unbind())
        });
        self.objects.clear();
        self.pending.clear();
        self.nodes.clear();
        self.text.clear();
        result
    }

    /// PyYAML's `construct_object`: each node is constructed once. Collections
    /// are created empty here and filled later (`fill_collection`).
    fn construct_object<'py>(
        &mut self,
        py: Python<'py>,
        node: NodeId,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(object) = &self.objects[node] {
            return Ok(object.bind(py).clone());
        }
        let remember = self.has_merge || self.nodes[node].anchored;
        let object = match self.nodes[node].tag {
            Tag::Seq | Tag::Omap | Tag::Pairs => PyList::empty(py).into_any(),
            Tag::Map => PyDict::new(py).into_any(),
            Tag::Set => PySet::empty(py)?.into_any(),
            _ => {
                let object = self.construct_scalar_object(py, node)?.into_bound(py);
                if remember {
                    self.objects[node] = Some(object.clone().unbind());
                }
                return Ok(object);
            }
        };
        self.pending.push_back((node, object.clone().unbind()));
        if remember {
            self.objects[node] = Some(object.clone().unbind());
        }
        Ok(object)
    }

    /// The constructors for scalar tags, and the undefined-tag error.
    fn construct_scalar_object(&mut self, py: Python, node: NodeId) -> PyResult<Py<PyAny>> {
        let tag = &self.nodes[node].tag;
        if !matches!(
            tag,
            Tag::Null | Tag::Bool | Tag::Int | Tag::Float | Tag::Binary | Tag::Timestamp | Tag::Str
        ) {
            return Err(exception::constructor_error(
                py,
                unknown_tag_message(tag.as_str()),
            ));
        }
        let value = self.construct_scalar(py, node)?;
        Ok(match self.nodes[node].tag {
            Tag::Null => py.None(),
            Tag::Bool => construct_bool_direct(py, value)?,
            Tag::Int => construct_int_direct(py, value)?,
            Tag::Float => construct_float_direct(py, value)?,
            Tag::Binary => construct_binary(py, value)?,
            Tag::Timestamp => match &self.nodes[node].data {
                NodeData::Scalar { implicit, .. } => construct_timestamp(py, value, *implicit)?,
                // PyYAML matches its timestamp regex against node.value, which
                // for a mapping is its list of entries.
                _ => {
                    return Err(PyTypeError::new_err(
                        "expected string or bytes-like object, got 'list'",
                    ));
                }
            },
            _ => PyString::new(py, value).into_any().unbind(),
        })
    }

    /// PyYAML's `SafeConstructor.construct_scalar`: a scalar's text, or for a
    /// mapping the text of its first `=` (value) key's value, recursively.
    fn construct_scalar(&self, py: Python, mut node: NodeId) -> PyResult<&str> {
        for _ in 0..=self.nodes.len() {
            match &self.nodes[node].data {
                NodeData::Scalar { value, .. } => return Ok(&self.text[value.clone()]),
                NodeData::Mapping(entries) => {
                    let value_key = entries
                        .iter()
                        .find(|(key, _)| self.nodes[*key].tag == Tag::Value);
                    match value_key {
                        Some(&(_, value)) => node = value,
                        None => return Err(expected_scalar_error(py, "mapping")),
                    }
                }
                NodeData::Sequence(_) => return Err(expected_scalar_error(py, "sequence")),
            }
        }
        // A `=` chain back to a mapping already visited: PyYAML recurses forever.
        Err(PyRecursionError::new_err(
            "maximum recursion depth exceeded",
        ))
    }

    /// The deferred part of a collection constructor (after PyYAML's `yield`).
    fn fill_collection(
        &mut self,
        py: Python,
        node: NodeId,
        object: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        match &self.nodes[node].tag {
            Tag::Seq => {
                let NodeData::Sequence(items) = &self.nodes[node].data else {
                    let found = self.nodes[node].data.kind();
                    return Err(exception::constructor_error(
                        py,
                        format!("expected a sequence node, but found {found}"),
                    ));
                };
                let list = object.cast::<PyList>()?;
                for index in 0..items.len() {
                    let NodeData::Sequence(items) = &self.nodes[node].data else {
                        unreachable!()
                    };
                    let item = self.construct_object(py, items[index])?;
                    list.append(item)?;
                }
            }
            Tag::Map => {
                let dict = object.cast::<PyDict>()?;
                self.construct_mapping(py, node, |key, value| dict.set_item(key, value))?;
            }
            Tag::Set => {
                let set = object.cast::<PySet>()?;
                self.construct_mapping(py, node, |key, _| set.add(key))?;
            }
            tag => {
                let pairs = object.cast::<PyList>()?;
                let what = if *tag == Tag::Omap {
                    "an ordered map"
                } else {
                    "pairs"
                };
                self.construct_pairs(py, node, what, pairs)?;
            }
        }
        Ok(())
    }

    /// PyYAML's `SafeConstructor.construct_mapping`: flatten merges, then
    /// construct each key and value in order.
    fn construct_mapping(
        &mut self,
        py: Python,
        node: NodeId,
        mut insert: impl FnMut(&Bound<'_, PyAny>, &Bound<'_, PyAny>) -> PyResult<()>,
    ) -> PyResult<()> {
        if !matches!(self.nodes[node].data, NodeData::Mapping(_)) {
            let found = self.nodes[node].data.kind();
            return Err(exception::constructor_error(
                py,
                format!("expected a mapping node, but found {found}"),
            ));
        }
        self.flatten_mapping(py, node)?;
        let mut index = 0;
        while let NodeData::Mapping(entries) = &self.nodes[node].data
            && let Some(&(key_node, value_node)) = entries.get(index)
        {
            index += 1;
            let key = self.construct_object(py, key_node)?;
            if key.is_instance_of::<PyList>()
                || key.is_instance_of::<PyDict>()
                || key.is_instance_of::<PySet>()
            {
                return Err(exception::constructor_error(
                    py,
                    "found unhashable key".to_string(),
                ));
            }
            let value = self.construct_object(py, value_node)?;
            insert(&key, &value)?;
        }
        Ok(())
    }

    /// PyYAML's `flatten_mapping`: replace `<<` entries with the entries of the
    /// mappings they merge (earlier merges and explicit entries win), and turn
    /// `=` keys into plain strings. Like PyYAML it rewrites the node itself.
    fn flatten_mapping(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        let mut merge: Vec<(NodeId, NodeId)> = Vec::new();
        let mut index = 0;
        loop {
            let NodeData::Mapping(entries) = &self.nodes[node].data else {
                return Ok(());
            };
            let Some(&(key_node, value_node)) = entries.get(index) else {
                break;
            };
            match self.nodes[key_node].tag {
                Tag::Merge => {
                    // Removed before recursing, which also ends merge cycles.
                    if let NodeData::Mapping(entries) = &mut self.nodes[node].data {
                        entries.remove(index);
                    }
                    match &self.nodes[value_node].data {
                        NodeData::Mapping(_) => {
                            self.flatten_mapping(py, value_node)?;
                            merge.extend(self.mapping_entries(value_node));
                        }
                        NodeData::Sequence(items) => {
                            let mut submerge = Vec::new();
                            for subnode in items.clone() {
                                if !matches!(self.nodes[subnode].data, NodeData::Mapping(_)) {
                                    let found = self.nodes[subnode].data.kind();
                                    return Err(exception::constructor_error(
                                        py,
                                        format!(
                                            "expected a mapping for merging, but found {found}"
                                        ),
                                    ));
                                }
                                self.flatten_mapping(py, subnode)?;
                                submerge.push(self.mapping_entries(subnode));
                            }
                            for entries in submerge.into_iter().rev() {
                                merge.extend(entries);
                            }
                        }
                        NodeData::Scalar { .. } => {
                            return Err(exception::constructor_error(
                                py,
                                "expected a mapping or list of mappings for merging, but found scalar"
                                    .to_string(),
                            ));
                        }
                    }
                }
                Tag::Value => {
                    self.nodes[key_node].tag = Tag::Str;
                    index += 1;
                }
                _ => index += 1,
            }
        }
        if !merge.is_empty()
            && let NodeData::Mapping(entries) = &mut self.nodes[node].data
        {
            merge.append(entries);
            *entries = merge;
        }
        Ok(())
    }

    fn mapping_entries(&self, node: NodeId) -> Vec<(NodeId, NodeId)> {
        match &self.nodes[node].data {
            NodeData::Mapping(entries) => entries.clone(),
            _ => Vec::new(),
        }
    }

    /// PyYAML's `construct_yaml_omap` / `construct_yaml_pairs`.
    fn construct_pairs(
        &mut self,
        py: Python,
        node: NodeId,
        what: &str,
        pairs: &Bound<'_, PyList>,
    ) -> PyResult<()> {
        let NodeData::Sequence(items) = &self.nodes[node].data else {
            let found = self.nodes[node].data.kind();
            return Err(exception::constructor_error(
                py,
                format!("while constructing {what}: expected a sequence, but found {found}"),
            ));
        };
        for subnode in items.clone() {
            let (key_node, value_node) = match &self.nodes[subnode].data {
                NodeData::Mapping(entries) if entries.len() == 1 => entries[0],
                NodeData::Mapping(entries) => {
                    return Err(exception::constructor_error(
                        py,
                        format!(
                            "while constructing {what}: expected a single mapping item, but found {} items",
                            entries.len()
                        ),
                    ));
                }
                data => {
                    return Err(exception::constructor_error(
                        py,
                        format!(
                            "while constructing {what}: expected a mapping of length 1, but found {}",
                            data.kind()
                        ),
                    ));
                }
            };
            let key = self.construct_object(py, key_node)?;
            let value = self.construct_object(py, value_node)?;
            pairs.append(PyTuple::new(py, [key, value])?)?;
        }
        Ok(())
    }
}

/// Raises PyYAML's `UnicodeDecodeError` for a tag whose URI escapes are not
/// valid UTF-8. libyaml passes such octets through, and PyYAML decodes the tag
/// when composing the node, after checking its anchor.
fn check_tag_utf8(py: Python, tag: Option<&str>) -> PyResult<()> {
    let Some(tag) = tag.filter(|tag| tag.contains(INVALID_UTF8_ESCAPE)) else {
        return Ok(());
    };
    let bytes = PyBytes::new(py, &tag_octets(tag));
    bytes.call_method1("decode", ("utf-8",))?;
    unreachable!("libyaml-safer marks only octets that are not UTF-8")
}

/// The octets libyaml would give for a tag marked by libyaml-safer: each
/// marker is followed by one escaped sequence, whose width comes from its
/// leading octet.
fn tag_octets(tag: &str) -> Vec<u8> {
    let mut octets = Vec::with_capacity(tag.len());
    let mut rest = tag;
    while let Some(start) = rest.find(INVALID_UTF8_ESCAPE) {
        octets.extend_from_slice(&rest.as_bytes()[..start]);
        rest = &rest[start + INVALID_UTF8_ESCAPE.len_utf8()..];
        let octet_at = |rest: &str, i: usize| {
            u8::from_str_radix(&rest[i * 3 + 1..i * 3 + 3], 16).expect("escaped octet")
        };
        let lead = octet_at(rest, 0);
        let width = match lead {
            b if b & 0xe0 == 0xc0 => 2,
            b if b & 0xf0 == 0xe0 => 3,
            _ => 4,
        };
        octets.extend((0..width).map(|i| octet_at(rest, i)));
        rest = &rest[width * 3..];
    }
    octets.extend_from_slice(rest.as_bytes());
    octets
}

/// A collection's tag: no tag or `!` means the default for its kind.
fn collection_tag(tag: Option<String>, default: Tag) -> Tag {
    match tag {
        None => default,
        Some(tag) if tag == "!" => default,
        Some(tag) => Tag::from_string(tag),
    }
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

fn unknown_tag_message(tag: &str) -> String {
    format!("could not determine a constructor for the tag '{}'", tag)
}

fn single_document_error(py: Python) -> PyErr {
    exception::composer_error(
        py,
        "expected a single document in the stream, but found another document".to_string(),
    )
}

/// The PyYAML exception class for a libyaml-safer error, by the stage that
/// raised it (PyYAML raises `ParserError` for structural errors, for example).
fn parse_error(py: Python, error: &libyaml_safer::Error) -> PyErr {
    let class = match error.kind() {
        // I/O errors only come from `Input`: bytes that did not decode.
        ErrorKind::Reader | ErrorKind::Io => "ReaderError",
        ErrorKind::Parser => "ParserError",
        ErrorKind::Composer => "ComposerError",
        ErrorKind::Scanner | ErrorKind::Emitter => "ScannerError",
    };
    let message = match error.kind() {
        ErrorKind::Io => format!("Reader error: {error}"),
        _ => format!("{error}"),
    };
    exception::yaml_error(py, class, message)
}

fn expected_scalar_error(py: Python, found: &str) -> PyErr {
    exception::constructor_error(py, format!("expected a scalar node, but found {}", found))
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
        // PyYAML indexes value[0] of the empty string.
        return Err(PyIndexError::new_err("string index out of range"));
    }

    let (sign, remaining) = match bytes[0] {
        b'-' => (-1i64, &value[1..]),
        b'+' => (1i64, &value[1..]),
        _ => (1i64, value),
    };

    if remaining.is_empty() {
        // PyYAML indexes value[0] after stripping the sign.
        return Err(PyIndexError::new_err("string index out of range"));
    }
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

/// Mirrors PyYAML's `construct_yaml_float`: strip underscores, lowercase,
/// strip one sign, then `.inf` / `.nan` / sexagesimal / `float()`. Pieces Rust
/// cannot parse go to Python's `float()`, so accepted forms and errors match.
fn construct_float_fallback(py: Python, value: &str) -> PyResult<Py<PyAny>> {
    let value = value.replace('_', "").to_lowercase();
    let Some(first) = value.chars().next() else {
        return Err(PyIndexError::new_err("string index out of range"));
    };
    let sign = if first == '-' { -1.0 } else { 1.0 };
    let remaining = if matches!(first, '-' | '+') {
        &value[1..]
    } else {
        value.as_str()
    };

    if remaining == ".inf" {
        return Ok(PyFloat::new(py, sign * f64::INFINITY).into_any().unbind());
    }
    if remaining == ".nan" {
        // PyYAML returns one shared NaN object, so repeated `.nan` mapping keys
        // collapse via dict's identity check; mirror that.
        let nan = NAN.get_or_init(py, || PyFloat::new(py, f64::NAN).into_any().unbind());
        return Ok(nan.clone_ref(py));
    }

    let result = if remaining.contains(':') {
        // PyYAML converts every part first (left to right, so the first bad
        // part raises ValueError before any OverflowError), then sums them
        // least significant first. Its base is an exact Python int, rounded to
        // a float for each multiplication (which raises OverflowError once it
        // no longer fits).
        let digits = remaining
            .split(':')
            .map(|part| python_float(py, part))
            .collect::<PyResult<Vec<f64>>>()?;
        let mut result = 0.0;
        let mut base = Some(1u128);
        for (power, digit) in digits.into_iter().rev().enumerate() {
            let base_f = match base {
                Some(b) => b as f64,
                None => PyInt::new(py, 60)
                    .pow(power as u32, None::<u32>)?
                    .call_method0("__float__")?
                    .extract::<f64>()?,
            };
            result += digit * base_f;
            base = base.and_then(|b| b.checked_mul(60));
        }
        result
    } else {
        python_float(py, remaining)?
    };
    Ok(PyFloat::new(py, sign * result).into_any().unbind())
}

/// Python's `float(s)`, using Rust's parser when it accepts `s`.
fn python_float(py: Python, s: &str) -> PyResult<f64> {
    if let Ok(v) = s.parse::<f64>() {
        return Ok(v);
    }
    py.import("builtins")?
        .getattr("float")?
        .call1((s,))?
        .extract::<f64>()
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

pub fn register_loader(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RSafeLoader>()?;
    Ok(())
}
