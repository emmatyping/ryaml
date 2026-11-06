//! Full RSafeDumper implementation: emitter + serializer + SafeRepresenter + resolver.
//! All in Rust, matching the RSafeLoader pattern.

use std::collections::HashMap;

use rustc_hash::FxBuildHasher;

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
use crate::resolver::{self, Tag};

/// A represented node (PyYAML's `yaml.nodes`), kept in `RSafeDumper::nodes`.
/// Children are indices into it, so a collection can be registered before its
/// children are represented and recursive data refers back to it, as in
/// PyYAML's representer.
#[derive(Debug)]
enum RepNode {
    Scalar {
        tag: Tag,
        value: ScalarText,
        style: Option<char>,
    },
    Sequence {
        tag: Tag,
        value: Vec<NodeId>,
        flow_style: Option<bool>,
    },
    Mapping {
        tag: Tag,
        value: Vec<(NodeId, NodeId)>,
        flow_style: Option<bool>,
    },
}

type NodeId = usize;

/// A scalar's text.
#[derive(Debug)]
enum ScalarText {
    Static(&'static str),
    Owned(String),
    /// Short text (a number) stored in the node, saving an allocation.
    Inline(InlineText),
    /// A `str`, encoded as UTF-8 only when the scalar is written: like
    /// PyYAML's emitter, which raises the error for a lone surrogate then,
    /// after the whole document is represented. Its UTF-8 is cached in the
    /// object, so this avoids a copy.
    Str(Py<PyString>),
}

impl From<&'static str> for ScalarText {
    fn from(text: &'static str) -> Self {
        ScalarText::Static(text)
    }
}

impl From<String> for ScalarText {
    fn from(text: String) -> Self {
        ScalarText::Owned(text)
    }
}

impl ScalarText {
    fn text<'a>(&'a self, py: Python<'_>) -> PyResult<&'a str> {
        match self {
            ScalarText::Static(text) => Ok(text),
            ScalarText::Owned(text) => Ok(text),
            ScalarText::Inline(text) => Ok(text.as_str()),
            ScalarText::Str(text) => {
                let mut size: pyo3::ffi::Py_ssize_t = 0;
                // SAFETY: `text` is a str; with the GIL held, this returns its
                // UTF-8, cached in the object, which lives at least as long as
                // the reference in `self`.
                let data = unsafe { pyo3::ffi::PyUnicode_AsUTF8AndSize(text.as_ptr(), &mut size) };
                if data.is_null() {
                    return Err(PyErr::fetch(py));
                }
                // SAFETY: CPython returned `size` bytes of valid UTF-8.
                Ok(unsafe {
                    std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                        data.cast::<u8>(),
                        size as usize,
                    ))
                })
            }
        }
    }
}

/// Up to `InlineText::CAPACITY` bytes of ASCII text, kept in place.
#[derive(Clone, Copy, Debug)]
struct InlineText {
    len: u8,
    bytes: [u8; InlineText::CAPACITY],
}

impl InlineText {
    /// Enough for any formatted i64 or float, and small enough that a
    /// `ScalarText` is no bigger than with just a `String`.
    const CAPACITY: usize = 30;

    fn new() -> Self {
        Self {
            len: 0,
            bytes: [0; Self::CAPACITY],
        }
    }

    fn push_str(&mut self, text: &str) {
        let start = usize::from(self.len);
        self.bytes[start..start + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len() as u8;
    }

    fn as_str(&self) -> &str {
        // Only ASCII is pushed, from `&str`s.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("ASCII text")
    }
}

/// The serializer's record of a node (PyYAML's `anchors` and
/// `serialized_nodes`).
#[derive(Clone, Copy, Default)]
struct NodeState {
    anchor: Anchor,
    serialized: bool,
}

#[derive(Clone, Copy, Default)]
enum Anchor {
    #[default]
    Unseen,
    Seen,
    Named(u32),
}

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

    // Inlined so the event is not copied again on its way to the emitter.
    #[inline(always)]
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
    /// The emitter writes every tag (canonical output).
    canonical: bool,
    document_start_implicit: bool,
    /// The `version` and `tags` options (PyYAML's `use_version` and
    /// `use_tags`), turned into `%YAML` and `%TAG` directives for each document
    /// (see `directives`).
    version: Option<Py<PyAny>>,
    tags: Option<Py<PyAny>>,
    document_end_implicit: bool,

    // Serializer state (reset per document)
    /// Per node of the document being serialized: whether the anchor pass
    /// saw it (and its anchor number once seen twice), and whether it was
    /// written yet.
    node_states: Vec<NodeState>,
    last_alias_id: u32,

    // Representer config
    default_style: Option<char>,
    default_flow_style: Option<bool>,
    sort_keys: bool,

    // Representer state (reset per represent() call)
    represented_objects: HashMap<usize, NodeId, FxBuildHasher>,
    /// Nodes of the document being represented.
    nodes: Vec<RepNode>,
    object_keeper: Vec<Py<PyAny>>,
    /// Nesting depth of the value being represented (the root is 0).
    level: usize,
    /// Representing the document found an object again, so some node may be
    /// reached twice and need an anchor. Without, the anchor pass is skipped.
    has_aliases: bool,
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
/// formatting dates), well within the red zone even for the levels between
/// checks: checking the stack costs about as much as a scalar's level, so it
/// is checked only every `STACK_CHECK_LEVELS` levels.
fn with_stack<T>(depth: usize, f: impl FnOnce() -> T) -> T {
    const RED_ZONE: usize = 64 * 1024;
    const SEGMENT: usize = 1024 * 1024;
    const STACK_CHECK_LEVELS: usize = 8;
    if depth.is_multiple_of(STACK_CHECK_LEVELS) {
        stacker::maybe_grow(RED_ZONE, SEGMENT, f)
    } else {
        f()
    }
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
            canonical: canonical.0,
            document_start_implicit: !explicit_start.0,
            version,
            tags,
            document_end_implicit: !explicit_end.0,
            node_states: Vec::new(),
            last_alias_id: 0,
            default_style: default_style_char,
            default_flow_style: default_flow_style.map(|style| style.0),
            sort_keys: sort_keys.0,
            represented_objects: HashMap::with_hasher(FxBuildHasher),
            nodes: Vec::new(),
            object_keeper: Vec::new(),
            level: 0,
            has_aliases: false,
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
        self.has_aliases = false;
        let node = self.represent_data(py, data.bind(py))?;
        self.serialize(py, node)?;
        self.represented_objects.clear();
        for object in self.object_keeper.drain(..) {
            drop(object.into_bound(py));
        }
        self.clear_nodes(py);
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
        with_stack(self.level, || self.represent_data_inner(py, data))
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
                self.has_aliases = true;
                return Ok(node);
            }
            self.object_keeper.push(data.clone().unbind());
            Some(key)
        };

        // SafeRepresenter dispatches on the exact type only (it registers no
        // multi-representers), so subclasses and other types are refused.
        let ty = data.get_type();
        // Exact type checks (pointer comparisons), most common first.
        let node = if data.is_exact_instance_of::<PyString>() {
            self.represent_str(data)?
        } else if data.is_exact_instance_of::<PyInt>() {
            self.represent_int(data)?
        } else if data.is_exact_instance_of::<PyDict>() {
            self.represent_dict(py, data, alias_key)?
        } else if data.is_exact_instance_of::<PyList>() || data.is_exact_instance_of::<PyTuple>() {
            self.represent_list(py, data, alias_key)?
        } else if data.is_none() {
            self.represent_none()
        } else if data.is_exact_instance_of::<PyFloat>() {
            self.represent_float(data)?
        } else if data.is_exact_instance_of::<PyBool>() {
            self.represent_bool(data)?
        } else if data.is_exact_instance_of::<PyBytes>() {
            self.represent_binary(py, data)?
        } else if ty.is(datetime_type(py, "datetime")?) {
            self.represent_datetime(py, data)?
        } else if ty.is(datetime_type(py, "date")?) {
            self.represent_date(py, data)?
        } else if data.is_exact_instance_of::<PySet>() {
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
        self.make_scalar(Tag::Null, "null", None)
    }

    fn represent_bool(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let b: bool = data.extract()?;
        let value = if b { "true" } else { "false" };
        Ok(self.make_scalar(Tag::Bool, value, None))
    }

    fn represent_int(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        // str(data); ints that fit in an i64 are formatted here.
        let value = match data.extract::<i64>() {
            Ok(value) => {
                let mut text = InlineText::new();
                text.push_str(itoa::Buffer::new().format(value));
                ScalarText::Inline(text)
            }
            Err(_) => ScalarText::Owned(data.str()?.to_string()),
        };
        Ok(self.push_scalar(Tag::Int, value, None))
    }

    fn represent_float(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value = yaml_float(data.extract::<f64>()?);
        Ok(self.push_scalar(Tag::Float, value, None))
    }

    fn represent_str(&mut self, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value = ScalarText::Str(data.cast::<PyString>()?.clone().unbind());
        Ok(self.push_scalar(Tag::Str, value, None))
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

        Ok(self.make_scalar(Tag::Binary, result, Some('|')))
    }

    fn represent_date(&mut self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value: String = data.call_method0("isoformat")?.extract()?;
        Ok(self.make_scalar(Tag::Timestamp, value, None))
    }

    fn represent_datetime(&mut self, _py: Python, data: &Bound<'_, PyAny>) -> PyResult<NodeId> {
        let value: String = data.call_method1("isoformat", (" ",))?.extract()?;
        Ok(self.make_scalar(Tag::Timestamp, value, None))
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
                tag: Tag::Seq,
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
        self.represent_mapping(py, Tag::Map, data.cast::<PyDict>()?, alias_key)
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
        self.represent_mapping(py, Tag::Set, &value, alias_key)
    }

    /// PyYAML's `represent_mapping`, for dicts and sets.
    fn represent_mapping(
        &mut self,
        py: Python,
        tag: Tag,
        mapping: &Bound<'_, PyDict>,
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
        let items = self.mapping_items(py, mapping)?;
        let mut best_style = true;
        self.level += 1;
        for (k, v) in &items {
            let key_node = self.represent_data(py, k)?;
            let val_node = self.represent_data(py, v)?;
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

    /// PyYAML's `represent_mapping`: the (key, value) items, sorted like
    /// `sorted(mapping.items())` when `sort_keys` is set, or left in order if
    /// that raises `TypeError` (keys that cannot be compared). Keys that are
    /// all exact `str` are sorted here by their UTF-8 bytes, which is Python's
    /// code point order (dict keys are distinct, so values never compare).
    fn mapping_items<'py>(
        &self,
        py: Python<'py>,
        mapping: &Bound<'py, PyDict>,
    ) -> PyResult<Vec<(Bound<'py, PyAny>, Bound<'py, PyAny>)>> {
        let items: Vec<_> = mapping.iter().collect();
        if !self.sort_keys || items.len() < 2 {
            return Ok(items);
        }
        let mut order = Vec::with_capacity(items.len());
        for (index, (key, _)) in items.iter().enumerate() {
            let text = if key.is_exact_instance_of::<PyString>() {
                // Lone surrogates have no UTF-8: leave those to Python.
                key.cast::<PyString>()?.to_str().ok()
            } else {
                None
            };
            match text {
                Some(text) => order.push((text, index)),
                None => return self.sort_items_in_python(py, items),
            }
        }
        order.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let order: Vec<usize> = order.into_iter().map(|(_, index)| index).collect();
        let mut items: Vec<_> = items.into_iter().map(Some).collect();
        Ok(order
            .into_iter()
            .map(|index| items[index].take().expect("each index once"))
            .collect())
    }

    /// `sorted(items)` in Python, keeping the order on `TypeError`.
    fn sort_items_in_python<'py>(
        &self,
        py: Python<'py>,
        items: Vec<(Bound<'py, PyAny>, Bound<'py, PyAny>)>,
    ) -> PyResult<Vec<(Bound<'py, PyAny>, Bound<'py, PyAny>)>> {
        let list = PyList::new(py, items.iter().map(|(k, v)| (k, v)))?;
        match py.import("builtins")?.getattr("sorted")?.call1((&list,)) {
            Ok(sorted) => sorted
                .cast_into::<PyList>()?
                .iter()
                .map(|item| item.extract::<(Bound<'py, PyAny>, Bound<'py, PyAny>)>())
                .collect(),
            Err(e) if e.is_instance_of::<pyo3::exceptions::PyTypeError>(py) => Ok(items),
            Err(e) => Err(e),
        }
    }

    /// Add a collection node and record it as `alias_key`'s node before its
    /// items are represented, like PyYAML's `represent_sequence` and
    /// `represent_mapping`.
    /// Drop the document's nodes, releasing their strings through `Bound`
    /// (a direct decref) rather than pyo3's deferred `Py` drop.
    fn clear_nodes(&mut self, py: Python) {
        for node in self.nodes.drain(..) {
            if let RepNode::Scalar {
                value: ScalarText::Str(text),
                ..
            } = node
            {
                drop(text.into_bound(py));
            }
        }
    }

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

    fn make_scalar(
        &mut self,
        tag: Tag,
        value: impl Into<ScalarText>,
        style: Option<char>,
    ) -> NodeId {
        self.push_scalar(tag, value.into(), style)
    }

    fn push_scalar(&mut self, tag: Tag, value: ScalarText, style: Option<char>) -> NodeId {
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
    #[inline(always)]
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

        // Like CEmitter, the anchors and written nodes of a document that
        // failed are kept (they are reset only after a document succeeds), so
        // a later document reusing its nodes sees them; then the anchor pass
        // is needed even if nothing repeats in this one.
        let carried_over = !self.node_states.is_empty();
        self.node_states
            .resize(self.nodes.len(), NodeState::default());
        let anchored = self.has_aliases || carried_over;
        if anchored {
            self.anchor_node(py, node, 0)?;
        }
        let result = self
            .serialize_node(py, node, 0)
            .and_then(|()| self.emit(py, Event::document_end(self.document_end_implicit)));
        if let Err(error) = result {
            if !anchored {
                // The kept state must be what the skipped anchor pass would
                // have left: every node seen (none twice, so no anchors).
                self.anchor_node(py, node, 0)?;
            }
            return Err(error);
        }

        // Reset serializer state
        self.node_states.clear();
        self.last_alias_id = 0;
        Ok(())
    }

    fn anchor_node(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        with_stack(depth, || self.anchor_node_inner(py, node, depth))
    }

    fn anchor_node_inner(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        // Nodes of a document are at most MAX_DEPTH deep, but nodes kept from
        // a failed document can be reused deeper in a later one.
        check_depth(depth)?;
        match self.node_states[node].anchor {
            Anchor::Unseen => self.node_states[node].anchor = Anchor::Seen,
            Anchor::Seen => {
                // Reached twice: it gets the next anchor (PyYAML's id001, ...).
                self.last_alias_id += 1;
                self.node_states[node].anchor = Anchor::Named(self.last_alias_id);
                return Ok(());
            }
            Anchor::Named(_) => return Ok(()),
        }
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
        with_stack(depth, || self.serialize_node_inner(py, node, depth))
    }

    fn serialize_node_inner(&mut self, py: Python, node: NodeId, depth: usize) -> PyResult<()> {
        check_depth(depth)?;
        let anchor = match self.node_states[node].anchor {
            Anchor::Named(id) => Some(format!("id{id:03}")),
            _ => None,
        };

        if self.node_states[node].serialized {
            // Emit alias
            let anchor_str = anchor.as_deref().unwrap_or("");
            self.emit(py, Event::alias(anchor_str))?;
            return Ok(());
        }
        self.node_states[node].serialized = true;

        let anchor_ref = anchor.as_deref();

        match &self.nodes[node] {
            RepNode::Scalar { tag, value, style } => {
                let value = value.text(py)?;
                let plain_implicit = *tag == resolver::resolve_scalar(value, true);
                // A non-plain scalar always resolves to str.
                let quoted_implicit = *tag == Tag::Str;
                let scalar_style = char_to_scalar_style(*style);
                // The emitter only looks at the tag when it is canonical or
                // neither implicit flag is set, so skip building it otherwise.
                let tag =
                    (self.canonical || !plain_implicit && !quoted_implicit).then(|| tag.as_str());

                // Reuse the string of an already written scalar's event.
                let mut text = self.emitter.emitter_mut().take_spare_string();
                text.push_str(value);
                let event = Event::scalar(
                    anchor_ref,
                    tag,
                    text,
                    plain_implicit,
                    quoted_implicit,
                    scalar_style,
                );
                self.emit(py, event)?;
            }
            RepNode::Sequence {
                tag, flow_style, ..
            } => {
                let implicit = *tag == Tag::Seq;
                // CEmitter: flow if node.flow_style is true, else block.
                let style = if *flow_style == Some(true) {
                    SequenceStyle::Flow
                } else {
                    SequenceStyle::Block
                };
                // As for scalars, the tag only matters when it is written.
                let tag = (self.canonical || !implicit).then(|| tag.as_str());
                let event = Event::sequence_start(anchor_ref, tag, implicit, style);
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
                let implicit = *tag == Tag::Map;
                let style = if *flow_style == Some(true) {
                    MappingStyle::Flow
                } else {
                    MappingStyle::Block
                };
                let tag = (self.canonical || !implicit).then(|| tag.as_str());
                let event = Event::mapping_start(anchor_ref, tag, implicit, style);
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

/// PyYAML's `represent_float`: `.nan`, `.inf`, `-.inf`, or `repr(value)`
/// lowercased, with `.0` added before an exponent when there is no decimal
/// point (`1e+17` becomes `1.0e+17`).
fn yaml_float(value: f64) -> ScalarText {
    if value.is_nan() {
        return ScalarText::Static(".nan");
    }
    if value.is_infinite() {
        return ScalarText::Static(if value > 0.0 { ".inf" } else { "-.inf" });
    }
    let mut text = InlineText::new();
    write_yaml_float(zmij::Buffer::new().format_finite(value), &mut text);
    ScalarText::Inline(text)
}

/// Rewrite zmij's output for a finite float in PyYAML's notation. zmij picks
/// the same digits as Python's `repr(float)` (the shortest that round-trip,
/// the closest to the value, ties to even), and writes them the same way
/// except that it uses fixed notation for a decimal exponent of -5 (Python
/// starts at -4) and does not pad the exponent to two digits.
fn write_yaml_float(zmij: &str, out: &mut InlineText) {
    let unsigned = match zmij.strip_prefix('-') {
        Some(unsigned) => {
            out.push_str("-");
            unsigned
        }
        None => zmij,
    };
    let (mantissa, exponent) = if let Some((mantissa, exponent)) = unsigned.split_once('e') {
        (mantissa, exponent)
    } else if let Some(digits) = unsigned.strip_prefix("0.0000") {
        // 0.0000ddd is d.dde-05 in Python.
        let (first, rest) = digits.split_at(1);
        out.push_str(first);
        if !rest.is_empty() {
            out.push_str(".");
            out.push_str(rest);
        } else {
            out.push_str(".0");
        }
        out.push_str("e-05");
        return;
    } else {
        out.push_str(unsigned);
        return;
    };
    out.push_str(mantissa);
    if !mantissa.contains('.') {
        out.push_str(".0");
    }
    let (sign, digits) = exponent.split_at(1);
    out.push_str("e");
    out.push_str(sign);
    if digits.len() == 1 {
        out.push_str("0");
    }
    out.push_str(digits);
}

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
pub fn dumps_to_string<'py>(
    py: Python<'py>,
    obj: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyString>> {
    let mut ew = EmitterWrapper::new();
    ew.configure(Encoding::Utf8, None, true);

    let mut dumper = RSafeDumper {
        emitter: ew,
        closed: 0,
        canonical: false,
        document_start_implicit: true,
        version: None,
        tags: None,
        document_end_implicit: true,
        node_states: Vec::new(),
        last_alias_id: 0,
        default_style: None,
        default_flow_style: Some(false),
        sort_keys: false,
        represented_objects: HashMap::with_hasher(FxBuildHasher),
        nodes: Vec::new(),
        object_keeper: Vec::new(),
        level: 0,
        has_aliases: false,
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
    output_to_str(py, &output)
}

/// The emitter's output (UTF-8) as a `str`. ASCII, the usual case, is copied
/// straight into a new compact ASCII string, skipping decoding (except with
/// the limited API, which cannot make one).
fn output_to_str<'py>(py: Python<'py>, output: &[u8]) -> PyResult<Bound<'py, PyString>> {
    let len = pyo3::ffi::Py_ssize_t::try_from(output.len())
        .map_err(|_| pyo3::exceptions::PyOverflowError::new_err("output is too long"))?;
    if cfg!(feature = "abi3") || !output.is_ascii() {
        // SAFETY: `output` holds `len` bytes; CPython decodes and checks them.
        let object = unsafe {
            pyo3::ffi::PyUnicode_DecodeUTF8(output.as_ptr().cast(), len, std::ptr::null())
        };
        // SAFETY: a new reference or NULL with an exception set.
        return unsafe { Bound::from_owned_ptr_or_err(py, object) }.map(|object| {
            object
                .cast_into::<PyString>()
                .expect("PyUnicode_DecodeUTF8 returns a str")
        });
    }
    #[cfg(not(feature = "abi3"))]
    // SAFETY: PyUnicode_New(len, 127) makes a compact ASCII string with room
    // for `len` bytes (plus the NUL it writes), whose data we fill before
    // anyone else can see it.
    unsafe {
        let object = pyo3::ffi::PyUnicode_New(len, 127);
        let object = Bound::from_owned_ptr_or_err(py, object)?;
        let data = pyo3::ffi::PyUnicode_DATA(object.as_ptr()).cast::<u8>();
        std::ptr::copy_nonoverlapping(output.as_ptr(), data, output.len());
        Ok(object.cast_into_unchecked::<PyString>())
    }
    #[cfg(feature = "abi3")]
    unreachable!("the limited API has no PyUnicode_New")
}

pub fn register_dumper(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RSafeDumper>()?;
    Ok(())
}
