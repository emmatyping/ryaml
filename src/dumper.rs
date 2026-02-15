//! Full RSafeDumper implementation: emitter + serializer + SafeRepresenter + resolver.
//! All in Rust, matching the RSafeLoader pattern.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::Engine as _;
use libyaml_safer::{Emitter, Encoding, Event, MappingStyle, ScalarStyle, SequenceStyle};
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyString, PyTuple,
};

use crate::exception;
use crate::resolver;

/// Internal representation node used by the representer/serializer.
/// Uses Rc for alias detection via pointer identity.
#[derive(Debug)]
enum RepNode {
    Scalar {
        tag: String,
        value: String,
        style: Option<char>,
    },
    Sequence {
        tag: String,
        value: Vec<Arc<RepNode>>,
        flow_style: Option<bool>,
    },
    Mapping {
        tag: String,
        value: Vec<(Arc<RepNode>, Arc<RepNode>)>,
        flow_style: Option<bool>,
    },
}

/// Wraps libyaml Emitter with a self-owned output buffer.
///
/// Safety: `emitter` borrows from `output` via an unsafe lifetime cast.
/// The `output` is heap-allocated (Box) so it has a stable address.
/// We guarantee emitter is always dropped before output by setting it
/// to None in dispose()/Drop.
struct EmitterWrapper {
    #[allow(clippy::box_collection)]
    output: Box<Vec<u8>>,
    emitter: Option<Emitter<'static>>,
}

impl EmitterWrapper {
    fn new() -> Self {
        let output = Box::new(Vec::new());
        let emitter = Emitter::new();
        EmitterWrapper {
            output,
            emitter: Some(emitter),
        }
    }

    fn configure(&mut self, encoding: Encoding) {
        // SAFETY: output lives in a Box (stable heap address) and we guarantee
        // the emitter is dropped before output (see dispose/Drop).
        let output_ref: &'static mut Vec<u8> =
            unsafe { &mut *(self.output.as_mut() as *mut Vec<u8>) };
        let emitter = self.emitter.as_mut().unwrap();
        emitter.set_encoding(encoding);
        emitter.set_output_string(output_ref);
    }

    fn emitter_mut(&mut self) -> &mut Emitter<'static> {
        self.emitter.as_mut().expect("emitter already disposed")
    }

    fn emit(&mut self, event: Event) -> Result<(), String> {
        self.emitter_mut()
            .emit(event)
            .map_err(|e| format!("emitter error: {e}"))
    }

    fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(self.output.as_mut())
    }

    fn dispose(&mut self) {
        // Drop emitter first (releases borrow on output)
        self.emitter = None;
    }
}

impl Drop for EmitterWrapper {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[pyclass(name = "_RSafeDumper", subclass)]
pub struct RSafeDumper {
    // Emitter
    emitter: EmitterWrapper,
    stream: Py<PyAny>,
    dump_unicode: bool,
    // -1 = not opened, 0 = open, 1 = closed
    closed: i8,

    // Serializer config
    document_start_implicit: bool,
    document_end_implicit: bool,

    // Serializer state (reset per document)
    serialized_nodes: HashSet<usize>,
    anchors: HashMap<usize, Option<String>>,
    last_alias_id: i32,

    // Representer config
    default_style: Option<char>,
    default_flow_style: Option<bool>,
    sort_keys: bool,

    // Representer state (reset per represent() call)
    represented_objects: HashMap<usize, Arc<RepNode>>,
    object_keeper: Vec<Py<PyAny>>,
}

#[pymethods]
impl RSafeDumper {
    #[allow(clippy::too_many_arguments)]
    #[new]
    #[pyo3(signature = (stream, default_style=None, default_flow_style=Some(false),
        canonical=None, indent=None, width=None, allow_unicode=None,
        line_break=None, encoding=None, explicit_start=None, explicit_end=None,
        version=None, tags=None, sort_keys=false))]
    #[allow(unused_variables)]
    fn new(
        py: Python,
        stream: Py<PyAny>,
        default_style: Option<&str>,
        default_flow_style: Option<bool>,
        canonical: Option<bool>,
        indent: Option<i32>,
        width: Option<i32>,
        allow_unicode: Option<bool>,
        line_break: Option<&str>,
        encoding: Option<&str>,
        explicit_start: Option<bool>,
        explicit_end: Option<bool>,
        version: Option<(i32, i32)>,
        tags: Option<HashMap<String, String>>,
        sort_keys: bool,
    ) -> PyResult<Self> {
        let mut ew = EmitterWrapper::new();

        // Configure emitter
        let enc = match encoding {
            None | Some("utf-8") | Some("utf8") => Encoding::Utf8,
            Some("utf-16-le") | Some("utf-16le") => Encoding::Utf16Le,
            Some("utf-16-be") | Some("utf-16be") => Encoding::Utf16Be,
            Some(other) => {
                return Err(exception::emitter_error(
                    py,
                    format!("unknown encoding: {other}"),
                ));
            }
        };
        ew.configure(enc);

        if let Some(true) = canonical {
            ew.emitter_mut().set_canonical(true);
        }
        if let Some(i) = indent {
            ew.emitter_mut().set_indent(i);
        }
        if let Some(w) = width {
            ew.emitter_mut().set_width(w);
        }
        if let Some(true) = allow_unicode {
            ew.emitter_mut().set_unicode(true);
        }
        if let Some(lb) = line_break {
            let brk = match lb {
                "\n" => libyaml_safer::Break::Ln,
                "\r" => libyaml_safer::Break::Cr,
                "\r\n" => libyaml_safer::Break::CrLn,
                _ => libyaml_safer::Break::Ln,
            };
            ew.emitter_mut().set_break(brk);
        }

        let dump_unicode = encoding.is_none();

        let default_style_char = default_style.and_then(|s| s.chars().next());

        Ok(RSafeDumper {
            emitter: ew,
            stream,
            dump_unicode,
            closed: -1,
            document_start_implicit: !explicit_start.unwrap_or(false),
            document_end_implicit: !explicit_end.unwrap_or(false),
            serialized_nodes: HashSet::new(),
            anchors: HashMap::new(),
            last_alias_id: 0,
            default_style: default_style_char,
            default_flow_style,
            sort_keys,
            represented_objects: HashMap::new(),
            object_keeper: Vec::new(),
        })
    }

    fn open(&mut self, py: Python) -> PyResult<()> {
        if self.closed != -1 {
            return Err(exception::serializer_error(
                py,
                if self.closed == 1 {
                    "serializer is closed"
                } else {
                    "serializer is already opened"
                }
                .to_string(),
            ));
        }
        self.emitter
            .emit(Event::stream_start(Encoding::Utf8))
            .map_err(|e| exception::emitter_error(py, e))?;
        self.closed = 0;
        Ok(())
    }

    fn represent(&mut self, py: Python, data: Py<PyAny>) -> PyResult<()> {
        let node = self.represent_data(py, data.bind(py))?;
        self.serialize(py, &node)?;
        self.represented_objects.clear();
        self.object_keeper.clear();
        Ok(())
    }

    fn close(&mut self, py: Python) -> PyResult<()> {
        if self.closed == -1 {
            return Err(exception::serializer_error(
                py,
                "serializer is not opened".to_string(),
            ));
        }
        if self.closed == 1 {
            return Ok(());
        }
        self.emitter
            .emit(Event::stream_end())
            .map_err(|e| exception::emitter_error(py, e))?;
        self.closed = 1;

        // Flush output to stream
        let output = self.emitter.take_output();
        let stream = self.stream.bind(py);
        if self.dump_unicode {
            let s = String::from_utf8(output)
                .map_err(|e| exception::emitter_error(py, format!("invalid utf8 output: {e}")))?;
            stream.call_method1("write", (s,))?;
        } else {
            stream.call_method1("write", (PyBytes::new(py, &output),))?;
        }
        Ok(())
    }

    fn dispose(&mut self) {
        self.emitter.dispose();
    }
}

// ── Representer ──────────────────────────────────────────────────────────────

impl RSafeDumper {
    fn ignore_aliases(&self, _py: Python, data: &Bound<'_, PyAny>) -> bool {
        data.is_none()
            || (data.is_instance_of::<PyTuple>() && data.len().is_ok_and(|l| l == 0))
            || data.is_instance_of::<PyString>()
            || data.is_instance_of::<PyBytes>()
            || data.is_instance_of::<PyBool>()
            || data.is_instance_of::<PyInt>()
            || data.is_instance_of::<PyFloat>()
    }

    fn represent_data(&mut self, py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        // Alias tracking
        let alias_key = if self.ignore_aliases(py, data) {
            None
        } else {
            let key = data.as_ptr() as usize;
            if let Some(node) = self.represented_objects.get(&key) {
                return Ok(Arc::clone(node));
            }
            self.object_keeper.push(data.clone().unbind());
            Some(key)
        };

        // Type dispatch (order matters: bool before int, datetime before date)
        let node = if data.is_none() {
            self.represent_none()
        } else if data.is_instance_of::<PyBool>() {
            self.represent_bool(data)?
        } else if data.is_instance_of::<PyInt>() {
            self.represent_int(data)?
        } else if data.is_instance_of::<PyFloat>() {
            self.represent_float(data)?
        } else if data.is_instance_of::<PyString>() {
            self.represent_str(data)?
        } else if data.is_instance_of::<PyBytes>() {
            self.represent_binary(py, data)?
        } else if Self::is_datetime(py, data)? {
            self.represent_datetime(py, data)?
        } else if Self::is_date(py, data)? {
            self.represent_date(py, data)?
        } else if data.is_instance_of::<PyList>() || data.is_instance_of::<PyTuple>() {
            self.represent_list(py, data)?
        } else if data.is_instance_of::<PyDict>() {
            self.represent_dict(py, data)?
        } else if data.is_instance_of::<PySet>() || data.is_instance_of::<PyFrozenSet>() {
            self.represent_set(py, data)?
        } else {
            return Err(exception::representer_error(
                py,
                format!("cannot represent an object: {:?}", data.get_type().name()?),
            ));
        };

        if let Some(key) = alias_key {
            self.represented_objects.insert(key, Arc::clone(&node));
        }

        Ok(node)
    }

    fn represent_none(&self) -> Arc<RepNode> {
        self.make_scalar("tag:yaml.org,2002:null", "null", None)
    }

    fn represent_bool(&self, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let b: bool = data.extract()?;
        let value = if b { "true" } else { "false" };
        Ok(self.make_scalar("tag:yaml.org,2002:bool", value, None))
    }

    fn represent_int(&self, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let s = data.str()?.to_string();
        Ok(self.make_scalar("tag:yaml.org,2002:int", &s, None))
    }

    fn represent_float(&self, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let f: f64 = data.extract()?;
        let value = format_float(f);
        Ok(self.make_scalar("tag:yaml.org,2002:float", &value, None))
    }

    fn represent_str(&self, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let s: String = data.extract()?;
        Ok(self.make_scalar("tag:yaml.org,2002:str", &s, None))
    }

    fn represent_binary(&self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let bytes: &[u8] = data.downcast::<PyBytes>()?.as_bytes();
        let encoded = base64::prelude::BASE64_STANDARD.encode(bytes);

        // Add line breaks every 76 characters to match Python's base64.encodebytes()
        let mut result = String::new();
        for (i, ch) in encoded.chars().enumerate() {
            if i > 0 && i % 76 == 0 {
                result.push('\n');
            }
            result.push(ch);
        }
        result.push('\n');

        Ok(self.make_scalar("tag:yaml.org,2002:binary", &result, Some('|')))
    }

    fn represent_date(&self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let value: String = data.call_method0("isoformat")?.extract()?;
        Ok(self.make_scalar("tag:yaml.org,2002:timestamp", &value, None))
    }

    fn represent_datetime(&self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let value: String = data.call_method1("isoformat", (" ",))?.extract()?;
        Ok(self.make_scalar("tag:yaml.org,2002:timestamp", &value, None))
    }

    fn represent_list(&mut self, py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        // Get iterator by calling __iter__
        let iter_obj = data.call_method0("__iter__")?;
        let mut items = Vec::new();
        let mut best_style = true;
        loop {
            match iter_obj.call_method0("__next__") {
                Ok(item) => {
                    let node = self.represent_data(py, &item)?;
                    if !is_plain_scalar(&node) {
                        best_style = false;
                    }
                    items.push(node);
                }
                Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => break,
                Err(e) => return Err(e),
            }
        }
        let flow_style = self.choose_flow_style(best_style);
        Ok(Arc::new(RepNode::Sequence {
            tag: "tag:yaml.org,2002:seq".to_string(),
            value: items,
            flow_style,
        }))
    }

    fn represent_dict(&mut self, py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        let dict = data.downcast::<PyDict>()?;
        let mut pairs: Vec<(Py<PyAny>, Py<PyAny>)> =
            dict.iter().map(|(k, v)| (k.unbind(), v.unbind())).collect();

        if self.sort_keys {
            // Sort by key, ignoring errors (matching pyyaml which wraps in try/except TypeError)
            let _ = try_sort_pairs(py, &mut pairs);
        }

        let mut items = Vec::new();
        let mut best_style = true;
        for (k, v) in &pairs {
            let key_node = self.represent_data(py, k.bind(py))?;
            let val_node = self.represent_data(py, v.bind(py))?;
            if !is_plain_scalar(&key_node) || !is_plain_scalar(&val_node) {
                best_style = false;
            }
            items.push((key_node, val_node));
        }
        let flow_style = self.choose_flow_style(best_style);
        Ok(Arc::new(RepNode::Mapping {
            tag: "tag:yaml.org,2002:map".to_string(),
            value: items,
            flow_style,
        }))
    }

    fn represent_set(&mut self, py: Python, data: &Bound<'_, PyAny>) -> PyResult<Arc<RepNode>> {
        // Get iterator by calling __iter__
        let iter_obj = data.call_method0("__iter__")?;
        let mut items = Vec::new();
        loop {
            match iter_obj.call_method0("__next__") {
                Ok(item) => {
                    let key_node = self.represent_data(py, &item)?;
                    // Create a fresh null node for each value (don't share Arc to avoid aliases)
                    let null_node = self.represent_none();
                    items.push((key_node, null_node));
                }
                Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(Arc::new(RepNode::Mapping {
            tag: "tag:yaml.org,2002:set".to_string(),
            value: items,
            flow_style: Some(false),
        }))
    }

    // ── Helpers ──

    fn make_scalar(&self, tag: &str, value: &str, style: Option<char>) -> Arc<RepNode> {
        let style = style.or(self.default_style);
        Arc::new(RepNode::Scalar {
            tag: tag.to_string(),
            value: value.to_string(),
            style,
        })
    }

    fn choose_flow_style(&self, best_style: bool) -> Option<bool> {
        if let Some(dfs) = self.default_flow_style {
            Some(dfs)
        } else {
            Some(best_style)
        }
    }

    fn is_datetime(py: Python, data: &Bound<'_, PyAny>) -> PyResult<bool> {
        let datetime_mod = py.import("datetime")?;
        let datetime_cls = datetime_mod.getattr("datetime")?;
        data.is_instance(&datetime_cls)
    }

    fn is_date(py: Python, data: &Bound<'_, PyAny>) -> PyResult<bool> {
        let datetime_mod = py.import("datetime")?;
        let date_cls = datetime_mod.getattr("date")?;
        data.is_instance(&date_cls)
    }
}

// ── Serializer ───────────────────────────────────────────────────────────────

impl RSafeDumper {
    fn serialize(&mut self, py: Python, node: &Arc<RepNode>) -> PyResult<()> {
        self.emitter
            .emit(Event::document_start(
                None,
                &[],
                self.document_start_implicit,
            ))
            .map_err(|e| exception::emitter_error(py, e))?;

        self.anchor_node(node);
        self.serialize_node(py, node)?;

        self.emitter
            .emit(Event::document_end(self.document_end_implicit))
            .map_err(|e| exception::emitter_error(py, e))?;

        // Reset serializer state
        self.serialized_nodes.clear();
        self.anchors.clear();
        self.last_alias_id = 0;
        Ok(())
    }

    fn anchor_node(&mut self, node: &Arc<RepNode>) {
        let key = Arc::as_ptr(node) as usize;
        if let Some(anchor) = self.anchors.get_mut(&key) {
            // Seen before with None → assign anchor name
            if anchor.is_none() {
                self.last_alias_id += 1;
                *anchor = Some(format!("id{:03}", self.last_alias_id));
            }
        } else {
            self.anchors.insert(key, None);
            match node.as_ref() {
                RepNode::Sequence { value, .. } => {
                    for item in value {
                        self.anchor_node(item);
                    }
                }
                RepNode::Mapping { value, .. } => {
                    for (k, v) in value {
                        self.anchor_node(k);
                        self.anchor_node(v);
                    }
                }
                RepNode::Scalar { .. } => {}
            }
        }
    }

    fn serialize_node(&mut self, py: Python, node: &Arc<RepNode>) -> PyResult<()> {
        let key = Arc::as_ptr(node) as usize;
        let anchor = self.anchors.get(&key).cloned().flatten();

        if self.serialized_nodes.contains(&key) {
            // Emit alias
            let anchor_str = anchor.as_deref().unwrap_or("");
            self.emitter
                .emit(Event::alias(anchor_str))
                .map_err(|e| exception::emitter_error(py, e))?;
            return Ok(());
        }
        self.serialized_nodes.insert(key);

        let anchor_ref = anchor.as_deref();

        match node.as_ref() {
            RepNode::Scalar { tag, value, style } => {
                let detected_tag = resolver::resolve_scalar_tag(value, true);
                let default_tag = resolver::resolve_scalar_tag(value, false);
                let plain_implicit = tag == detected_tag;
                let quoted_implicit = tag == default_tag;
                let scalar_style = char_to_scalar_style(*style);

                self.emitter
                    .emit(Event::scalar(
                        anchor_ref,
                        Some(tag),
                        value,
                        plain_implicit,
                        quoted_implicit,
                        scalar_style,
                    ))
                    .map_err(|e| exception::emitter_error(py, e))?;
            }
            RepNode::Sequence {
                tag,
                value,
                flow_style,
            } => {
                let implicit = tag == resolver::DEFAULT_SEQUENCE_TAG;
                let style = match flow_style {
                    Some(true) => SequenceStyle::Flow,
                    Some(false) => SequenceStyle::Block,
                    None => SequenceStyle::Any,
                };
                self.emitter
                    .emit(Event::sequence_start(
                        anchor_ref,
                        Some(tag),
                        implicit,
                        style,
                    ))
                    .map_err(|e| exception::emitter_error(py, e))?;
                for item in value {
                    self.serialize_node(py, item)?;
                }
                self.emitter
                    .emit(Event::sequence_end())
                    .map_err(|e| exception::emitter_error(py, e))?;
            }
            RepNode::Mapping {
                tag,
                value,
                flow_style,
            } => {
                let implicit = tag == resolver::DEFAULT_MAPPING_TAG;
                let style = match flow_style {
                    Some(true) => MappingStyle::Flow,
                    Some(false) => MappingStyle::Block,
                    None => MappingStyle::Any,
                };
                self.emitter
                    .emit(Event::mapping_start(anchor_ref, Some(tag), implicit, style))
                    .map_err(|e| exception::emitter_error(py, e))?;
                for (k, v) in value {
                    self.serialize_node(py, k)?;
                    self.serialize_node(py, v)?;
                }
                self.emitter
                    .emit(Event::mapping_end())
                    .map_err(|e| exception::emitter_error(py, e))?;
            }
        }
        Ok(())
    }
}

// ── Free helpers ─────────────────────────────────────────────────────────────

fn is_plain_scalar(node: &Arc<RepNode>) -> bool {
    matches!(node.as_ref(), RepNode::Scalar { style: None, .. })
}

fn char_to_scalar_style(style: Option<char>) -> ScalarStyle {
    match style {
        None => ScalarStyle::Any,
        Some('\'') => ScalarStyle::SingleQuoted,
        Some('"') => ScalarStyle::DoubleQuoted,
        Some('|') => ScalarStyle::Literal,
        Some('>') => ScalarStyle::Folded,
        _ => ScalarStyle::Any,
    }
}

/// Format a float matching pyyaml's SafeRepresenter.represent_float
fn format_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".to_string();
    }
    if f.is_infinite() {
        return if f.is_sign_positive() {
            ".inf"
        } else {
            "-.inf"
        }
        .to_string();
    }
    // Use Python's repr-like formatting
    let mut value = format!("{}", f);
    // Ensure lower case for scientific notation
    value = value.to_lowercase();
    // If there's no decimal point but there's an 'e', add '.0' before it
    if !value.contains('.') && value.contains('e') {
        value = value.replacen('e', ".0e", 1);
    }
    // If there's no decimal point and no 'e', it's an integer-looking float
    // Python repr would show e.g. "1.0", but Rust format! shows "1" for 1.0f64
    // Actually Rust shows "1" only for integers; for f64 it shows e.g. "1.5"
    // But for whole numbers like 1.0, Rust shows "1" with {} formatter
    if !value.contains('.') && !value.contains('e') {
        value.push_str(".0");
    }
    value
}

/// Try to sort (key, value) pairs by key. Silently fails on TypeError (matching pyyaml).
fn try_sort_pairs(py: Python, pairs: &mut [(Py<PyAny>, Py<PyAny>)]) -> PyResult<()> {
    // Use Python's comparison to sort keys
    pairs.sort_by(|a, b| {
        a.0.bind(py)
            .lt(b.0.bind(py))
            .and_then(|lt| {
                if lt {
                    Ok(std::cmp::Ordering::Less)
                } else {
                    a.0.bind(py).gt(b.0.bind(py)).map(|gt| {
                        if gt {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    })
                }
            })
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(())
}

// ── Fast-path for dumps() ────────────────────────────────────────────────────

/// Dump a Python object to a YAML string, bypassing the pyyaml stream protocol.
pub fn dumps_to_string(py: Python, obj: &Bound<'_, PyAny>) -> PyResult<String> {
    let mut ew = EmitterWrapper::new();
    ew.configure(Encoding::Utf8);

    let mut dumper = RSafeDumper {
        emitter: ew,
        stream: py.None(),
        dump_unicode: true,
        closed: -1,
        document_start_implicit: true,
        document_end_implicit: true,
        serialized_nodes: HashSet::new(),
        anchors: HashMap::new(),
        last_alias_id: 0,
        default_style: None,
        default_flow_style: Some(false),
        sort_keys: false,
        represented_objects: HashMap::new(),
        object_keeper: Vec::new(),
    };

    dumper
        .emitter
        .emit(Event::stream_start(Encoding::Utf8))
        .map_err(|e| exception::emitter_error(py, e))?;

    let node = dumper.represent_data(py, obj)?;
    dumper.serialize(py, &node)?;

    dumper
        .emitter
        .emit(Event::stream_end())
        .map_err(|e| exception::emitter_error(py, e))?;

    let output = dumper.emitter.take_output();
    String::from_utf8(output)
        .map_err(|e| exception::emitter_error(py, format!("invalid utf8 output: {e}")))
}

pub fn register_dumper(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RSafeDumper>()?;
    Ok(())
}
