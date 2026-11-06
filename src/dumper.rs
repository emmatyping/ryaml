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
    /// The `version` and `tags` options (PyYAML's `use_version` and
    /// `use_tags`), turned into `%YAML` and `%TAG` directives for each document
    /// (see `directives`).
    version: Option<Py<PyAny>>,
    tags: Option<Py<PyAny>>,
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
    level: usize,
}

/// The deepest nesting ryaml represents and serializes; deeper data raises
/// `RecursionError`. The recursion is native and grows its stack as needed
/// (`with_stack`), so the limit is the same on every platform and Python
/// version. It is deeper than PyYAML goes with the default recursion limit
/// (about 330 levels, as its representer is recursive Python, three frames
/// per level).
const MAX_DEPTH: usize = 500;

/// Run one level of the dumper's recursion, on a new stack segment if the
/// current one is nearly used up, so `MAX_DEPTH` holds whatever the thread's
/// stack size (musl gives threads 128 KB, for example). A level needs about
/// half a kilobyte, plus whatever Python calls it makes (comparing keys,
/// formatting dates), well within the red zone.
fn with_stack<T>(f: impl FnOnce() -> T) -> T {
    const RED_ZONE: usize = 64 * 1024;
    const SEGMENT: usize = 1024 * 1024;
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}

fn check_depth(depth: usize) -> PyResult<()> {
    if depth > MAX_DEPTH {
        return Err(pyo3::exceptions::PyRecursionError::new_err(
            "maximum recursion depth exceeded while representing an object",
        ));
    }
    Ok(())
}

#[pymethods]
impl RSafeDumper {
    #[allow(clippy::too_many_arguments)]
    #[new]
    #[pyo3(signature = (stream, default_style=None, default_flow_style=Some(Truthy(false)),
        canonical=Truthy(false), indent=None, width=None, allow_unicode=Truthy(false),
        line_break=None, encoding=None, explicit_start=Truthy(false), explicit_end=Truthy(false),
        version=None, tags=None, sort_keys=Truthy(true)))]
    fn new(
        py: Python,
        stream: Py<PyAny>,
        default_style: Option<Bound<'_, PyAny>>,
        default_flow_style: Option<Truthy>,
        canonical: Truthy,
        indent: Option<CInt>,
        width: Option<CInt>,
        allow_unicode: Truthy,
        line_break: Option<Bound<'_, PyAny>>,
        encoding: Option<Bound<'_, PyAny>>,
        explicit_start: Truthy,
        explicit_end: Truthy,
        version: Option<Py<PyAny>>,
        tags: Option<Py<PyAny>>,
        sort_keys: Truthy,
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

        if canonical.0 {
            ew.emitter_mut().set_canonical(true);
        }
        if let Some(CInt(i)) = indent {
            ew.emitter_mut().set_indent(i);
        }
        if let Some(CInt(w)) = width {
            ew.emitter_mut().set_width(w);
        }
        if allow_unicode.0 {
            ew.emitter_mut().set_unicode(true);
        }
        // CEmitter only recognizes these line breaks and ignores anything else.
        let line_break = line_break.and_then(|lb| lb.extract::<String>().ok());
        let brk = match line_break.as_deref() {
            Some("\n") => Some(libyaml_safer::Break::Ln),
            Some("\r") => Some(libyaml_safer::Break::Cr),
            Some("\r\n") => Some(libyaml_safer::Break::CrLn),
            _ => None,
        };
        if let Some(brk) = brk {
            ew.emitter_mut().set_break(brk);
        }

        // The representer stores default_style on every scalar node, and
        // CEmitter writes only exactly "'", '"', '|' or '>' as that style.
        // Any other truthy value is written plain but still counts as a style
        // when choosing flow style.
        let default_style_char = match default_style {
            None => None,
            Some(style) => match style.extract::<String>().ok().as_deref() {
                Some("'") => Some('\''),
                Some("\"") => Some('"'),
                Some("|") => Some('|'),
                Some(">") => Some('>'),
                _ if style.is_truthy()? => Some(OTHER_STYLE),
                _ => None,
            },
        };

        Ok(RSafeDumper {
            emitter: ew,
            closed: -1,
            document_start_implicit: !explicit_start.0,
            version,
            tags,
            document_end_implicit: !explicit_end.0,
            serialized_nodes: HashSet::new(),
            anchors: HashMap::new(),
            last_alias_id: 0,
            default_style: default_style_char,
            default_flow_style: default_flow_style.map(|style| style.0),
            sort_keys: sort_keys.0,
            represented_objects: HashMap::new(),
            nodes: Vec::new(),
            object_keeper: Vec::new(),
            level: 0,
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
        with_stack(|| self.represent_data_inner(py, data))
    }

    fn represent_data_inner(&mut self, py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        check_depth(self.level)?;
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

    /// The `%YAML` and `%TAG` directives for a document, checked when it is
    /// serialized, as CEmitter does: a truthy `version` gives
    /// `version[0]`.`version[1]`; a truthy `tags` mapping (at most 128) gives
    /// each handle and its prefix, which must be str or bytes.
    fn directives(&self, py: Python) -> PyResult<(Option<VersionDirective>, Vec<TagDirective>)> {
        let mut version = None;
        if let Some(use_version) = &self.version {
            let use_version = use_version.bind(py);
            if use_version.is_truthy()? {
                version = Some(VersionDirective {
                    major: use_version.get_item(0)?.extract()?,
                    minor: use_version.get_item(1)?.extract()?,
                });
            }
        }
        let mut tags = Vec::new();
        if let Some(use_tags) = &self.tags {
            let use_tags = use_tags.bind(py);
            if use_tags.is_truthy()? {
                if use_tags.len()? > 128 {
                    return Err(pyo3::exceptions::PyValueError::new_err("too many tags"));
                }
                for handle in use_tags.try_iter()? {
                    let handle = handle?;
                    let prefix = use_tags.get_item(&handle)?;
                    tags.push(TagDirective {
                        handle: tag_part(&handle, "handle")?,
                        prefix: tag_part(&prefix, "prefix")?,
                    });
                }
            }
        }
        Ok((version, tags))
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
        let (version, tags) = self.directives(py)?;
        self.emit(
            py,
            Event::document_start(version, &tags, self.document_start_implicit),
        )?;

        self.anchor_node(py, node, 0)?;
        self.serialize_node(py, node, 0)?;

        self.emit(py, Event::document_end(self.document_end_implicit))?;

        // Reset serializer state
        self.serialized_nodes.clear();
        self.anchors.clear();
        self.last_alias_id = 0;
        Ok(())
    }

    fn anchor_node(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        with_stack(|| self.anchor_node_inner(py, node, depth))
    }

    fn anchor_node_inner(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        // Nodes of a document are at most MAX_DEPTH deep, but nodes kept from
        // a failed document can be reused deeper in a later one.
        check_depth(depth)?;
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
                    self.anchor_node(py, item, depth + 1)?;
                }
                RepNode::Mapping { value, .. } if index < value.len() => {
                    let (k, v) = value[index];
                    self.anchor_node(py, k, depth + 1)?;
                    self.anchor_node(py, v, depth + 1)?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn serialize_node(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        with_stack(|| self.serialize_node_inner(py, node, depth))
    }

    fn serialize_node_inner(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        check_depth(depth)?;
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
                    self.serialize_node(py, item, depth + 1)?;
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
                    self.serialize_node(py, k, depth + 1)?;
                    self.serialize_node(py, v, depth + 1)?;
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

/// A tag handle or prefix: CEmitter takes exactly a str (encoded as UTF-8)
/// or bytes.
fn tag_part(value: &Bound<'_, PyAny>, what: &str) -> PyResult<String> {
    if value.is_exact_instance_of::<PyString>() {
        return value.extract();
    }
    if value.is_exact_instance_of::<PyBytes>() {
        return Ok(std::str::from_utf8(value.cast::<PyBytes>()?.as_bytes())?.to_owned());
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "tag {what} must be a string"
    )))
}

/// An option CEmitter passes to libyaml as a C int: like `int(value)` for
/// numbers (floats and decimals truncate), but str and bytes are refused.
struct CInt(i32);

impl<'py> FromPyObject<'py> for CInt {
    fn extract_bound(value: &Bound<'py, PyAny>) -> PyResult<Self> {
        let ty = value.get_type();
        if !ty.hasattr("__index__")? && !ty.hasattr("__int__")? {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "an integer is required",
            ));
        }
        let int = value.py().get_type::<PyInt>().call1((value,))?;
        Ok(CInt(int.extract()?))
    }
}

/// A PyYAML option used for its truthiness (`if canonical:`).
struct Truthy(bool);

impl<'py> FromPyObject<'py> for Truthy {
    fn extract_bound(value: &Bound<'py, PyAny>) -> PyResult<Self> {
        Ok(Truthy(value.is_truthy()?))
    }
}

/// A `default_style` PyYAML keeps on scalar nodes but CEmitter writes plain.
const OTHER_STYLE: char = '\0';

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
        tags: None,
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
    };

    dumper
        .emitter
        .emit(Event::stream_start(Encoding::Utf8))
        .map_err(|e| exception::emitter_error(py, e))?;

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
