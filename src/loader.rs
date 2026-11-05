//! Module implement pyyaml compatibility layer for ryaml via libyaml
//! Implements RLoader, which can load YAML 1.1

use base64::Engine as _;
use libyaml_safer::{ErrorKind, Event, EventData, Parser};
use pyo3::exceptions::{PyIndexError, PyNotImplementedError, PyRecursionError, PyTypeError};
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
use crate::input::Source;
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
    /// Anchored nodes that ryaml used without constructing them as PyYAML would
    /// (a merge source or pair entry whose tag was ignored, a pair entry with an
    /// unhashable key), with the ConstructorError message PyYAML raises when an
    /// alias uses the node as an ordinary value.
    value_alias_errors: HashMap<String, String, FxBuildHasher>,
    /// Anchored !!omap / !!pairs merge sources: `anchors` holds the tuple list an
    /// alias gets as a value, this holds the mappings an alias merges.
    merge_source_lists: HashMap<String, Py<PyAny>, FxBuildHasher>,
    /// Raw text of anchored scalars, for an alias used as the `=` value of a
    /// mapping that is constructed as a scalar (see `mapping_value_scalar`).
    scalar_anchor_values: HashMap<String, String, FxBuildHasher>,
    /// Anchored scalars inside skipped entries (see `skip_node`): PyYAML composes
    /// them but constructs them only when an alias uses them, so keep the tag,
    /// raw text and implicit flag until then.
    deferred_scalars: HashMap<String, (Option<String>, String, bool), FxBuildHasher>,
    /// Anchored collections inside skipped entries. They count for duplicate
    /// anchors, but cannot be constructed afterwards.
    skipped_anchors: HashSet<String, FxBuildHasher>,
    /// Anchors of mappings currently being constructed as scalars (see
    /// `construct_mapping_as_scalar`), innermost last.
    scalar_mappings_in_progress: Vec<String>,
    pending_mappings: Vec<PendingMapping>,
    // Sets are represented by mapping nodes in YAML and can be merge sources.
    set_mappings: HashMap<usize, (Py<PySet>, Py<PyDict>), FxBuildHasher>,
    /// Anchored !!omap / !!pairs lists (by object) and the single-entry
    /// mappings PyYAML would merge if an alias to them is a merge source.
    pairs_mappings: HashMap<usize, Py<PyList>, FxBuildHasher>,
    anchored_mapping_lengths: HashMap<usize, usize, FxBuildHasher>,
}

#[pymethods]
impl RSafeLoader {
    #[new]
    pub fn new(py: Python, source: Source) -> PyResult<Self> {
        Ok(Self::from_text(source.into_text(py)?))
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
            match self.construct_document(py) {
                Ok(document) => document,
                // PyYAML's get_single_node also checks for the end of the
                // stream before constructing, so that error wins too.
                Err(err) if !is_compose_error(py, &err) => {
                    return Err(match self._parse_next_event(py) {
                        Err(parse_err) => parse_err,
                        Ok(()) if !self.at_stream_end() => single_document_error(py),
                        Ok(()) => err,
                    });
                }
                Err(err) => return Err(err),
            }
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
            return Err(single_document_error(py));
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
                Err(e) => return Err(parse_error(py, &e)),
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

    /// Whether `name` was defined earlier in the document, in any form.
    fn is_known_anchor(&self, name: &str) -> bool {
        self.anchors.contains_key(name)
            || self.merge_anchors.contains(name)
            || self.deferred_scalars.contains_key(name)
            || self.skipped_anchors.contains(name)
    }

    /// A node that fails to construct still holds its anchor (PyYAML's composer
    /// registered it first), so a duplicate is still an error and aliases to it
    /// inside the rest of the document are not "undefined".
    fn reserve_failed_anchor(&mut self, py: Python, anchor: Option<String>) -> PyResult<()> {
        if let Some(anchor) = anchor {
            self.check_new_anchor(py, &anchor)?;
            self.skipped_anchors.insert(anchor);
        }
        Ok(())
    }

    fn check_new_anchor(&self, py: Python, name: &str) -> PyResult<()> {
        if self.is_known_anchor(name) {
            return Err(exception::composer_error(
                py,
                format!("found duplicate anchor '{}'", name),
            ));
        }
        Ok(())
    }

    fn at_stream_end(&self) -> bool {
        matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::StreamEnd,
                ..
            })
        )
    }

    /// PyYAML composes a whole document before constructing it, so a parse or
    /// composer error anywhere in it wins over a construction error raised
    /// earlier. After a construction error, read the rest of the document and
    /// return the first such error, or `err` if there is none.
    fn first_compose_error(&mut self, py: Python, err: PyErr) -> PyErr {
        if is_compose_error(py, &err) {
            return err;
        }
        loop {
            let event = match self.parsed_event.take() {
                Some(event) => event,
                None => match self.parser.parse() {
                    Ok(event) => event,
                    Err(e) => return parse_error(py, &e),
                },
            };
            let anchor = match event.data {
                EventData::DocumentEnd { .. } | EventData::StreamEnd => return err,
                EventData::Alias { anchor } => {
                    if !self.is_known_anchor(&anchor) {
                        return exception::composer_error(
                            py,
                            format!("found undefined alias '{}'", anchor),
                        );
                    }
                    continue;
                }
                EventData::Scalar { anchor, .. }
                | EventData::SequenceStart { anchor, .. }
                | EventData::MappingStart { anchor, .. } => anchor,
                _ => None,
            };
            if let Some(anchor) = anchor {
                if let Err(dup) = self.check_new_anchor(py, &anchor) {
                    return dup;
                }
                self.skipped_anchors.insert(anchor);
            }
        }
    }

    /// Construct a document directly from events
    fn construct_document(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        // Eat document start event
        self.parsed_event = None;

        // Construct the root object directly from events
        self._parse_next_event(py)?;
        let result = match self.construct_from_events(py) {
            Ok(result) => result,
            Err(err) => return Err(self.first_compose_error(py, err)),
        };

        // Eat document end event
        self._parse_next_event(py)?;
        self.parsed_event = None;

        // Resolve merges after all explicit entries, including ancestor entries,
        // exist. Preserve the objects already referenced by aliases.
        self.finish_mappings(py)?;

        // Clear anchors for next document
        self.anchors.clear();
        self.merge_anchors.clear();
        self.value_alias_errors.clear();
        self.merge_source_lists.clear();
        self.scalar_anchor_values.clear();
        self.deferred_scalars.clear();
        self.skipped_anchors.clear();
        self.scalar_mappings_in_progress.clear();
        self.set_mappings.clear();
        self.pairs_mappings.clear();
        self.anchored_mapping_lengths.clear();

        Ok(Some(result))
    }

    /// Core single-pass constructor: consume the current event and produce a Python object
    fn construct_from_events(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        let event = self.parsed_event.take().unwrap();
        match event.data {
            EventData::Alias { anchor } => {
                if let Some(message) = self.value_alias_errors.get(&anchor) {
                    Err(exception::constructor_error(py, message.clone()))
                } else if let Some(obj) = self.anchors.get(&anchor) {
                    Ok(obj.clone_ref(py))
                } else if let Some((tag, value, implicit)) = self.deferred_scalars.remove(&anchor) {
                    // First use of a composed-but-unconstructed scalar: construct
                    // it now (with its own errors, as PyYAML) and cache it.
                    let obj = self.construct_scalar_direct(py, None, tag, value, implicit)?;
                    self.anchors.insert(anchor, obj.clone_ref(py));
                    Ok(obj)
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

        // PyYAML's composer registers (and checks) the anchor before the
        // constructor runs, so a duplicate wins over a construction error and a
        // node that fails to construct still holds its anchor.
        if let Some(anchor_name) = &anchor {
            self.check_new_anchor(py, anchor_name)?;
        }
        let result = (|| -> PyResult<Py<PyAny>> {
            Ok(match resolved_tag {
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
            })
        })();
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                if let Some(anchor_name) = anchor {
                    self.skipped_anchors.insert(anchor_name);
                }
                return Err(err);
            }
        };

        if let Some(anchor_name) = anchor {
            self.scalar_anchor_values.insert(anchor_name.clone(), value);
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
            self.reserve_failed_anchor(py, anchor)?;
            return Err(exception::constructor_error(py, unknown_tag_message(t)));
        }
        let pairs = matches!(tag.as_deref(), Some(crate::TAG_OMAP | crate::TAG_PAIRS));
        let list = PyList::empty(py);
        let list_obj: Py<PyAny> = list.clone().unbind().into_any();

        // Store in anchors BEFORE recursing (handles circular references)
        let anchored_pairs = pairs && anchor.is_some();
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
        if anchored_pairs {
            // PyYAML merges the node, a sequence of single-entry mappings, so an
            // alias used as a merge source gets those mappings, not the tuples.
            let mappings = PyList::empty(py);
            let all_hashable = list.iter().all(|entry| {
                let mapping = PyDict::new(py);
                entry
                    .cast::<PyTuple>()
                    .ok()
                    .and_then(|t| {
                        mapping
                            .set_item(t.get_item(0).ok()?, t.get_item(1).ok()?)
                            .ok()
                    })
                    .and_then(|()| mappings.append(mapping).ok())
                    .is_some()
            });
            if all_hashable {
                self.pairs_mappings
                    .insert(list.as_ptr() as usize, mappings.unbind());
            }
        }
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
        if let Some(t) = tag.as_deref()
            && is_scalar_tag(t)
        {
            return self.construct_mapping_as_scalar(py, anchor, tag.unwrap());
        }
        // Reject unknown tags (pyyaml SafeConstructor behavior)
        if let Some(ref t) = tag
            && t != "!"
            && t != crate::TAG_MAP
            && t != crate::TAG_SET
        {
            self.reserve_failed_anchor(py, anchor)?;
            return Err(exception::constructor_error(py, unknown_tag_message(t)));
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
                let mut value = self.construct_merge_value(py)?;
                if let Some((_, mapping)) = self.set_mappings.get(&(value.as_ptr() as usize)) {
                    value = mapping.clone_ref(py).into_any();
                }

                // Collect merge source(s) — must be a mapping or list of mappings
                if value.downcast_bound::<PyDict>(py).is_ok() {
                    merge_sources.push(value);
                } else if value.downcast_bound::<PyList>(py).is_ok() {
                    // Expanded in `finish_mapping`: the list may be an alias to
                    // a sequence still under construction (a recursive merge).
                    merge_sources.push(value);
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
                && is_value_key(tag.as_deref(), value, plain_implicit)
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

    /// A scalar tag on a mapping: PyYAML's `SafeConstructor.construct_scalar`
    /// uses the raw value of the mapping's first `=` (value) key, so
    /// `!!int {=: 5}` is 5. The mapping's other entries are never constructed.
    fn construct_mapping_as_scalar(
        &mut self,
        py: Python,
        anchor: Option<String>,
        tag: String,
    ) -> PyResult<Py<PyAny>> {
        // PyYAML's composer registers the anchor before the mapping's entries,
        // so they can neither reuse it nor see it as undefined.
        if let Some(anchor) = &anchor {
            self.check_new_anchor(py, anchor)?;
            self.skipped_anchors.insert(anchor.clone());
            self.scalar_mappings_in_progress.push(anchor.clone());
        }
        let value = self.mapping_value_scalar(py, false);
        if anchor.is_some() {
            self.scalar_mappings_in_progress.pop();
        }
        let Some(value) = value? else {
            return Err(expected_scalar_error(py, "mapping"));
        };
        if tag == crate::TAG_TIMESTAMP {
            // PyYAML matches its timestamp regex against the node's entry list.
            return Err(PyTypeError::new_err(
                "expected string or bytes-like object, got 'list'",
            ));
        }
        // The anchor stays reserved on the errors above, as PyYAML's composer
        // registered it; construct_scalar_direct registers it properly.
        if let Some(anchor) = &anchor {
            self.skipped_anchors.remove(anchor);
        }
        self.construct_scalar_direct(py, anchor, Some(tag), value, false)
    }

    /// Consume the rest of a mapping (its start event already taken) and return
    /// the raw scalar text of its first `=` key's value, if it has one.
    ///
    /// When `skipping` (the mapping is inside an entry PyYAML never constructs),
    /// a `=` value that is not a plain scalar is only checked, not resolved:
    /// resolving it could fail (e.g. an alias back into a mapping still being
    /// built) where PyYAML does nothing. Its result is then unknown (`None`).
    fn mapping_value_scalar(&mut self, py: Python, skipping: bool) -> PyResult<Option<String>> {
        let mut found = None;
        let mut value_seen = false;
        loop {
            self._parse_next_event(py)?;
            let is_value = match &self.parsed_event {
                Some(Event {
                    data: EventData::MappingEnd,
                    ..
                }) => break,
                Some(Event {
                    data:
                        EventData::Scalar {
                            tag,
                            value,
                            plain_implicit,
                            ..
                        },
                    ..
                }) => is_value_key(tag.as_deref(), value, *plain_implicit),
                _ => false,
            };
            self.skip_node(py)?;
            self._parse_next_event(py)?;
            let scalar = matches!(
                &self.parsed_event,
                Some(Event {
                    data: EventData::Scalar { .. },
                    ..
                })
            );
            if is_value && !value_seen && (scalar || !skipping) {
                found = Some(self.node_scalar_value(py)?);
            } else {
                self.skip_node(py)?;
            }
            value_seen |= is_value;
        }
        self.parsed_event = None;
        Ok(found)
    }

    /// The raw scalar text of the current node, following `=` keys through
    /// mappings like PyYAML's `construct_scalar`.
    fn node_scalar_value(&mut self, py: Python) -> PyResult<String> {
        let event = self.parsed_event.take().unwrap();
        match event.data {
            EventData::Scalar {
                anchor,
                tag,
                value,
                plain_implicit,
                ..
            } => {
                if let Some(anchor) = anchor {
                    self.defer_scalar(py, anchor, tag, value.clone(), plain_implicit)?;
                }
                Ok(value)
            }
            EventData::Alias { anchor } => match self.scalar_anchor_values.get(&anchor) {
                Some(value) => Ok(value.clone()),
                // PyYAML's construct_scalar follows the alias back into the
                // mapping it is constructing, forever.
                None if self.scalar_mappings_in_progress.contains(&anchor) => Err(
                    PyRecursionError::new_err("maximum recursion depth exceeded"),
                ),
                None if self.anchors.contains_key(&anchor)
                    || self.skipped_anchors.contains(&anchor) =>
                {
                    Err(expected_scalar_error(py, "collection"))
                }
                None => Err(exception::composer_error(
                    py,
                    format!("found undefined alias '{}'", anchor),
                )),
            },
            EventData::MappingStart { anchor, tag, .. } => self
                .scan_mapping(py, anchor, tag, false)?
                .ok_or_else(|| expected_scalar_error(py, "mapping")),
            _ => Err(expected_scalar_error(py, "sequence")),
        }
    }

    /// Consume the current node without constructing it. PyYAML still composes
    /// it, so every anchor inside is checked for duplicates and recorded, and
    /// every alias must refer to an earlier anchor.
    fn skip_node(&mut self, py: Python) -> PyResult<()> {
        match self.parsed_event.take().unwrap().data {
            EventData::Scalar {
                anchor: Some(anchor),
                tag,
                value,
                plain_implicit,
                ..
            } => self.defer_scalar(py, anchor, tag, value, plain_implicit)?,
            EventData::Alias { anchor } if !self.is_known_anchor(&anchor) => {
                return Err(exception::composer_error(
                    py,
                    format!("found undefined alias '{}'", anchor),
                ));
            }
            EventData::SequenceStart { anchor, .. } => {
                if let Some(anchor) = anchor {
                    self.check_new_anchor(py, &anchor)?;
                    self.skipped_anchors.insert(anchor);
                }
                loop {
                    self._parse_next_event(py)?;
                    if matches!(
                        &self.parsed_event,
                        Some(Event {
                            data: EventData::SequenceEnd,
                            ..
                        })
                    ) {
                        self.parsed_event = None;
                        break;
                    }
                    self.skip_node(py)?;
                }
            }
            EventData::MappingStart { anchor, tag, .. } => {
                self.scan_mapping(py, anchor, tag, true)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Read a mapping (its start event already taken) without constructing it,
    /// returning the raw text of its `=` value like `mapping_value_scalar`. Its
    /// anchor is registered first, as PyYAML's composer does; afterwards an
    /// alias to it constructs lazily if it has a scalar tag and a `=` value
    /// (what PyYAML would build), and is otherwise a skipped collection.
    fn scan_mapping(
        &mut self,
        py: Python,
        anchor: Option<String>,
        tag: Option<String>,
        skipping: bool,
    ) -> PyResult<Option<String>> {
        let Some(anchor) = anchor else {
            return self.mapping_value_scalar(py, skipping);
        };
        self.check_new_anchor(py, &anchor)?;
        self.skipped_anchors.insert(anchor.clone());
        self.scalar_mappings_in_progress.push(anchor.clone());
        let value = self.mapping_value_scalar(py, skipping);
        self.scalar_mappings_in_progress.pop();
        let value = value?;
        if let Some(value) = &value {
            // An alias in another `=` value follows this mapping's `=` key.
            self.scalar_anchor_values
                .insert(anchor.clone(), value.clone());
            if let Some(t) = tag.as_deref()
                && is_scalar_tag(t)
                && t != crate::TAG_TIMESTAMP
            {
                self.skipped_anchors.remove(&anchor);
                self.deferred_scalars
                    .insert(anchor, (tag, value.clone(), false));
            }
        }
        Ok(value)
    }

    fn defer_scalar(
        &mut self,
        py: Python,
        anchor: String,
        tag: Option<String>,
        value: String,
        plain_implicit: bool,
    ) -> PyResult<()> {
        self.check_new_anchor(py, &anchor)?;
        self.scalar_anchor_values
            .insert(anchor.clone(), value.clone());
        self.deferred_scalars
            .insert(anchor, (tag, value, plain_implicit));
        Ok(())
    }

    /// Construct the value of a `<<` key. PyYAML merges by node kind and never
    /// constructs the merge value, or the mappings in a merge sequence, by tag.
    fn construct_merge_value(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        if let Some(Event {
            data: EventData::Alias { anchor },
            ..
        }) = &self.parsed_event
        {
            if let Some(obj) = self
                .merge_source_lists
                .get(anchor)
                .or_else(|| self.anchors.get(anchor))
            {
                let obj = obj.clone_ref(py);
                self.parsed_event = None;
                return Ok(obj);
            }
        }
        let stripped = self.untag_merge_source(true);
        let pairs = matches!(
            stripped.as_deref(),
            Some(crate::TAG_OMAP | crate::TAG_PAIRS)
        );
        if !matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::SequenceStart { .. },
                ..
            })
        ) {
            return self.construct_from_events(py);
        }
        let Some(Event {
            data: EventData::SequenceStart { anchor, .. },
            ..
        }) = self.parsed_event.take()
        else {
            unreachable!()
        };
        let list = PyList::empty(py);
        if let Some(anchor) = &anchor {
            self.register_anchor(py, anchor.clone(), list.clone().into_any().unbind())?;
        }
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
            let item = self.construct_merge_value_item(py)?;
            list.append(item)?;
        }
        self.parsed_event = None;
        if pairs && let Some(anchor) = anchor {
            // An alias used as another merge source merges these mappings, but
            // one used as a value gets the !!omap / !!pairs list PyYAML would
            // construct from the same node (or its error).
            self.merge_source_lists
                .insert(anchor.clone(), list.clone().into_any().unbind());
            let entries = PyList::empty(py);
            for item in list.iter() {
                match item.cast::<PyDict>() {
                    Ok(mapping) if mapping.len() == 1 => {
                        let (key, value) = mapping.iter().next().unwrap();
                        entries.append(PyTuple::new(py, [key, value])?)?;
                    }
                    _ => {
                        self.value_alias_errors
                            .insert(anchor, unknown_tag_message(&stripped.unwrap()));
                        return Ok(list.into_any().unbind());
                    }
                }
            }
            self.anchors.insert(anchor, entries.into_any().unbind());
        }
        Ok(list.into_any().unbind())
    }

    fn construct_merge_value_item(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        if let Some(Event {
            data: EventData::Alias { anchor },
            ..
        }) = &self.parsed_event
        {
            if let Some(obj) = self.anchors.get(anchor) {
                let obj = obj.clone_ref(py);
                self.parsed_event = None;
                return Ok(obj);
            }
        }
        self.untag_merge_source(false);
        self.construct_from_events(py)
    }

    /// Drop a merge source's tag if constructing by it would reject the node or
    /// change its shape, so it is merged as a plain mapping or sequence.
    fn untag_merge_source(&mut self, allow_sequence: bool) -> Option<String> {
        let (anchor, tag, sequence) = match &mut self.parsed_event {
            Some(Event {
                data: EventData::MappingStart { anchor, tag, .. },
                ..
            }) if tag
                .as_deref()
                .is_some_and(|t| !matches!(t, "!" | crate::TAG_MAP | crate::TAG_SET)) =>
            {
                (anchor, tag, false)
            }
            Some(Event {
                data: EventData::SequenceStart { anchor, tag, .. },
                ..
            }) if allow_sequence
                && tag
                    .as_deref()
                    .is_some_and(|t| !matches!(t, "!" | crate::TAG_SEQ)) =>
            {
                (anchor, tag, true)
            }
            _ => return None,
        };
        let tag = tag.take().unwrap();
        // Aliases to !!omap / !!pairs sequences are handled by
        // `construct_merge_value`, which builds the list PyYAML would.
        if let Some(anchor) = anchor
            && !(sequence && matches!(tag.as_str(), crate::TAG_OMAP | crate::TAG_PAIRS))
        {
            self.value_alias_errors
                .insert(anchor.clone(), unknown_tag_message(&tag));
        }
        Some(tag)
    }

    /// !!omap / !!pairs entries are raw single-entry mappings, not constructed
    /// dicts: their keys may be unhashable, and duplicate entries must be rejected
    /// before a dict could collapse them.
    fn construct_pair_entry(&mut self, py: Python) -> PyResult<Py<PyAny>> {
        let event = self.parsed_event.take().unwrap();
        let anchor = match event.data {
            EventData::Alias { anchor } => {
                if self.merge_anchors.contains(&anchor)
                    || self.deferred_scalars.contains_key(&anchor)
                {
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
            EventData::MappingStart { anchor, tag, .. } => (anchor, tag),
            _ => {
                return Err(exception::constructor_error(
                    py,
                    "expected a mapping of length 1".to_string(),
                ));
            }
        };
        let (anchor, tag) = anchor;
        let wrapper = if let Some(anchor) = &anchor {
            let mapping = PyDict::new(py);
            self.register_anchor(py, anchor.clone(), mapping.clone().into_any().unbind())?;
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
        if let (Some(mapping), Some(anchor)) = (wrapper, anchor) {
            // The wrapper is a mapping node if referenced elsewhere by an alias.
            // A pair key may be unhashable; PyYAML only fails if an alias then
            // uses the entry as a mapping (or set).
            if mapping.set_item(&key, &value).is_err() {
                self.anchors.remove(&anchor);
                self.skipped_anchors.insert(anchor.clone());
                self.value_alias_errors
                    .insert(anchor, "found unhashable key".to_string());
                return Ok(PyTuple::new(py, [key, value])?.into_any().unbind());
            }
            // PyYAML reads the entry by node kind and ignores its tag, but an
            // alias used as a value is constructed by that tag.
            match tag.as_deref() {
                None | Some("!") | Some(crate::TAG_MAP) => {}
                Some(crate::TAG_SET) => {
                    let set = PySet::new(py, [&key])?;
                    self.set_mappings.insert(
                        set.as_ptr() as usize,
                        (set.clone().unbind(), mapping.clone().unbind()),
                    );
                    self.anchors.insert(anchor, set.into_any().unbind());
                }
                Some(t) => {
                    self.value_alias_errors
                        .insert(anchor, unknown_tag_message(t));
                }
            }
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
        let ctx = MergeContext {
            pending: &pending,
            indices: &indices,
            set_mappings: &self.set_mappings,
            pairs_mappings: &self.pairs_mappings,
        };
        let mut visited = vec![false; pending.len()];
        // Parents are appended after their children. Flatten parents first, as
        // PyYAML does, and break recursive merge edges at the explicit entries.
        for i in (0..pending.len()).rev() {
            finish_mapping(py, i, &ctx, &mut visited)?;
        }
        Ok(())
    }
}

/// Read-only lookups for flattening merges at the end of a document.
struct MergeContext<'a> {
    pending: &'a [PendingMapping],
    /// Pending mapping index by dict object.
    indices: &'a HashMap<usize, usize, FxBuildHasher>,
    /// !!set objects and the dicts they were built from.
    set_mappings: &'a HashMap<usize, (Py<PySet>, Py<PyDict>), FxBuildHasher>,
    /// !!omap / !!pairs lists and their single-entry mappings.
    pairs_mappings: &'a HashMap<usize, Py<PyList>, FxBuildHasher>,
}

/// Rebuild only mappings which contain merges, preserving insertion order and
/// alias identity. Explicit entries override all merged entries.
fn finish_mapping(
    py: Python,
    index: usize,
    ctx: &MergeContext,
    visited: &mut [bool],
) -> PyResult<()> {
    if visited[index] {
        return Ok(());
    }
    visited[index] = true;
    let mapping = &ctx.pending[index];
    let mut merged: Option<Bound<'_, PyDict>> = None;
    for source in &mapping.sources {
        let source = source.bind(py);
        let Ok(mut list) = source.cast::<PyList>().cloned() else {
            merge_source(py, source, &mut merged, ctx, visited)?;
            continue;
        };
        if let Some(mappings) = ctx.pairs_mappings.get(&(list.as_ptr() as usize)) {
            list = mappings.bind(py).clone();
        }
        // A merge sequence is flattened in reverse so its first mapping wins,
        // while later << entries still win overall.
        let items: Vec<_> = list.iter().collect();
        for item in items.into_iter().rev() {
            let item = match ctx.set_mappings.get(&(item.as_ptr() as usize)) {
                Some((_, mapping)) => mapping.bind(py).clone().into_any(),
                None => item,
            };
            if !item.is_instance_of::<PyDict>() {
                return Err(exception::constructor_error(
                    py,
                    "expected a mapping for merging, but found non-mapping".to_string(),
                ));
            }
            merge_source(py, &item, &mut merged, ctx, visited)?;
        }
    }
    let dict = mapping.dict.bind(py);
    if let Some(merged) = merged {
        merged.update(dict.as_mapping())?;
        dict.clear();
        dict.update(merged.as_mapping())?;
    }
    // Only empty merge sequences leave `merged` unset: nothing to merge.
    if let Some(set) = &mapping.set {
        for key in dict.keys().iter() {
            set.bind(py).add(key)?;
        }
    }
    Ok(())
}

fn merge_source<'py>(
    py: Python<'py>,
    source: &Bound<'py, PyAny>,
    merged: &mut Option<Bound<'py, PyDict>>,
    ctx: &MergeContext,
    visited: &mut [bool],
) -> PyResult<()> {
    if let Some(&source_index) = ctx.indices.get(&(source.as_ptr() as usize)) {
        finish_mapping(py, source_index, ctx, visited)?;
    }
    let source = source.cast::<PyDict>()?;
    match merged {
        Some(merged) => merged.update(source.as_mapping())?,
        None => *merged = Some(source.copy()?),
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

fn unknown_tag_message(tag: &str) -> String {
    format!("could not determine a constructor for the tag '{}'", tag)
}

fn single_document_error(py: Python) -> PyErr {
    exception::composer_error(
        py,
        "expected a single document in the stream, but found another document".to_string(),
    )
}

/// Whether `err` comes from reading or composing (PyYAML's first phase) rather
/// than from constructing Python objects.
fn is_compose_error(py: Python, err: &PyErr) -> bool {
    let Ok(module) = py.import("ryaml.error") else {
        return false;
    };
    [
        "ReaderError",
        "ScannerError",
        "ParserError",
        "ComposerError",
    ]
    .iter()
    .filter_map(|name| module.getattr(*name).ok())
    .any(|class| err.value(py).is_instance(&class).unwrap_or(false))
}

/// The PyYAML exception class for a libyaml-safer error, by the stage that
/// raised it (PyYAML raises `ParserError` for structural errors, for example).
fn parse_error(py: Python, error: &libyaml_safer::Error) -> PyErr {
    let class = match error.kind() {
        ErrorKind::Reader => "ReaderError",
        ErrorKind::Parser => "ParserError",
        ErrorKind::Composer => "ComposerError",
        ErrorKind::Scanner | ErrorKind::Emitter | ErrorKind::Io => "ScannerError",
    };
    exception::yaml_error(py, class, format!("{}", error))
}

/// Tags whose constructors take a scalar node.
fn is_scalar_tag(tag: &str) -> bool {
    matches!(
        tag,
        crate::TAG_NULL
            | crate::TAG_BOOL
            | crate::TAG_INT
            | crate::TAG_FLOAT
            | crate::TAG_BINARY
            | crate::TAG_TIMESTAMP
            | crate::TAG_STR
    )
}

/// A `=` mapping key, which PyYAML resolves to the `value` tag.
fn is_value_key(tag: Option<&str>, value: &str, plain_implicit: bool) -> bool {
    tag == Some(crate::TAG_VALUE)
        || (matches!(tag, None | Some("!")) && plain_implicit && value == "=")
}

fn expected_scalar_error(py: Python, found: &str) -> PyErr {
    exception::constructor_error(py, format!("expected a scalar node, but found {}", found))
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
        // Least significant part first, as PyYAML sums them. Its base is an
        // exact Python int, rounded to a float for each multiplication (which
        // raises OverflowError once it no longer fits).
        let mut result = 0.0;
        let mut base = Some(1u128);
        for (power, part) in remaining.rsplit(':').enumerate() {
            let digit = python_float(py, part)?;
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

impl RSafeLoader {
    pub fn from_text(source: String) -> Self {
        let mut parser = Parser::new();
        parser.set_input(Cursor::new(source));
        Self {
            parser,
            parsed_event: None,
            anchors: HashMap::with_hasher(FxBuildHasher),
            merge_anchors: HashSet::with_hasher(FxBuildHasher),
            value_alias_errors: HashMap::with_hasher(FxBuildHasher),
            merge_source_lists: HashMap::with_hasher(FxBuildHasher),
            scalar_anchor_values: HashMap::with_hasher(FxBuildHasher),
            deferred_scalars: HashMap::with_hasher(FxBuildHasher),
            skipped_anchors: HashSet::with_hasher(FxBuildHasher),
            scalar_mappings_in_progress: Vec::new(),
            pending_mappings: Vec::new(),
            set_mappings: HashMap::with_hasher(FxBuildHasher),
            pairs_mappings: HashMap::with_hasher(FxBuildHasher),
            anchored_mapping_lengths: HashMap::with_hasher(FxBuildHasher),
        }
    }
}

pub fn register_loader(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RSafeLoader>()?;
    Ok(())
}
