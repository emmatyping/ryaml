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
    PyBool, PyBytes, PyDict, PyFloat, PyInt, PyIterator, PyList, PySet, PyString, PyTuple, PyType,
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
    /// A PyYAML node object: returned by a custom representer, or a node made
    /// into one for Python code (see `Representer::export`). It is read when
    /// the document is serialized (see `Representer::read_python_structure`).
    Python(Py<PyAny>),
}

type NodeId = usize;

/// Release a node's Python objects through `Bound` (a direct decref).
#[inline(always)]
fn drop_node(py: Python, node: RepNode) {
    match node {
        RepNode::Scalar {
            value: ScalarText::Str(text),
            ..
        } => drop(text.into_bound(py)),
        RepNode::Python(object) => drop(object.into_bound(py)),
        _ => {}
    }
}

/// The items of a node being made into a PyYAML node object.
enum ExportItems {
    Sequence(Vec<NodeId>),
    Mapping(Vec<(NodeId, NodeId)>),
}

/// A collection made into a PyYAML node object: its (still empty) value
/// list, and the items to add to it.
type PendingItems<'py> = (Bound<'py, PyList>, ExportItems);

/// The tag of a node object whose tag is `None`: CEmitter writes no tag.
/// Tags read from node objects end at a NUL, so none is this.
const NO_TAG: &str = "\0";

/// A node object's tag, read like CEmitter: `None` gives no tag (`NO_TAG`);
/// a `str` or `bytes` ends at a NUL, as a C string, and the emitter refuses
/// an empty one. A `bytes` tag is never implicit, as it never equals the
/// resolved tag.
fn python_tag(tag: &Bound<'_, PyAny>) -> PyResult<Tag> {
    let py = tag.py();
    if tag.is_none() {
        return Ok(Tag::Other(NO_TAG.to_owned()));
    }
    let (text, from_str) = if tag.is_exact_instance_of::<PyString>() {
        (tag.cast::<PyString>()?.to_str()?, true)
    } else if tag.is_exact_instance_of::<PyBytes>() {
        let bytes = tag.cast::<PyBytes>()?.as_bytes();
        let text = std::str::from_utf8(bytes)
            .map_err(|e| pyo3::exceptions::PyUnicodeDecodeError::new_err_from_utf8(py, bytes, e))?;
        (text, false)
    } else {
        return Err(pyo3::exceptions::PyTypeError::new_err(
            "tag must be a string",
        ));
    };
    Ok(match text.find('\0') {
        Some(end) => Tag::Other(text[..end].to_owned()),
        None if from_str => Tag::from_string(text.to_owned()),
        None => Tag::Other(text.to_owned()),
    })
}

/// `key, value = item`, as Python unpacks an item of a mapping node's value.
fn unpack_pair<'py>(item: &Bound<'py, PyAny>) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
    if let Ok(pair) = item.cast_exact::<PyTuple>()
        && pair.len() == 2
    {
        return Ok((pair.get_item(0)?, pair.get_item(1)?));
    }
    let Ok(mut items) = item.try_iter() else {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "cannot unpack non-iterable {} object",
            item.get_type().name()?
        )));
    };
    let mut next = |got| {
        items.next().transpose()?.ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "not enough values to unpack (expected 2, got {got})"
            ))
        })
    };
    let key = next(0)?;
    let value = next(1)?;
    if items.next().transpose()?.is_some() {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "too many values to unpack (expected 2)",
        ));
    }
    Ok((key, value))
}

type MappingItem<'py> = (Bound<'py, PyAny>, Bound<'py, PyAny>);

enum CollectionItems<'py> {
    Sequence(Bound<'py, PyIterator>),
    Mapping {
        items: std::vec::IntoIter<MappingItem<'py>>,
        pending_value: Option<Bound<'py, PyAny>>,
        represented_key: Option<NodeId>,
    },
}

struct CollectionFrame<'py> {
    node: NodeId,
    items: CollectionItems<'py>,
    best_style: bool,
}

impl<'py> CollectionFrame<'py> {
    fn new(node: NodeId, items: CollectionItems<'py>) -> Self {
        Self {
            node,
            items,
            best_style: true,
        }
    }

    fn next(&mut self) -> PyResult<Option<Bound<'py, PyAny>>> {
        match &mut self.items {
            CollectionItems::Sequence(items) => items.next().transpose(),
            CollectionItems::Mapping {
                items,
                pending_value,
                ..
            } => {
                if let Some(value) = pending_value.take() {
                    return Ok(Some(value));
                }
                Ok(items.next().map(|(key, value)| {
                    *pending_value = Some(value);
                    key
                }))
            }
        }
    }
}

enum NodeVisit {
    Enter(NodeId, usize),
    Leave(NodeId),
}

/// Walk one child at a time so traversal space depends on nesting depth.
/// A collection `Representer::read_python_structure` is going through.
enum StructureFrame<'py> {
    /// A node object, whose items are read from `items`.
    Python {
        id: NodeId,
        object: Py<PyAny>,
        items: Bound<'py, PyIterator>,
        read: ReadItems,
    },
    /// A node made in Rust, whose items can be node objects.
    Rust { id: NodeId, index: usize },
}

/// The items of a node object read so far; for a mapping, also the value
/// to go through after the key just gone through.
enum ReadItems {
    Sequence(Vec<NodeId>),
    Mapping(Vec<(NodeId, NodeId)>, Option<NodeId>),
}

struct NodeWalk {
    parents: Vec<(NodeId, usize)>,
    next_node: Option<NodeId>,
}

impl NodeWalk {
    fn new(root: NodeId) -> Self {
        Self {
            parents: Vec::new(),
            next_node: Some(root),
        }
    }

    fn next(&mut self, nodes: &[RepNode]) -> Option<NodeVisit> {
        if let Some(node) = self.next_node.take() {
            return Some(NodeVisit::Enter(node, self.parents.len()));
        }
        let (node, index) = self.parents.last_mut()?;
        let child = match &nodes[*node] {
            RepNode::Sequence { value, .. } => value.get(*index).copied(),
            RepNode::Mapping { value, .. } => value
                .get(*index / 2)
                .map(|&(key, val)| if index.is_multiple_of(2) { key } else { val }),
            RepNode::Scalar { .. } | RepNode::Python(_) => unreachable!(),
        };
        if let Some(child) = child {
            *index += 1;
            Some(NodeVisit::Enter(child, self.parents.len()))
        } else {
            let node = *node;
            self.parents.pop();
            Some(NodeVisit::Leave(node))
        }
    }

    fn descend(&mut self, node: NodeId) {
        self.parents.push((node, 0));
    }
}

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
    /// A node object's `bytes` value (as UTF-8), which, as in CEmitter, no
    /// implicit resolver matches.
    Bytes(String),
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
    #[inline(always)]
    fn text<'a>(&'a self, py: Python<'_>) -> PyResult<&'a str> {
        match self {
            ScalarText::Static(text) => Ok(text),
            ScalarText::Owned(text) => Ok(text),
            ScalarText::Inline(text) => Ok(text.as_str()),
            ScalarText::Bytes(text) => Ok(text),
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
            // Dumping aliased strings can make huge output; raise MemoryError
            // like PyYAML rather than abort when it does not fit.
            if self.output.try_reserve(buf.len()).is_err() {
                self.error = Some(pyo3::exceptions::PyMemoryError::new_err(()));
                return Err(std::io::Error::other("out of memory"));
            }
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

    /// The representer, with the nodes of the document. Rust code moves it
    /// out while it represents data, and moves it back while a custom
    /// representer runs, so that the callback can use the dumper.
    rep: Representer,
    /// `rep` is moved out.
    rep_busy: bool,
    /// A document is being represented and serialized (`represent` or
    /// `serialize`), which a custom representer cannot start again.
    in_document: bool,
}

/// PyYAML's SafeRepresenter: its options, and the state of one document.
#[derive(Default)]
struct Representer {
    default_style: Option<char>,
    /// The `default_style` option itself: the style of a Python node made
    /// from a scalar with `OTHER_STYLE`.
    default_style_object: Option<Py<PyAny>>,
    default_flow_style: Option<bool>,
    sort_keys: bool,

    // State, reset after each document
    represented_objects: HashMap<usize, NodeId, FxBuildHasher>,
    /// Nodes of the document being represented.
    nodes: Vec<RepNode>,
    object_keeper: Vec<Py<PyAny>>,
    /// Representing the document found an object again, so some node may be
    /// reached twice and need an anchor. Without, the anchor pass is skipped.
    has_aliases: bool,
    /// PyYAML's `alias_key`: the id of the object `represent_data` last
    /// started on, or `None` for one that is never aliased.
    alias_key: Option<usize>,
    /// The node of each PyYAML node object in `nodes`, by identity. The
    /// objects are kept in `python_keeper` so that their ids stay unique.
    python_ids: HashMap<usize, NodeId, FxBuildHasher>,
    python_keeper: Vec<Py<PyAny>>,
    /// Node objects whose structure is read but whose tag, value and styles
    /// are read when they are serialized.
    python_unread: HashMap<NodeId, Py<PyAny>, FxBuildHasher>,
    /// The errors of the nodes standing where a node object's items could
    /// not be read again, raised when they are serialized (see
    /// `reread_items`).
    python_errors: HashMap<NodeId, PyErr, FxBuildHasher>,
    /// The custom representers of the dumper's class, found for each document.
    custom: Option<Custom>,
}

/// The built-in types SafeRepresenter represents, which Rust represents
/// unless a custom representer replaces SafeRepresenter's.
#[derive(Clone, Copy)]
enum Kind {
    Str,
    Int,
    Dict,
    List,
    Tuple,
    None,
    Float,
    Bool,
    Bytes,
    Datetime,
    Date,
    Set,
}

impl Kind {
    const ALL: [Kind; 12] = [
        Kind::Str,
        Kind::Int,
        Kind::Dict,
        Kind::List,
        Kind::Tuple,
        Kind::None,
        Kind::Float,
        Kind::Bool,
        Kind::Bytes,
        Kind::Datetime,
        Kind::Date,
        Kind::Set,
    ];

    fn bit(self) -> u16 {
        1 << self as u16
    }

    fn type_object(self, py: Python<'_>) -> PyResult<Bound<'_, PyType>> {
        Ok(match self {
            Kind::Str => py.get_type::<PyString>(),
            Kind::Int => py.get_type::<PyInt>(),
            Kind::Dict => py.get_type::<PyDict>(),
            Kind::List => py.get_type::<PyList>(),
            Kind::Tuple => py.get_type::<PyTuple>(),
            Kind::None => py.None().into_bound(py).get_type(),
            Kind::Float => py.get_type::<PyFloat>(),
            Kind::Bool => py.get_type::<PyBool>(),
            Kind::Bytes => py.get_type::<PyBytes>(),
            Kind::Datetime => datetime_type(py, "datetime")?.clone(),
            Kind::Date => datetime_type(py, "date")?.clone(),
            Kind::Set => py.get_type::<PySet>(),
        })
    }
}

/// The representer registries of a dumper class that has custom ones
/// (`yaml_representers` and `yaml_multi_representers`, as in PyYAML).
struct Custom {
    representers: Py<PyAny>,
    multi_representers: Py<PyAny>,
    /// The kinds (`Kind::bit`) still represented by SafeRepresenter's own
    /// functions, which Rust represents instead.
    native: u16,
}

/// `ryaml.compat`'s defaults, which RSafeDumper and its subclasses share
/// until a representer is registered.
static DEFAULT_REGISTRIES: PyOnceLock<Option<(Py<PyAny>, Py<PyAny>)>> = PyOnceLock::new();

impl Custom {
    /// The custom registries of `dumper`'s class, or `None` if it only has
    /// the defaults, which Rust implements.
    fn for_dumper(dumper: &Bound<'_, RSafeDumper>) -> PyResult<Option<Custom>> {
        let py = dumper.py();
        let ty = dumper.get_type();
        let (Ok(representers), Ok(multi_representers)) = (
            ty.getattr(pyo3::intern!(py, "yaml_representers")),
            ty.getattr(pyo3::intern!(py, "yaml_multi_representers")),
        ) else {
            return Ok(None);
        };
        let defaults = DEFAULT_REGISTRIES.get_or_try_init(py, || -> PyResult<_> {
            let Ok(compat) = py.import("ryaml.compat") else {
                return Ok(None);
            };
            let base = compat.getattr("_SafeRepresenter")?;
            Ok(Some((
                base.getattr("yaml_representers")?.unbind(),
                base.getattr("yaml_multi_representers")?.unbind(),
            )))
        })?;
        let Some((default_representers, default_multi)) = defaults else {
            return Ok(None);
        };
        let default_representers = default_representers.bind(py);
        if representers.is(default_representers) && multi_representers.is(default_multi.bind(py)) {
            return Ok(None);
        }
        // PyYAML looks up the exact type first, so a built-in type keeps
        // SafeRepresenter's representation exactly when its function does.
        let mut native = 0;
        for kind in Kind::ALL {
            let ty = kind.type_object(py)?;
            let current = mapping_get(&representers, &ty)?;
            let default = mapping_get(default_representers, &ty)?;
            let same = match (current, default) {
                (Some(current), Some(default)) => current.is(&default),
                (None, None) => true,
                _ => false,
            };
            if same {
                native |= kind.bit();
            }
        }
        Ok(Some(Custom {
            representers: representers.unbind(),
            multi_representers: multi_representers.unbind(),
            native,
        }))
    }

    fn bind<'a, 'py>(&self, dumper: &'a Bound<'py, RSafeDumper>) -> Ctx<'a, 'py> {
        let py = dumper.py();
        Ctx {
            dumper,
            representers: self.representers.bind(py).clone(),
            multi_representers: self.multi_representers.bind(py).clone(),
            native: self.native,
        }
    }
}

/// What representing data with custom representers needs: the dumper the
/// callbacks are given, and its class's registries.
struct Ctx<'a, 'py> {
    dumper: &'a Bound<'py, RSafeDumper>,
    representers: Bound<'py, PyAny>,
    multi_representers: Bound<'py, PyAny>,
    native: u16,
}

impl<'py> Ctx<'_, 'py> {
    /// PyYAML's `represent_data` lookup: the exact type's representer, then
    /// the first multi-representer along the MRO, then a multi-representer
    /// for `None`, then a representer for `None`.
    fn find(&self, data: &Bound<'py, PyAny>) -> PyResult<Option<Bound<'py, PyAny>>> {
        let py = data.py();
        let ty = data.get_type();
        if let Some(representer) = mapping_get(&self.representers, &ty)? {
            return Ok(Some(representer));
        }
        for base in ty.getattr(pyo3::intern!(py, "__mro__"))?.try_iter()? {
            if let Some(representer) = mapping_get(&self.multi_representers, &base?)? {
                return Ok(Some(representer));
            }
        }
        let none = py.None().into_bound(py);
        if let Some(representer) = mapping_get(&self.multi_representers, &none)? {
            return Ok(Some(representer));
        }
        mapping_get(&self.representers, &none)
    }
}

/// `mapping[key]`, or `None` for a `KeyError`.
fn mapping_get<'py>(
    mapping: &Bound<'py, PyAny>,
    key: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    match mapping.get_item(key) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_instance_of::<pyo3::exceptions::PyKeyError>(mapping.py()) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// PyYAML's node classes.
struct NodeClasses {
    scalar: Py<PyType>,
    sequence: Py<PyType>,
    mapping: Py<PyType>,
}

static NODE_CLASSES: PyOnceLock<NodeClasses> = PyOnceLock::new();

fn node_classes(py: Python<'_>) -> PyResult<&NodeClasses> {
    NODE_CLASSES.get_or_try_init(py, || {
        let nodes = py.import("yaml.nodes")?;
        let class = |name| -> PyResult<Py<PyType>> {
            Ok(nodes.getattr(name)?.cast_into::<PyType>()?.unbind())
        };
        Ok(NodeClasses {
            scalar: class("ScalarNode")?,
            sequence: class("SequenceNode")?,
            mapping: class("MappingNode")?,
        })
    })
}

fn busy_error() -> PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(
        "the dumper cannot be used while it represents or serializes a document",
    )
}

/// Move the representer out of the dumper, for Rust code to use.
fn take_rep(dumper: &Bound<'_, RSafeDumper>) -> PyResult<Representer> {
    let mut dumper = dumper.try_borrow_mut()?;
    if dumper.rep_busy {
        return Err(busy_error());
    }
    dumper.rep_busy = true;
    Ok(std::mem::take(&mut dumper.rep))
}

fn put_rep(dumper: &Bound<'_, RSafeDumper>, rep: Representer) {
    let mut dumper = dumper.borrow_mut();
    dumper.rep = rep;
    dumper.rep_busy = false;
}

/// PyYAML's `represented_objects`, as seen from Python: object ids mapped to
/// the nodes representing them.
#[pyclass(name = "_RepresentedObjects")]
struct RepresentedObjects {
    dumper: Py<RSafeDumper>,
}

impl RepresentedObjects {
    fn dumper<'py>(&self, py: Python<'py>) -> PyResult<PyRefMut<'py, RSafeDumper>> {
        let dumper = self.dumper.bind(py).try_borrow_mut()?;
        if dumper.rep_busy {
            return Err(busy_error());
        }
        Ok(dumper)
    }
}

#[pymethods]
impl RepresentedObjects {
    fn __setitem__(&self, py: Python, key: usize, node: Bound<'_, PyAny>) -> PyResult<()> {
        let mut dumper = self.dumper(py)?;
        let node = dumper.rep.bind_python(node);
        dumper.rep.represented_objects.insert(key, node);
        Ok(())
    }

    fn __getitem__<'py>(&self, py: Python<'py>, key: usize) -> PyResult<Bound<'py, PyAny>> {
        let mut dumper = self.dumper(py)?;
        let Some(&node) = dumper.rep.represented_objects.get(&key) else {
            return Err(pyo3::exceptions::PyKeyError::new_err(key));
        };
        dumper.rep.export(py, node)
    }

    fn __delitem__(&self, py: Python, key: usize) -> PyResult<()> {
        if self
            .dumper(py)?
            .rep
            .represented_objects
            .remove(&key)
            .is_none()
        {
            return Err(pyo3::exceptions::PyKeyError::new_err(key));
        }
        Ok(())
    }

    fn __contains__(&self, py: Python, key: Bound<'_, PyAny>) -> PyResult<bool> {
        let Ok(key) = key.extract::<usize>() else {
            return Ok(false);
        };
        Ok(self.dumper(py)?.rep.represented_objects.contains_key(&key))
    }

    fn __len__(&self, py: Python) -> PyResult<usize> {
        Ok(self.dumper(py)?.rep.represented_objects.len())
    }
}

/// The deepest nesting ryaml represents and serializes; deeper data raises
/// `RecursionError`. Traversal uses explicit stacks so the limit is independent
/// of the calling thread's native stack and Python's recursion limit.
const MAX_DEPTH: usize = 500;

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
        let (default_style, default_style_object) = match default_style {
            Some(style) => (scalar_style(&style)?, Some(style.unbind())),
            None => (None, None),
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
            rep: Representer {
                default_style,
                default_style_object,
                default_flow_style: default_flow_style.map(|style| style.0),
                sort_keys: sort_keys.0,
                ..Representer::default()
            },
            rep_busy: false,
            in_document: false,
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

    fn represent(slf: &Bound<'_, Self>, data: Bound<'_, PyAny>) -> PyResult<()> {
        let py = slf.py();
        let custom = Custom::for_dumper(slf)?;
        {
            let mut dumper = slf.try_borrow_mut()?;
            if dumper.in_document || dumper.rep_busy {
                return Err(busy_error());
            }
            dumper.in_document = true;
        }
        let result = (|| {
            let mut rep = take_rep(slf)?;
            rep.custom = custom;
            rep.has_aliases = false;
            let ctx = rep.custom.as_ref().map(|custom| custom.bind(slf));
            let result = rep.represent_data(py, &data, ctx.as_ref());
            put_rep(slf, rep);
            let node = result?;
            let mut dumper = slf.borrow_mut();
            dumper.serialize(py, node)?;
            dumper.rep.clear_representation(py);
            Ok(())
        })();
        slf.borrow_mut().in_document = false;
        result
    }

    /// Serialize a node graph made of PyYAML's node objects.
    #[pyo3(name = "serialize")]
    fn serialize_python_node(slf: &Bound<'_, Self>, node: Bound<'_, PyAny>) -> PyResult<()> {
        let py = slf.py();
        let mut dumper = slf.try_borrow_mut()?;
        if dumper.in_document || dumper.rep_busy {
            return Err(busy_error());
        }
        let root = dumper.rep.bind_python(node);
        dumper.serialize(py, root)?;
        dumper.rep.clear_representation(py);
        Ok(())
    }

    /// PyYAML's `represent_data`, for custom representers: the node for
    /// `data`, as a PyYAML node object.
    fn represent_data<'py>(
        slf: &Bound<'py, Self>,
        data: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        // Inside `represent`, the registries were found for the document.
        let custom = if slf.try_borrow()?.in_document {
            None
        } else {
            Some(Custom::for_dumper(slf)?)
        };
        let mut rep = take_rep(slf)?;
        if let Some(custom) = custom {
            rep.custom = custom;
        }
        let ctx = rep.custom.as_ref().map(|custom| custom.bind(slf));
        let result = rep
            .represent_data(py, &data, ctx.as_ref())
            .and_then(|node| rep.export(py, node));
        put_rep(slf, rep);
        result
    }

    #[getter]
    fn get_alias_key(&self) -> Option<usize> {
        self.rep.alias_key
    }

    #[setter]
    fn set_alias_key(&mut self, alias_key: Option<usize>) {
        self.rep.alias_key = alias_key;
    }

    #[getter]
    fn get_represented_objects(slf: &Bound<'_, Self>) -> RepresentedObjects {
        RepresentedObjects {
            dumper: slf.clone().unbind(),
        }
    }

    /// Replace the represented objects, as `BaseRepresenter` does with `{}`.
    #[setter]
    fn set_represented_objects(&mut self, objects: Bound<'_, PyAny>) -> PyResult<()> {
        if self.rep_busy {
            return Err(busy_error());
        }
        self.rep.represented_objects.clear();
        for item in objects.call_method0("items")?.try_iter()? {
            let (key, node): (usize, Bound<'_, PyAny>) = item?.extract()?;
            let node = self.rep.bind_python(node);
            self.rep.represented_objects.insert(key, node);
        }
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

impl Representer {
    fn clear_representation(&mut self, py: Python) {
        self.represented_objects.clear();
        for object in self.object_keeper.drain(..) {
            drop(object.into_bound(py));
        }
        self.python_ids.clear();
        for (_, node) in self.python_unread.drain() {
            drop(node.into_bound(py));
        }
        self.python_errors.clear();
        for node in self.python_keeper.drain(..) {
            drop(node.into_bound(py));
        }
        self.alias_key = None;
        self.clear_nodes(py);
    }

    fn ignore_aliases(&self, _py: Python, data: &Bound<'_, PyAny>) -> bool {
        data.is_none()
            || (data.is_instance_of::<PyTuple>() && data.len().is_ok_and(|l| l == 0))
            || data.is_instance_of::<PyString>()
            || data.is_instance_of::<PyBytes>()
            || data.is_instance_of::<PyBool>()
            || data.is_instance_of::<PyInt>()
            || data.is_instance_of::<PyFloat>()
    }

    /// Represent `data`. With `ctx`, the dumper's class has custom
    /// representers, called for the types they handle.
    fn represent_data(
        &mut self,
        py: Python,
        data: &Bound<'_, PyAny>,
        ctx: Option<&Ctx<'_, '_>>,
    ) -> PyResult<NodeId> {
        let (root, frame) = self.represent_node(py, data, 0, ctx)?;
        let Some(frame) = frame else {
            return Ok(root);
        };
        let mut stack = vec![frame];
        while let Some(frame) = stack.last_mut() {
            if let Some(data) = frame.next()? {
                let (node, child) = self.represent_node(py, &data, stack.len(), ctx)?;
                if let Some(child) = child {
                    stack.push(child);
                } else {
                    self.append_child(py, stack.last_mut().unwrap(), node)?;
                }
            } else {
                let node = frame.node;
                let flow = self.choose_flow_style(frame.best_style);
                match &mut self.nodes[node] {
                    RepNode::Sequence { flow_style, .. } | RepNode::Mapping { flow_style, .. } => {
                        *flow_style = flow;
                    }
                    // Made into a PyYAML node while its items were represented.
                    RepNode::Python(object) => {
                        object
                            .bind(py)
                            .setattr(pyo3::intern!(py, "flow_style"), flow)?;
                    }
                    RepNode::Scalar { .. } => unreachable!(),
                }
                stack.pop();
                if let Some(parent) = stack.last_mut() {
                    self.append_child(py, parent, node)?;
                }
            }
        }
        Ok(root)
    }

    #[inline(always)]
    fn append_child(
        &mut self,
        py: Python,
        frame: &mut CollectionFrame<'_>,
        child: NodeId,
    ) -> PyResult<()> {
        frame.best_style &= match &self.nodes[child] {
            RepNode::Scalar { style, .. } => style.is_none(),
            RepNode::Python(_) => self.is_plain_scalar(py, child)?,
            _ => false,
        };
        // Add completed items immediately so a failed representation retains
        // the parts of a registered collection that were already represented.
        match (&mut self.nodes[frame.node], &mut frame.items) {
            (RepNode::Sequence { value, .. }, CollectionItems::Sequence(_)) => value.push(child),
            (
                RepNode::Mapping { value, .. },
                CollectionItems::Mapping {
                    represented_key, ..
                },
            ) => {
                if let Some(key) = represented_key.take() {
                    value.push((key, child));
                } else {
                    *represented_key = Some(child);
                }
            }
            (RepNode::Python(_), _) => return self.append_python_child(py, frame, child),
            _ => unreachable!(),
        }
        Ok(())
    }

    /// Add an item to a collection that was given to a custom representer as
    /// a PyYAML node, as PyYAML would: to that node object.
    #[cold]
    #[inline(never)]
    fn append_python_child(
        &mut self,
        py: Python,
        frame: &mut CollectionFrame<'_>,
        child: NodeId,
    ) -> PyResult<()> {
        let RepNode::Python(object) = &self.nodes[frame.node] else {
            unreachable!()
        };
        let value = object.bind(py).getattr(pyo3::intern!(py, "value"))?;
        match &mut frame.items {
            CollectionItems::Sequence(_) => {
                value.call_method1(pyo3::intern!(py, "append"), (self.export(py, child)?,))?;
            }
            CollectionItems::Mapping {
                represented_key, ..
            } => {
                if let Some(key) = represented_key.take() {
                    let item = (self.export(py, key)?, self.export(py, child)?);
                    value.call_method1(pyo3::intern!(py, "append"), (item,))?;
                } else {
                    *represented_key = Some(child);
                }
            }
        }
        Ok(())
    }

    fn represent_node<'py>(
        &mut self,
        py: Python<'py>,
        data: &Bound<'py, PyAny>,
        depth: usize,
        ctx: Option<&Ctx<'_, 'py>>,
    ) -> PyResult<(NodeId, Option<CollectionFrame<'py>>)> {
        check_depth(depth)?;
        // Alias tracking. Collections register their node before representing
        // their items (see `register`), so recursive data finds it here.
        let alias_key = if self.ignore_aliases(py, data) {
            None
        } else {
            Some(data.as_ptr() as usize)
        };
        self.alias_key = alias_key;
        if let Some(key) = alias_key {
            if let Some(&node) = self.represented_objects.get(&key) {
                self.has_aliases = true;
                return Ok((node, None));
            }
            self.object_keeper.push(data.clone().unbind());
        }

        // SafeRepresenter dispatches on the exact type only (it registers no
        // multi-representers), so subclasses and other types are refused,
        // unless the dumper's class has representers for them.
        // Each built-in type is represented here unless a custom representer
        // replaces SafeRepresenter's for it (`ctx` is `None` without any).
        macro_rules! native {
            ($kind:expr) => {
                if ctx.is_some_and(|ctx| ctx.native & $kind.bit() == 0) {
                    return Ok((self.represent_other(data, ctx)?, None));
                }
            };
        }
        // Exact type checks (pointer comparisons), most common first.
        let node = if data.is_exact_instance_of::<PyString>() {
            native!(Kind::Str);
            self.represent_str(data)?
        } else if data.is_exact_instance_of::<PyInt>() {
            native!(Kind::Int);
            self.represent_int(data)?
        } else if data.is_exact_instance_of::<PyDict>() {
            native!(Kind::Dict);
            return self.represent_mapping(py, Tag::Map, data.cast::<PyDict>()?, alias_key);
        } else if data.is_exact_instance_of::<PyList>() {
            native!(Kind::List);
            return self.represent_list(data, alias_key);
        } else if data.is_exact_instance_of::<PyTuple>() {
            native!(Kind::Tuple);
            return self.represent_list(data, alias_key);
        } else if data.is_none() {
            native!(Kind::None);
            self.represent_none()
        } else if data.is_exact_instance_of::<PyFloat>() {
            native!(Kind::Float);
            self.represent_float(data)?
        } else if data.is_exact_instance_of::<PyBool>() {
            native!(Kind::Bool);
            self.represent_bool(data)?
        } else if data.is_exact_instance_of::<PyBytes>() {
            native!(Kind::Bytes);
            self.represent_binary(py, data)?
        } else {
            let ty = data.get_type();
            if ty.is(datetime_type(py, "datetime")?) {
                native!(Kind::Datetime);
                self.represent_datetime(py, data)?
            } else if ty.is(datetime_type(py, "date")?) {
                native!(Kind::Date);
                self.represent_date(py, data)?
            } else if data.is_exact_instance_of::<PySet>() {
                native!(Kind::Set);
                // PyYAML represents a set as a mapping of its members to None.
                let mapping = PyDict::new(py);
                for item in data.try_iter()? {
                    mapping.set_item(item?, py.None())?;
                }
                return self.represent_mapping(py, Tag::Set, &mapping, alias_key);
            } else {
                return Ok((self.represent_other(data, ctx)?, None));
            }
        };

        if let Some(key) = alias_key {
            self.represented_objects.insert(key, node);
        }

        Ok((node, None))
    }

    // ── Custom representers ──

    /// Represent what Rust does not: with custom representers, through them;
    /// without, it cannot be represented.
    #[cold]
    #[inline(never)]
    fn represent_other<'py>(
        &mut self,
        data: &Bound<'py, PyAny>,
        ctx: Option<&Ctx<'_, 'py>>,
    ) -> PyResult<NodeId> {
        match ctx {
            Some(ctx) => self.represent_custom(data, ctx),
            None => Err(exception::representer_error(
                data.py(),
                format!("cannot represent an object: {}", data.repr()?),
            )),
        }
    }

    /// Call the custom representer for `data`, as PyYAML's `represent_data`
    /// does. Its result is not recorded for `data`: the callback does that
    /// through `represent_scalar` and the like, which record the node for the
    /// dumper's `alias_key`.
    fn represent_custom<'py>(
        &mut self,
        data: &Bound<'py, PyAny>,
        ctx: &Ctx<'_, 'py>,
    ) -> PyResult<NodeId> {
        let py = data.py();
        let node = match ctx.find(data)? {
            Some(representer) => {
                self.call_python(ctx.dumper, || representer.call1((ctx.dumper, data)))?
            }
            None => node_classes(py)?
                .scalar
                .bind(py)
                .call1((py.None(), data.str()?))?,
        };
        Ok(self.bind_python(node))
    }

    /// Run Python code that may use the dumper, with the representer back in
    /// it.
    fn call_python<T>(
        &mut self,
        dumper: &Bound<'_, RSafeDumper>,
        f: impl FnOnce() -> PyResult<T>,
    ) -> PyResult<T> {
        put_rep(dumper, std::mem::take(self));
        let result = f();
        let mut dumper = dumper.borrow_mut();
        // Anything the callback started with the representer has finished,
        // so it is back.
        debug_assert!(!dumper.rep_busy);
        *self = std::mem::take(&mut dumper.rep);
        dumper.rep_busy = true;
        result
    }

    /// The node for a PyYAML node object (or anything a custom representer
    /// returned), read when the document is serialized.
    fn bind_python(&mut self, node: Bound<'_, PyAny>) -> NodeId {
        let key = node.as_ptr() as usize;
        if let Some(&id) = self.python_ids.get(&key) {
            return id;
        }
        let id = self.nodes.len();
        self.nodes.push(RepNode::Python(node.clone().unbind()));
        self.python_ids.insert(key, id);
        self.python_keeper.push(node.unbind());
        id
    }

    /// `node` as a PyYAML node object, for Python code. From then on, that
    /// object is the node: what Python does to it is serialized.
    fn export<'py>(&mut self, py: Python<'py>, node: NodeId) -> PyResult<Bound<'py, PyAny>> {
        let (object, items) = self.export_node(py, node)?;
        // Fill in the items of collections, depth first, without recursion.
        let mut stack: Vec<_> = items.into_iter().collect();
        while let Some((list, items)) = stack.pop() {
            match items {
                ExportItems::Sequence(items) => {
                    for item in items {
                        let (item, more) = self.export_node(py, item)?;
                        list.append(item)?;
                        stack.extend(more);
                    }
                }
                ExportItems::Mapping(items) => {
                    for (key, value) in items {
                        let (key, key_items) = self.export_node(py, key)?;
                        let (value, value_items) = self.export_node(py, value)?;
                        list.append((key, value))?;
                        stack.extend(key_items);
                        stack.extend(value_items);
                    }
                }
            }
        }
        Ok(object)
    }

    /// Make one node a PyYAML node object; a collection's items are returned,
    /// with the empty list to add them to.
    fn export_node<'py>(
        &mut self,
        py: Python<'py>,
        id: NodeId,
    ) -> PyResult<(Bound<'py, PyAny>, Option<PendingItems<'py>>)> {
        let classes = node_classes(py)?;
        let default_style = &self.default_style_object;
        let (object, items) = match &mut self.nodes[id] {
            RepNode::Python(object) => return Ok((object.bind(py).clone(), None)),
            RepNode::Scalar { tag, value, style } => {
                let value = match value {
                    ScalarText::Str(text) => text.bind(py).clone().into_any(),
                    other => PyString::new(py, other.text(py)?).into_any(),
                };
                let style = match *style {
                    None => py.None().into_bound(py),
                    Some(OTHER_STYLE) => match default_style {
                        Some(style) => style.bind(py).clone(),
                        None => py.None().into_bound(py),
                    },
                    Some(style) => PyString::new(py, style.encode_utf8(&mut [0; 4])).into_any(),
                };
                let object = classes.scalar.bind(py).call1((
                    tag.as_str(),
                    value,
                    py.None(),
                    py.None(),
                    style,
                ))?;
                (object, None)
            }
            RepNode::Sequence {
                tag,
                value,
                flow_style,
            } => {
                let list = PyList::empty(py);
                let object = classes.sequence.bind(py).call1((
                    tag.as_str(),
                    &list,
                    py.None(),
                    py.None(),
                    *flow_style,
                ))?;
                let items = ExportItems::Sequence(std::mem::take(value));
                (object, Some((list, items)))
            }
            RepNode::Mapping {
                tag,
                value,
                flow_style,
            } => {
                let list = PyList::empty(py);
                let object = classes.mapping.bind(py).call1((
                    tag.as_str(),
                    &list,
                    py.None(),
                    py.None(),
                    *flow_style,
                ))?;
                let items = ExportItems::Mapping(std::mem::take(value));
                (object, Some((list, items)))
            }
        };
        let old = std::mem::replace(
            &mut self.nodes[id],
            RepNode::Python(object.clone().unbind()),
        );
        drop_node(py, old);
        self.python_ids.insert(object.as_ptr() as usize, id);
        self.python_keeper.push(object.clone().unbind());
        Ok((object, items))
    }

    /// Read the structure of the PyYAML node objects the document reaches
    /// from `root`: each collection's items. Like CEmitter's anchor pass,
    /// right after the document start, this walks the graph depth first,
    /// taking each item from a collection's value (and unpacking a pair)
    /// only after going through the item before, and reads nothing else; the
    /// tag, a scalar's value and the styles are read as each node is
    /// serialized (see `read_python_attributes`).
    ///
    /// Every node it reaches is added to `visits`, in order, as the anchor
    /// pass would see it. Like the anchor pass, it does not go into nodes
    /// `kept` from a failed document has seen.
    fn read_python_structure(
        &mut self,
        py: Python,
        root: NodeId,
        kept: &[NodeState],
        visits: &mut Vec<NodeId>,
    ) -> PyResult<()> {
        let classes = node_classes(py)?;
        let mut seen = Vec::new();
        let mut stack = Vec::new();
        let mut next = Some(root);
        loop {
            if let Some(id) = next.take() {
                visits.push(id);
                if seen.len() <= id {
                    seen.resize(self.nodes.len(), false);
                }
                if std::mem::replace(&mut seen[id], true)
                    || kept
                        .get(id)
                        .is_some_and(|state| !matches!(state.anchor, Anchor::Unseen))
                {
                    continue;
                }
                let frame = match &self.nodes[id] {
                    RepNode::Python(object) => {
                        let object = object.bind(py).clone();
                        let is_sequence = object.is_instance(classes.sequence.bind(py))?;
                        if is_sequence || object.is_instance(classes.mapping.bind(py))? {
                            let items = object.getattr(pyo3::intern!(py, "value"))?.try_iter()?;
                            StructureFrame::Python {
                                id,
                                object: object.unbind(),
                                items,
                                read: if is_sequence {
                                    ReadItems::Sequence(Vec::new())
                                } else {
                                    ReadItems::Mapping(Vec::new(), None)
                                },
                            }
                        } else {
                            // The tag and style are placeholders until it is read.
                            let old = std::mem::replace(
                                &mut self.nodes[id],
                                RepNode::Scalar {
                                    tag: Tag::Str,
                                    value: ScalarText::Static(""),
                                    style: None,
                                },
                            );
                            drop_node(py, old);
                            self.python_unread.insert(id, object.unbind());
                            continue;
                        }
                    }
                    RepNode::Sequence { .. } | RepNode::Mapping { .. } => {
                        StructureFrame::Rust { id, index: 0 }
                    }
                    RepNode::Scalar { .. } => continue,
                };
                stack.push(frame);
                continue;
            }
            let Some(frame) = stack.last_mut() else {
                break;
            };
            match frame {
                StructureFrame::Rust { id, index } => {
                    next = match &self.nodes[*id] {
                        RepNode::Sequence { value, .. } => value.get(*index).copied(),
                        RepNode::Mapping { value, .. } => value
                            .get(*index / 2)
                            .map(|&(key, val)| if index.is_multiple_of(2) { key } else { val }),
                        RepNode::Scalar { .. } | RepNode::Python(_) => unreachable!(),
                    };
                    *index += 1;
                }
                StructureFrame::Python { items, read, .. } => {
                    if let ReadItems::Mapping(_, pending @ Some(_)) = read {
                        // The value of the pair whose key was gone through.
                        next = pending.take();
                        continue;
                    }
                    match items.next() {
                        Some(item) => {
                            let item = item?;
                            match read {
                                ReadItems::Sequence(value) => {
                                    let item = self.bind_python(item);
                                    value.push(item);
                                    next = Some(item);
                                }
                                ReadItems::Mapping(value, pending) => {
                                    let (key, item_value) = unpack_pair(&item)?;
                                    let (key, item_value) =
                                        (self.bind_python(key), self.bind_python(item_value));
                                    value.push((key, item_value));
                                    *pending = Some(item_value);
                                    next = Some(key);
                                }
                            }
                        }
                        None => {
                            let Some(StructureFrame::Python {
                                id, object, read, ..
                            }) = stack.pop()
                            else {
                                unreachable!()
                            };
                            // The tag and flow style are placeholders until it is read.
                            let node = match read {
                                ReadItems::Sequence(value) => RepNode::Sequence {
                                    tag: Tag::Seq,
                                    value,
                                    flow_style: None,
                                },
                                ReadItems::Mapping(value, _) => RepNode::Mapping {
                                    tag: Tag::Map,
                                    value,
                                    flow_style: None,
                                },
                            };
                            let old = std::mem::replace(&mut self.nodes[id], node);
                            drop_node(py, old);
                            self.python_unread.insert(id, object);
                        }
                    }
                    continue;
                }
            }
            if next.is_none() {
                stack.pop();
            }
        }
        // A node object can be anywhere in the graph more than once.
        self.has_aliases = true;
        Ok(())
    }

    /// Read a node object's tag, scalar value and style into its node, as
    /// CEmitter does when it serializes it: the tag and a scalar's value are
    /// exact `str` or `bytes`, and a style is used if it is exactly one of
    /// PyYAML's. Unlike CEmitter, which skips anything but its node classes,
    /// ryaml accepts their subclasses and refuses other objects.
    fn read_python_attributes(
        &mut self,
        py: Python,
        id: NodeId,
        object: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let classes = node_classes(py)?;
        let is_scalar = object.is_instance(classes.scalar.bind(py))?;
        if !is_scalar
            && !object.is_instance(classes.sequence.bind(py))?
            && !object.is_instance(classes.mapping.bind(py))?
        {
            return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "expected a ScalarNode, SequenceNode or MappingNode, not {}",
                object.get_type().name()?
            )));
        }
        let tag = object.getattr(pyo3::intern!(py, "tag"))?;
        if is_scalar {
            let value = object.getattr(pyo3::intern!(py, "value"))?;
            let value = if value.is_exact_instance_of::<PyString>() {
                ScalarText::Str(value.cast_into::<PyString>()?.unbind())
            } else if value.is_exact_instance_of::<PyBytes>() {
                let bytes = value.cast::<PyBytes>()?.as_bytes();
                if bytes.is_empty() {
                    // CEmitter's resolver looks at `value[0]`.
                    return Err(pyo3::exceptions::PyIndexError::new_err(
                        "index out of range",
                    ));
                }
                let text = std::str::from_utf8(bytes).map_err(|e| {
                    pyo3::exceptions::PyUnicodeDecodeError::new_err_from_utf8(py, bytes, e)
                })?;
                ScalarText::Bytes(text.to_owned())
            } else {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "value must be a string",
                ));
            };
            let tag = python_tag(&tag)?;
            let style = scalar_style(&object.getattr(pyo3::intern!(py, "style"))?)?;
            let old = std::mem::replace(&mut self.nodes[id], RepNode::Scalar { tag, value, style });
            drop_node(py, old);
            return Ok(());
        }
        let new_tag = python_tag(&tag)?;
        let new_flow_style = object
            .getattr(pyo3::intern!(py, "flow_style"))?
            .is_truthy()?;
        match &mut self.nodes[id] {
            RepNode::Sequence {
                tag, flow_style, ..
            }
            | RepNode::Mapping {
                tag, flow_style, ..
            } => {
                *tag = new_tag;
                *flow_style = Some(new_flow_style);
            }
            _ => unreachable!("read as a collection"),
        }
        Ok(())
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
        let bytes: &[u8] = data.cast::<PyBytes>()?.as_bytes();
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

    fn represent_list<'py>(
        &mut self,
        data: &Bound<'py, PyAny>,
        alias_key: Option<usize>,
    ) -> PyResult<(NodeId, Option<CollectionFrame<'py>>)> {
        let node = self.register(
            alias_key,
            RepNode::Sequence {
                tag: Tag::Seq,
                value: Vec::new(),
                flow_style: None,
            },
        );
        let items = CollectionItems::Sequence(data.try_iter()?);
        Ok((node, Some(CollectionFrame::new(node, items))))
    }

    /// PyYAML's `represent_mapping`, for dicts and sets.
    fn represent_mapping<'py>(
        &mut self,
        py: Python<'py>,
        tag: Tag,
        mapping: &Bound<'py, PyDict>,
        alias_key: Option<usize>,
    ) -> PyResult<(NodeId, Option<CollectionFrame<'py>>)> {
        let node = self.register(
            alias_key,
            RepNode::Mapping {
                tag,
                value: Vec::new(),
                flow_style: None,
            },
        );
        let items = CollectionItems::Mapping {
            items: self.mapping_items(py, mapping)?.into_iter(),
            pending_value: None,
            represented_key: None,
        };
        Ok((node, Some(CollectionFrame::new(node, items))))
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
    ) -> PyResult<Vec<MappingItem<'py>>> {
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
        items: Vec<MappingItem<'py>>,
    ) -> PyResult<Vec<MappingItem<'py>>> {
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

    /// Drop the document's nodes, releasing their strings through `Bound`
    /// (a direct decref) rather than pyo3's deferred `Py` drop.
    fn clear_nodes(&mut self, py: Python) {
        for node in self.nodes.drain(..) {
            drop_node(py, node);
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

    /// PyYAML's test for the best flow style: the item is a scalar node
    /// without a style.
    #[cold]
    fn is_plain_scalar(&self, py: Python, node: NodeId) -> PyResult<bool> {
        match &self.nodes[node] {
            RepNode::Scalar { style, .. } => Ok(style.is_none()),
            RepNode::Python(object) => {
                let object = object.bind(py);
                Ok(object.is_instance(node_classes(py)?.scalar.bind(py))?
                    && !object.getattr(pyo3::intern!(py, "style"))?.is_truthy()?)
            }
            _ => Ok(false),
        }
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

        // CEmitter's anchor pass reads node objects' items, after the
        // document start.
        if !self.rep.python_ids.is_empty() {
            self.read_python_structure(py, node)?;
        }

        // Like CEmitter, the anchors and written nodes of a document that
        // failed are kept (they are reset only after a document succeeds), so
        // a later document reusing its nodes sees them; then the anchor pass
        // is needed even if nothing repeats in this one.
        let carried_over = !self.node_states.is_empty();
        self.node_states
            .resize(self.rep.nodes.len(), NodeState::default());
        let anchored = self.rep.has_aliases || carried_over;
        if anchored {
            self.anchor_nodes(node)?;
        }
        let result = self
            .serialize_nodes(py, node)
            .and_then(|()| self.emit(py, Event::document_end(self.document_end_implicit)));
        if let Err(error) = result {
            if !anchored {
                // The kept state must be what the skipped anchor pass would
                // have left: every node seen (none twice, so no anchors).
                self.anchor_nodes(node)?;
            }
            return Err(error);
        }

        // Reset serializer state
        self.node_states.clear();
        self.last_alias_id = 0;
        Ok(())
    }

    /// Read the structure of the node objects in the document (see
    /// `Representer::read_python_structure`). If that fails, the nodes
    /// reached so far are anchored, as CEmitter's anchor pass, which reads
    /// them, leaves them for the next document.
    #[cold]
    #[inline(never)]
    fn read_python_structure(&mut self, py: Python, root: NodeId) -> PyResult<()> {
        let mut visits = Vec::new();
        let result = self
            .rep
            .read_python_structure(py, root, &self.node_states, &mut visits);
        if result.is_err() {
            self.node_states
                .resize(self.rep.nodes.len(), NodeState::default());
            for node in visits {
                self.anchor_visit(node);
            }
        }
        result
    }

    /// The anchor pass reaching `node`: whether to go into its items.
    #[inline(always)]
    fn anchor_visit(&mut self, node: NodeId) -> bool {
        match self.node_states[node].anchor {
            Anchor::Unseen => {
                self.node_states[node].anchor = Anchor::Seen;
                true
            }
            Anchor::Seen => {
                // Reached twice: it gets the next anchor (PyYAML's id001, ...).
                self.last_alias_id += 1;
                self.node_states[node].anchor = Anchor::Named(self.last_alias_id);
                false
            }
            Anchor::Named(_) => false,
        }
    }

    // Only needed when a node repeats, so kept out of `serialize`.
    #[inline(never)]
    fn anchor_nodes(&mut self, root: NodeId) -> PyResult<()> {
        let mut walk = NodeWalk::new(root);
        while let Some(visit) = walk.next(&self.rep.nodes) {
            let NodeVisit::Enter(node, depth) = visit else {
                continue;
            };
            // Nodes kept from a failed document can be reused deeper in a later one.
            check_depth(depth)?;
            if self.anchor_visit(node) && !matches!(self.rep.nodes[node], RepNode::Scalar { .. }) {
                walk.descend(node);
            }
        }
        Ok(())
    }

    fn serialize_nodes(&mut self, py: Python, root: NodeId) -> PyResult<()> {
        let mut walk = NodeWalk::new(root);
        while let Some(visit) = walk.next(&self.rep.nodes) {
            match visit {
                NodeVisit::Enter(node, depth) => {
                    check_depth(depth)?;
                    let is_alias = self.node_states[node].serialized;
                    self.emit_node(py, node)?;
                    if !is_alias && !matches!(self.rep.nodes[node], RepNode::Scalar { .. }) {
                        walk.descend(node);
                    }
                }
                NodeVisit::Leave(node) => {
                    let event = match &self.rep.nodes[node] {
                        RepNode::Sequence { .. } => Event::sequence_end(),
                        RepNode::Mapping { .. } => Event::mapping_end(),
                        RepNode::Scalar { .. } | RepNode::Python(_) => unreachable!(),
                    };
                    self.emit(py, event)?;
                }
            }
        }
        Ok(())
    }

    /// Read `node` if it is a node object not read yet, as CEmitter does
    /// when it serializes it: its tag, scalar value and styles, then, as
    /// CEmitter iterates a collection's value again to serialize its items,
    /// its items again.
    #[cold]
    #[inline(never)]
    fn read_unread_node(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        if let Some(error) = self.rep.python_errors.remove(&node) {
            return Err(error);
        }
        if let RepNode::Python(_) = self.rep.nodes[node] {
            return self.read_failed_node(py, node);
        }
        let Some(object) = self.rep.python_unread.remove(&node) else {
            return Ok(());
        };
        let object = object.into_bound(py);
        self.rep.read_python_attributes(py, node, &object)?;
        if !matches!(self.rep.nodes[node], RepNode::Scalar { .. }) {
            self.reread_items(py, node, &object);
        }
        Ok(())
    }

    /// Read a node object whose items could not be read in the anchor pass
    /// of a document that failed, so that the anchors kept for the next
    /// document include it, as CEmitter does: its tag and styles, then its
    /// items again, as when serializing it.
    #[cold]
    #[inline(never)]
    fn read_failed_node(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        let RepNode::Python(object) = &self.rep.nodes[node] else {
            return Ok(());
        };
        let object = object.clone_ref(py);
        let classes = node_classes(py)?;
        let bound = object.bind(py);
        let placeholder = if bound.is_instance(classes.sequence.bind(py))? {
            RepNode::Sequence {
                tag: Tag::Seq,
                value: Vec::new(),
                flow_style: None,
            }
        } else if bound.is_instance(classes.mapping.bind(py))? {
            RepNode::Mapping {
                tag: Tag::Map,
                value: Vec::new(),
                flow_style: None,
            }
        } else {
            RepNode::Scalar {
                tag: Tag::Str,
                value: ScalarText::Static(""),
                style: None,
            }
        };
        let old = std::mem::replace(&mut self.rep.nodes[node], placeholder);
        drop_node(py, old);
        self.rep.python_unread.insert(node, object);
        self.read_unread_node(py, node)
    }

    /// Replace the items of the collection node `id` with those read again
    /// from its node object. They usually are the same, but the value can be
    /// an iterator, exhausted by the anchor pass. Where reading fails, or
    /// finds a node the anchor pass did not (for which CEmitter raises
    /// KeyError), the items end with a new node that raises the error when
    /// it is serialized, after the items before it, as in CEmitter.
    fn reread_items(&mut self, py: Python, id: NodeId, object: &Bound<'_, PyAny>) {
        let is_mapping = matches!(self.rep.nodes[id], RepNode::Mapping { .. });
        let python_ids = &self.rep.python_ids;
        let node_states = &self.node_states;
        let anchored = |child: &Bound<'_, PyAny>| -> PyResult<NodeId> {
            match python_ids.get(&(child.as_ptr() as usize)) {
                Some(&child) if !matches!(node_states[child].anchor, Anchor::Unseen) => Ok(child),
                _ => Err(pyo3::exceptions::PyKeyError::new_err(
                    child.clone().unbind(),
                )),
            }
        };
        let mut sequence = Vec::new();
        let mut mapping = Vec::new();
        // A key serialized before its value is found not to be a node.
        let mut lone_key = None;
        let result = object
            .getattr(pyo3::intern!(py, "value"))
            .and_then(|value| value.try_iter())
            .and_then(|items| {
                for item in items {
                    let item = item?;
                    if is_mapping {
                        let (key, value) = unpack_pair(&item)?;
                        let key = anchored(&key)?;
                        lone_key = Some(key);
                        mapping.push((key, anchored(&value)?));
                        lone_key = None;
                    } else {
                        sequence.push(anchored(&item)?);
                    }
                }
                Ok(())
            });
        if let Err(error) = result {
            let failed = self.rep.nodes.len();
            self.rep.nodes.push(RepNode::Scalar {
                tag: Tag::Str,
                value: ScalarText::Static(""),
                style: None,
            });
            self.node_states.push(NodeState::default());
            self.rep.python_errors.insert(failed, error);
            sequence.push(failed);
            mapping.push((lone_key.unwrap_or(failed), failed));
        }
        match &mut self.rep.nodes[id] {
            RepNode::Sequence { value, .. } => *value = sequence,
            RepNode::Mapping { value, .. } => *value = mapping,
            RepNode::Scalar { .. } | RepNode::Python(_) => unreachable!("read as a collection"),
        }
    }

    fn emit_node(&mut self, py: Python, node: NodeId) -> PyResult<()> {
        let anchor = match self.node_states[node].anchor {
            Anchor::Named(id) => Some(format!("id{id:03}")),
            _ => None,
        };

        if self.node_states[node].serialized {
            // Emit alias. Only a node object's items read again can repeat
            // a node the anchor pass saw once (see `alias_without_anchor`).
            let Some(anchor_str) = anchor.as_deref() else {
                return Err(alias_without_anchor(py));
            };
            self.emit(py, Event::alias(anchor_str))?;
            return Ok(());
        }
        self.node_states[node].serialized = true;
        if !self.rep.python_ids.is_empty() {
            self.read_unread_node(py, node)?;
        }

        let anchor_ref = anchor.as_deref();

        match &self.rep.nodes[node] {
            RepNode::Python(_) => unreachable!("node objects are read before serializing"),
            RepNode::Scalar { tag, value, style } => {
                let is_bytes = matches!(value, ScalarText::Bytes(_));
                let value = value.text(py)?;
                let plain_implicit = if is_bytes {
                    *tag == Tag::Str
                } else {
                    *tag == resolver::resolve_scalar(value, true)
                };
                // A non-plain scalar always resolves to str.
                let quoted_implicit = *tag == Tag::Str;
                let scalar_style = char_to_scalar_style(*style);
                // The emitter only looks at the tag when it is canonical or
                // neither implicit flag is set, so skip building it otherwise.
                let tag = (self.canonical || !plain_implicit && !quoted_implicit)
                    .then(|| written_tag(tag))
                    .flatten();

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
                let tag = (self.canonical || !implicit)
                    .then(|| written_tag(tag))
                    .flatten();
                let event = Event::sequence_start(anchor_ref, tag, implicit, style);
                self.emit(py, event)?;
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
                let tag = (self.canonical || !implicit)
                    .then(|| written_tag(tag))
                    .flatten();
                let event = Event::mapping_start(anchor_ref, tag, implicit, style);
                self.emit(py, event)?;
            }
        }
        Ok(())
    }
}

// ── Free helpers ─────────────────────────────────────────────────────────────

/// For an alias without an anchor, PyYAML's Python emitter raises this
/// (LibYAML aborts).
#[cold]
#[inline(never)]
fn alias_without_anchor(py: Python) -> PyErr {
    exception::emitter_error(py, "anchor is not specified for alias".to_string())
}

/// A node's tag as written: none for `NO_TAG`.
#[inline(always)]
fn written_tag(tag: &Tag) -> Option<&str> {
    match tag {
        Tag::Other(text) if text == NO_TAG => None,
        tag => Some(tag.as_str()),
    }
}

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
        let bytes = value.cast::<PyBytes>()?.as_bytes();
        return std::str::from_utf8(bytes).map(str::to_owned).map_err(|e| {
            pyo3::exceptions::PyUnicodeDecodeError::new_err_from_utf8(value.py(), bytes, e)
        });
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "tag {what} must be a string"
    )))
}

/// An option CEmitter passes to libyaml as a C int: like `int(value)` for
/// numbers (floats and decimals truncate), but str and bytes are refused.
struct CInt(i32);

impl<'py> FromPyObject<'_, 'py> for CInt {
    type Error = PyErr;

    fn extract(value: Borrowed<'_, 'py, PyAny>) -> PyResult<Self> {
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

impl<'py> FromPyObject<'_, 'py> for Truthy {
    type Error = PyErr;

    fn extract(value: Borrowed<'_, 'py, PyAny>) -> PyResult<Self> {
        Ok(Truthy(value.is_truthy()?))
    }
}

/// A `default_style` PyYAML keeps on scalar nodes but CEmitter writes plain.
const OTHER_STYLE: char = '\0';

fn scalar_style(style: &Bound<'_, PyAny>) -> PyResult<Option<char>> {
    Ok(match style.extract::<String>().ok().as_deref() {
        Some("'") => Some('\''),
        Some("\"") => Some('"'),
        Some("|") => Some('|'),
        Some(">") => Some('>'),
        _ if style.is_truthy()? => Some(OTHER_STYLE),
        _ => None,
    })
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
        rep: Representer {
            default_flow_style: Some(false),
            ..Representer::default()
        },
        rep_busy: false,
        in_document: false,
    };

    dumper
        .emitter
        .emit(Event::stream_start(Encoding::Utf8))
        .map_err(|e| exception::emitter_error(py, e))?;

    let node = dumper.rep.represent_data(py, obj, None)?;
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
