//! Full RSafeDumper implementation: emitter + serializer + SafeRepresenter + resolver.
//! All in Rust, matching the RSafeLoader pattern.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use libyaml_safer::{
    Emitter, Encoding, Event, MappingStyle, ScalarStyle, SequenceStyle, TagDirective,
    VersionDirective,
};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PySet, PyString, PyTuple, PyType,
};

use crate::exception;
use crate::resolver;

/// A represented node (PyYAML's `yaml.nodes`), kept in `RSafeDumper::nodes`.
/// Children are indices into it, so a collection can be registered before its
/// children are represented and recursive data refers back to it, as in
/// PyYAML's representer.
#[derive(Debug)]
enum RepNode {
    Scalar {
        tag: &'static str,
        /// The text, or the error converting a `str` to UTF-8 (a lone
        /// surrogate), which PyYAML's emitter only raises when it writes the
        /// scalar, after the whole document is represented.
        value: PyResult<String>,
        style: Option<char>,
    },
    Sequence {
        tag: &'static str,
        value: Vec<NodeId>,
        flow_style: Option<bool>,
    },
    Mapping {
        tag: &'static str,
        value: Vec<(NodeId, NodeId)>,
        flow_style: Option<bool>,
    },
}

type NodeId = usize;

/// Where the emitter's output goes. Like PyYAML's CEmitter, whose libyaml
/// output handler writes every flush to the stream (at each document end, and
/// whenever the buffer fills), each flush is written to `stream` as it happens,
/// as `str` or `bytes`. Without a stream (`ryaml.dumps`) the output is kept.
#[derive(Default)]
struct StreamWriter {
    stream: Option<Py<PyAny>>,
    dump_unicode: bool,
    output: Vec<u8>,
    /// The exception the stream's `write` raised, re-raised by `emit`.
    error: Option<PyErr>,
}

impl std::io::Write for StreamWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(stream) = &self.stream else {
            self.output.extend_from_slice(buf);
            return Ok(buf.len());
        };
        let result = Python::attach(|py| -> PyResult<()> {
            let stream = stream.bind(py);
            if self.dump_unicode {
                let text = std::str::from_utf8(buf).map_err(|e| {
                    exception::emitter_error(py, format!("invalid utf8 output: {e}"))
                })?;
                stream.call_method1("write", (text,))?;
            } else {
                stream.call_method1("write", (PyBytes::new(py, buf),))?;
            }
            Ok(())
        });
        match result {
            // The emitter writes each flush with one write_all.
            Ok(()) => Ok(buf.len()),
            Err(error) => {
                self.error = Some(error);
                Err(std::io::Error::other("write error"))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Wraps libyaml Emitter with a self-owned output buffer.
///
/// Safety: `emitter` borrows from `output` via an unsafe lifetime cast.
/// The `output` is heap-allocated (Box) so it has a stable address.
/// We guarantee emitter is always dropped before output by setting it
/// to None in dispose()/Drop.
struct EmitterWrapper {
    output: Box<StreamWriter>,
    emitter: Option<Emitter<'static>>,
}

impl EmitterWrapper {
    fn new() -> Self {
        let output = Box::new(StreamWriter::default());
        let emitter = Emitter::new();
        EmitterWrapper {
            output,
            emitter: Some(emitter),
        }
    }

    fn configure(&mut self, encoding: Encoding, stream: Option<Py<PyAny>>, dump_unicode: bool) {
        self.output.stream = stream;
        self.output.dump_unicode = dump_unicode;
        // SAFETY: output lives in a Box (stable heap address) and we guarantee
        // the emitter is dropped before output (see dispose/Drop).
        let output_ref: &'static mut StreamWriter =
            unsafe { &mut *(self.output.as_mut() as *mut StreamWriter) };
        let emitter = self.emitter.as_mut().unwrap();
        emitter.set_encoding(encoding);
        emitter.set_output(output_ref);
    }

    fn emitter_mut(&mut self) -> &mut Emitter<'static> {
        self.emitter.as_mut().expect("emitter already disposed")
    }

    fn emit(&mut self, event: Event) -> Result<(), String> {
        self.emitter_mut()
            .emit(event)
            .map_err(|e| format!("emitter error: {e}"))
    }

    /// The output flushed so far, without a stream.
    fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output.output)
    }

    /// The exception the stream raised during the last emit, if any.
    fn take_write_error(&mut self) -> Option<PyErr> {
        self.output.error.take()
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
    // -1 = not opened, 0 = open, 1 = closed
    closed: i8,

    // Serializer config
    document_start_implicit: bool,
    /// `%YAML` and `%TAG` directives for every document (PyYAML's
    /// `use_version` and `use_tags`), tags in the dict's order.
    version: Option<VersionDirective>,
    tags: Vec<TagDirective>,
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
    represented_objects: HashMap<usize, NodeId>,
    /// Nodes of the document being represented.
    nodes: Vec<RepNode>,
    object_keeper: Vec<Py<PyAny>>,
    /// Nesting depth of the value being represented (the root is 0).
    level: isize,
    budget: RecursionBudget,
}

/// The deepest levels at which PyYAML's pure-Python representer still fits in
/// the recursion limit, so ryaml raises `RecursionError` where it would.
///
/// Called with `P` Python frames on the stack (up to `dump_all`'s), PyYAML's
/// `represent` runs at frame `P + 1` and `represent_data` for a value at level
/// `d` at `P + 2 + 3d`: each level adds `represent_data`, the type's
/// representer and `represent_sequence`/`represent_mapping`. Every value first
/// calls `ignore_aliases` (frame `P + 3 + 3d`), as do aliases and refused
/// values (`represent_undefined`); a represented value reaches its node's
/// `__init__` at `P + 5 + 3d`. Python raises once a frame passes the limit.
#[derive(Clone, Copy, Debug)]
struct RecursionBudget {
    /// Deepest level reaching `P + 3 + 3d`.
    max_seen_level: isize,
    /// Deepest level reaching `P + 5 + 3d`.
    max_node_level: isize,
}

impl RecursionBudget {
    const UNLIMITED: Self = Self {
        max_seen_level: isize::MAX,
        max_node_level: isize::MAX,
    };

    /// The budget for a representer called from the current Python frame,
    /// plus `extra_frames` frames PyYAML would have on top of it.
    fn current(py: Python, extra_frames: isize) -> PyResult<Self> {
        let mut frames = extra_frames;
        let mut frame = py.import("sys")?.call_method1("_getframe", (0,))?;
        while !frame.is_none() {
            frames += 1;
            frame = frame.getattr("f_back")?;
        }
        let limit = py
            .import("sys")?
            .call_method0("getrecursionlimit")?
            .extract::<isize>()?;
        Ok(Self {
            max_seen_level: (limit - frames - 3).div_euclid(3),
            max_node_level: (limit - frames - 5).div_euclid(3),
        })
    }
}

fn recursion_error() -> PyErr {
    pyo3::exceptions::PyRecursionError::new_err("maximum recursion depth exceeded")
}

/// Guards one level of native recursion with Python's own check, so very deep
/// data raises `RecursionError` instead of overflowing the native stack.
struct RecursionGuard;

impl RecursionGuard {
    fn enter(py: Python) -> PyResult<Self> {
        // SAFETY: called with the GIL held; the guard calls
        // Py_LeaveRecursiveCall exactly once when dropped.
        if unsafe { pyo3::ffi::Py_EnterRecursiveCall(c" while representing an object".as_ptr()) }
            != 0
        {
            return Err(PyErr::fetch(py));
        }
        Ok(RecursionGuard)
    }
}

impl Drop for RecursionGuard {
    fn drop(&mut self) {
        // SAFETY: paired with the successful Py_EnterRecursiveCall in `enter`,
        // still holding the GIL.
        unsafe { pyo3::ffi::Py_LeaveRecursiveCall() }
    }
}

#[pymethods]
impl RSafeDumper {
    #[allow(clippy::too_many_arguments)]
    #[new]
    #[pyo3(signature = (stream, default_style=None, default_flow_style=Some(false),
        canonical=None, indent=None, width=None, allow_unicode=None,
        line_break=None, encoding=None, explicit_start=None, explicit_end=None,
        version=None, tags=None, sort_keys=true))]
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
        encoding: Option<Bound<'_, PyAny>>,
        explicit_start: Option<bool>,
        explicit_end: Option<bool>,
        version: Option<(i32, i32)>,
        tags: Option<Bound<'_, PyDict>>,
        sort_keys: bool,
    ) -> PyResult<Self> {
        let mut ew = EmitterWrapper::new();

        // Like PyYAML's CEmitter: it writes str (so UTF-8) to a stream with an
        // `encoding` attribute, or when no encoding is given. Otherwise it
        // writes bytes: UTF-16 for exactly "utf-16-le" or "utf-16-be", UTF-8
        // for any other encoding.
        let dump_unicode = stream.bind(py).hasattr("encoding")? || encoding.is_none();
        let enc = match encoding
            .as_ref()
            .and_then(|e| e.extract::<String>().ok())
            .as_deref()
        {
            _ if dump_unicode => Encoding::Utf8,
            Some("utf-16-le") => Encoding::Utf16Le,
            Some("utf-16-be") => Encoding::Utf16Be,
            _ => Encoding::Utf8,
        };
        ew.configure(enc, Some(stream.clone_ref(py)), dump_unicode);

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

        let tags = match tags {
            Some(tags) => tags
                .iter()
                .map(|(handle, prefix)| {
                    Ok(TagDirective {
                        handle: handle.extract()?,
                        prefix: prefix.extract()?,
                    })
                })
                .collect::<PyResult<Vec<_>>>()?,
            None => Vec::new(),
        };

        let default_style_char = default_style.and_then(|s| s.chars().next());

        Ok(RSafeDumper {
            emitter: ew,
            closed: -1,
            document_start_implicit: !explicit_start.unwrap_or(false),
            version: version.map(|(major, minor)| VersionDirective { major, minor }),
            tags,
            document_end_implicit: !explicit_end.unwrap_or(false),
            serialized_nodes: HashSet::new(),
            anchors: HashMap::new(),
            last_alias_id: 0,
            default_style: default_style_char,
            default_flow_style,
            sort_keys,
            represented_objects: HashMap::new(),
            nodes: Vec::new(),
            object_keeper: Vec::new(),
            level: 0,
            budget: RecursionBudget::UNLIMITED,
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
        self.emit(py, Event::stream_start(Encoding::Utf8))?;
        self.closed = 0;
        Ok(())
    }

    fn represent(&mut self, py: Python, data: Py<PyAny>) -> PyResult<()> {
        // Called from PyYAML's dump_all, whose frame is the current one.
        self.budget = RecursionBudget::current(py, 0)?;
        self.level = 0;
        let node = self.represent_data(py, data.bind(py))?;
        self.serialize(py, node)?;
        self.represented_objects.clear();
        self.object_keeper.clear();
        self.nodes.clear();
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
        self.emit(py, Event::stream_end())?;
        self.closed = 1;
        Ok(())
    }

    /// Like PyYAML's `CEmitter.dispose`, which does nothing: the dumper can
    /// still be used, and the emitter is freed with it.
    fn dispose(&mut self) {}
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

    fn represent_data(&mut self, py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let _guard = RecursionGuard::enter(py)?;
        if self.level > self.budget.max_seen_level {
            return Err(recursion_error());
        }
        // Alias tracking. Collections register their node before representing
        // their items (see `register`), so recursive data finds it here.
        let alias_key = if self.ignore_aliases(py, data) {
            None
        } else {
            let key = data.as_ptr() as usize;
            if let Some(&node) = self.represented_objects.get(&key) {
                return Ok(node);
            }
            self.object_keeper.push(data.clone().unbind());
            Some(key)
        };

        // SafeRepresenter dispatches on the exact type only (it registers no
        // multi-representers), so subclasses and other types are refused.
        let ty = data.get_type();
        if self.level > self.budget.max_node_level && is_represented_type(py, &ty)? {
            return Err(recursion_error());
        }
        let node = if data.is_none() {
            self.represent_none()
        } else if ty.is(py.get_type::<PyBool>()) {
            self.represent_bool(data)?
        } else if ty.is(py.get_type::<PyInt>()) {
            self.represent_int(data)?
        } else if ty.is(py.get_type::<PyFloat>()) {
            self.represent_float(data)?
        } else if ty.is(py.get_type::<PyString>()) {
            self.represent_str(data)?
        } else if ty.is(py.get_type::<PyBytes>()) {
            self.represent_binary(py, data)?
        } else if ty.is(datetime_type(py, "datetime")?) {
            self.represent_datetime(py, data)?
        } else if ty.is(datetime_type(py, "date")?) {
            self.represent_date(py, data)?
        } else if ty.is(py.get_type::<PyList>()) || ty.is(py.get_type::<PyTuple>()) {
            self.represent_list(py, data, alias_key)?
        } else if ty.is(py.get_type::<PyDict>()) {
            self.represent_dict(py, data, alias_key)?
        } else if ty.is(py.get_type::<PySet>()) {
            self.represent_set(py, data, alias_key)?
        } else {
            return Err(exception::representer_error(
                py,
                format!("cannot represent an object: {}", data.repr()?),
            ));
        };

        if let Some(key) = alias_key {
            self.represented_objects.insert(key, node);
        }

        Ok(node)
    }

    fn represent_none(&mut self) -> NodeId {
        self.make_scalar(crate::TAG_NULL, "null", None)
    }

    fn represent_bool(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let b: bool = data.extract()?;
        let value = if b { "true" } else { "false" };
        Ok(self.make_scalar(crate::TAG_BOOL, value, None))
    }

    fn represent_int(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let s = data.str()?.to_string();
        Ok(self.make_scalar(crate::TAG_INT, &s, None))
    }

    fn represent_float(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let f: f64 = data.extract()?;
        let value = if f.is_nan() {
            ".nan".to_string()
        } else if f.is_infinite() {
            if f > 0.0 { ".inf" } else { "-.inf" }.to_string()
        } else {
            // PyYAML: repr(data).lower(), with ".0" added before an exponent
            // when there is no decimal point (repr(1e17) is "1e+17").
            let value = data.repr()?.to_str()?.to_lowercase();
            if !value.contains('.') && value.contains('e') {
                value.replacen('e', ".0e", 1)
            } else {
                value
            }
        };
        Ok(self.make_scalar(crate::TAG_FLOAT, &value, None))
    }

    fn represent_str(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value = data.extract::<String>();
        Ok(self.push_scalar(crate::TAG_STR, value, None))
    }

    fn represent_binary(&mut self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let bytes: &[u8] = data.downcast::<PyBytes>()?.as_bytes();
        let encoded = base64::prelude::BASE64_STANDARD.encode(bytes);

        // Python's base64.encodebytes: lines of at most 76 characters, each
        // ending in a newline (so no bytes give an empty string).
        let mut result = String::with_capacity(encoded.len() + encoded.len() / 76 + 1);
        for line in encoded.as_bytes().chunks(76) {
            result.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            result.push('\n');
        }

        Ok(self.make_scalar(crate::TAG_BINARY, &result, Some('|')))
    }

    fn represent_date(&mut self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value: String = data.call_method0("isoformat")?.extract()?;
        Ok(self.make_scalar(crate::TAG_TIMESTAMP, &value, None))
    }

    fn represent_datetime(&mut self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value: String = data.call_method1("isoformat", (" ",))?.extract()?;
        Ok(self.make_scalar(crate::TAG_TIMESTAMP, &value, None))
    }

    fn represent_list(
        &mut self,
        py: Python,
        data: &Bound<'_, PyAny>,
        alias_key: Option<usize>,
    ) -> PyResult<NodeId> {
        let node = self.register(
            alias_key,
            RepNode::Sequence {
                tag: crate::TAG_SEQ,
                value: Vec::new(),
                flow_style: None,
            },
        );
        let mut best_style = true;
        self.level += 1;
        for item in data.try_iter()? {
            let item = self.represent_data(py, &item?)?;
            best_style &= self.is_plain_scalar(item);
            // Added at once, like PyYAML: if a later item fails, the node
            // stays registered with the items so far, and a later `represent`
            // of the same object reuses it.
            if let RepNode::Sequence { value, .. } = &mut self.nodes[node] {
                value.push(item);
            }
        }
        self.level -= 1;
        let flow = self.choose_flow_style(best_style);
        if let RepNode::Sequence { flow_style, .. } = &mut self.nodes[node] {
            *flow_style = flow;
        }
        Ok(node)
    }

    fn represent_dict(
        &mut self,
        py: Python,
        data: &Bound<'_, PyAny>,
        alias_key: Option<usize>,
    ) -> PyResult<NodeId> {
        let items = data.cast::<PyDict>()?.items();
        self.represent_mapping(py, crate::TAG_MAP, items, alias_key)
    }

    /// PyYAML's `represent_set`: a mapping of the members to None.
    fn represent_set(
        &mut self,
        py: Python,
        data: &Bound<'_, PyAny>,
        alias_key: Option<usize>,
    ) -> PyResult<NodeId> {
        let value = PyDict::new(py);
        for item in data.try_iter()? {
            value.set_item(item?, py.None())?;
        }
        self.represent_mapping(py, crate::TAG_SET, value.items(), alias_key)
    }

    /// PyYAML's `represent_mapping`, for dicts and sets.
    fn represent_mapping(
        &mut self,
        py: Python,
        tag: &'static str,
        items: Bound<'_, PyList>,
        alias_key: Option<usize>,
    ) -> PyResult<NodeId> {
        let node = self.register(
            alias_key,
            RepNode::Mapping {
                tag,
                value: Vec::new(),
                flow_style: None,
            },
        );
        let items = self.mapping_items(py, items)?;
        let mut best_style = true;
        self.level += 1;
        for item in items.iter() {
            let (k, v) = item.extract::<(Bound<'_, PyAny>, Bound<'_, PyAny>)>()?;
            let key_node = self.represent_data(py, &k)?;
            let val_node = self.represent_data(py, &v)?;
            best_style &= self.is_plain_scalar(key_node) && self.is_plain_scalar(val_node);
            // Added at once, like PyYAML (see `represent_list`).
            if let RepNode::Mapping { value, .. } = &mut self.nodes[node] {
                value.push((key_node, val_node));
            }
        }
        self.level -= 1;
        let flow = self.choose_flow_style(best_style);
        if let RepNode::Mapping { flow_style, .. } = &mut self.nodes[node] {
            *flow_style = flow;
        }
        Ok(node)
    }

    // ── Helpers ──

    /// PyYAML's `represent_mapping`: the (key, value) items, sorted with
    /// Python's `sorted` when `sort_keys` is set, or left in order if that
    /// raises `TypeError` (keys that cannot be compared).
    fn mapping_items<'py>(
        &self,
        py: Python<'py>,
        items: Bound<'py, PyList>,
    ) -> PyResult<Bound<'py, PyList>> {
        if !self.sort_keys {
            return Ok(items);
        }
        match py.import("builtins")?.getattr("sorted")?.call1((&items,)) {
            Ok(sorted) => Ok(sorted.cast_into::<PyList>()?),
            Err(e) if e.is_instance_of::<pyo3::exceptions::PyTypeError>(py) => Ok(items),
            Err(e) => Err(e),
        }
    }

    /// Add a collection node and record it as `alias_key`'s node before its
    /// items are represented, like PyYAML's `represent_sequence` and
    /// `represent_mapping`.
    fn register(&mut self, alias_key: Option<usize>, node: RepNode) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        if let Some(key) = alias_key {
            self.represented_objects.insert(key, id);
        }
        id
    }

    fn is_plain_scalar(&self, node: NodeId) -> bool {
        matches!(self.nodes[node], RepNode::Scalar { style: None, .. })
    }

    fn make_scalar(&mut self, tag: &'static str, value: &str, style: Option<char>) -> NodeId {
        self.push_scalar(tag, Ok(value.to_string()), style)
    }

    fn push_scalar(
        &mut self,
        tag: &'static str,
        value: PyResult<String>,
        style: Option<char>,
    ) -> NodeId {
        let style = style.or(self.default_style);
        self.nodes.push(RepNode::Scalar { tag, value, style });
        self.nodes.len() - 1
    }

    fn choose_flow_style(&self, best_style: bool) -> Option<bool> {
        if let Some(dfs) = self.default_flow_style {
            Some(dfs)
        } else {
            Some(best_style)
        }
    }
}

// ── Serializer ───────────────────────────────────────────────────────────────

impl RSafeDumper {
    /// Emit an event; the emitter writes each flush to the stream (see
    /// `StreamWriter`).
    fn emit(&mut self, py: Python, event: Event) -> PyResult<()> {
        self.emitter.emit(event).map_err(|e| {
            // A failed write raises the stream's own exception, as in PyYAML.
            self.emitter
                .take_write_error()
                .unwrap_or_else(|| exception::emitter_error(py, e))
        })
    }

    fn serialize(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        // PyYAML represents the data first, then its serializer checks it is
        // open.
        match self.closed {
            -1 => {
                return Err(exception::serializer_error(
                    py,
                    "serializer is not opened".to_string(),
                ));
            }
            1 => {
                return Err(exception::serializer_error(
                    py,
                    "serializer is closed".to_string(),
                ));
            }
            _ => {}
        }
        self.emit(
            py,
            Event::document_start(self.version, &self.tags, self.document_start_implicit),
        )?;

        self.anchor_node(py, node)?;
        self.serialize_node(py, node)?;

        self.emit(py, Event::document_end(self.document_end_implicit))?;

        // Reset serializer state
        self.serialized_nodes.clear();
        self.anchors.clear();
        self.last_alias_id = 0;
        Ok(())
    }

    fn anchor_node(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        let _guard = RecursionGuard::enter(py)?;
        if let Some(anchor) = self.anchors.get_mut(&node) {
            // Seen before with None → assign anchor name
            if anchor.is_none() {
                self.last_alias_id += 1;
                *anchor = Some(format!("id{:03}", self.last_alias_id));
            }
            return Ok(());
        }
        self.anchors.insert(node, None);
        for index in 0.. {
            match &self.nodes[node] {
                RepNode::Sequence { value, .. } if index < value.len() => {
                    let item = value[index];
                    self.anchor_node(py, item)?;
                }
                RepNode::Mapping { value, .. } if index < value.len() => {
                    let (k, v) = value[index];
                    self.anchor_node(py, k)?;
                    self.anchor_node(py, v)?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn serialize_node(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        let _guard = RecursionGuard::enter(py)?;
        let anchor = self.anchors.get(&node).cloned().flatten();

        if self.serialized_nodes.contains(&node) {
            // Emit alias
            let anchor_str = anchor.as_deref().unwrap_or("");
            self.emit(py, Event::alias(anchor_str))?;
            return Ok(());
        }
        self.serialized_nodes.insert(node);

        let anchor_ref = anchor.as_deref();

        match &self.nodes[node] {
            RepNode::Scalar { tag, value, style } => {
                let value = value.as_ref().map_err(|e| e.clone_ref(py))?;
                let detected_tag = resolver::resolve_scalar_tag(value, true);
                let default_tag = resolver::resolve_scalar_tag(value, false);
                let plain_implicit = *tag == detected_tag;
                let quoted_implicit = *tag == default_tag;
                let scalar_style = char_to_scalar_style(*style);

                let event = Event::scalar(
                    anchor_ref,
                    Some(tag),
                    value,
                    plain_implicit,
                    quoted_implicit,
                    scalar_style,
                );
                self.emit(py, event)?;
            }
            RepNode::Sequence {
                tag, flow_style, ..
            } => {
                let implicit = *tag == resolver::DEFAULT_SEQUENCE_TAG;
                // CEmitter: flow if node.flow_style is true, else block.
                let style = if *flow_style == Some(true) {
                    SequenceStyle::Flow
                } else {
                    SequenceStyle::Block
                };
                let event = Event::sequence_start(anchor_ref, Some(tag), implicit, style);
                self.emit(py, event)?;
                for index in 0.. {
                    let RepNode::Sequence { value, .. } = &self.nodes[node] else {
                        unreachable!()
                    };
                    let Some(&item) = value.get(index) else {
                        break;
                    };
                    self.serialize_node(py, item)?;
                }
                self.emit(py, Event::sequence_end())?;
            }
            RepNode::Mapping {
                tag, flow_style, ..
            } => {
                let implicit = *tag == resolver::DEFAULT_MAPPING_TAG;
                let style = if *flow_style == Some(true) {
                    MappingStyle::Flow
                } else {
                    MappingStyle::Block
                };
                let event = Event::mapping_start(anchor_ref, Some(tag), implicit, style);
                self.emit(py, event)?;
                for index in 0.. {
                    let RepNode::Mapping { value, .. } = &self.nodes[node] else {
                        unreachable!()
                    };
                    let Some(&(k, v)) = value.get(index) else {
                        break;
                    };
                    self.serialize_node(py, k)?;
                    self.serialize_node(py, v)?;
                }
                self.emit(py, Event::mapping_end())?;
            }
        }
        Ok(())
    }
}

// ── Free helpers ─────────────────────────────────────────────────────────────

static DATE_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static DATETIME_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();

/// Whether SafeRepresenter has a representer for exactly this type.
fn is_represented_type(py: Python, ty: &Bound<'_, PyType>) -> PyResult<bool> {
    Ok(ty.is(py.None().bind(py).get_type())
        || ty.is(py.get_type::<PyBool>())
        || ty.is(py.get_type::<PyInt>())
        || ty.is(py.get_type::<PyFloat>())
        || ty.is(py.get_type::<PyString>())
        || ty.is(py.get_type::<PyBytes>())
        || ty.is(py.get_type::<PyList>())
        || ty.is(py.get_type::<PyTuple>())
        || ty.is(py.get_type::<PyDict>())
        || ty.is(py.get_type::<PySet>())
        || ty.is(datetime_type(py, "date")?)
        || ty.is(datetime_type(py, "datetime")?))
}

/// `datetime.date` or `datetime.datetime`.
fn datetime_type<'py>(py: Python<'py>, name: &str) -> PyResult<&'py Bound<'py, PyType>> {
    let cache = if name == "date" {
        &DATE_TYPE
    } else {
        &DATETIME_TYPE
    };
    Ok(cache
        .get_or_try_init(py, || -> PyResult<_> {
            Ok(py
                .import("datetime")?
                .getattr(name)?
                .cast_into::<PyType>()?
                .unbind())
        })?
        .bind(py))
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

// ── Fast-path for dumps() ────────────────────────────────────────────────────

/// Dump a Python object to a YAML string, bypassing the pyyaml stream protocol.
pub fn dumps_to_string(py: Python, obj: &Bound<'_, PyAny>) -> PyResult<String> {
    let mut ew = EmitterWrapper::new();
    ew.configure(Encoding::Utf8, None, true);

    let mut dumper = RSafeDumper {
        emitter: ew,
        closed: 0,
        document_start_implicit: true,
        version: None,
        tags: Vec::new(),
        document_end_implicit: true,
        serialized_nodes: HashSet::new(),
        anchors: HashMap::new(),
        last_alias_id: 0,
        default_style: None,
        default_flow_style: Some(false),
        sort_keys: false,
        represented_objects: HashMap::new(),
        nodes: Vec::new(),
        object_keeper: Vec::new(),
        level: 0,
        budget: RecursionBudget::UNLIMITED,
    };

    dumper
        .emitter
        .emit(Event::stream_start(Encoding::Utf8))
        .map_err(|e| exception::emitter_error(py, e))?;

    // As if through yaml.dump, which adds dump's and dump_all's frames.
    dumper.budget = RecursionBudget::current(py, 2)?;
    let node = dumper.represent_data(py, obj)?;
    dumper.serialize(py, node)?;

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
