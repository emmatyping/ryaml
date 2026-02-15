//! Module implement pyyaml compatibility layer for ryaml via libyaml
//! Implements RLoader, which can load YAML 1.1

use libyaml_safer::{
    Event, EventData, MappingStyle, Parser, ScalarStyle, SequenceStyle,
};
use pyo3::exceptions::PyNotImplementedError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};
use rustc_hash::{FxBuildHasher, FxHashMap};
use std::collections::HashMap;
use std::io::Cursor;

use crate::exception;
use crate::nodes::{PyMappingNode, PyNode, PyScalarNode, PySequenceNode};
use crate::resolver;

#[allow(dead_code)]
#[pyclass(name = "_RSafeLoader", subclass)]
pub struct RSafeLoader {
    /// Parser over an in-memory string passed by Python
    parser: Parser<Cursor<String>>,
    /// Event requested by Python functions
    current_event: Option<Event>,
    /// Event used by internal parser
    parsed_event: Option<Event>,
    /// Anchors for node composition (maps anchor name to node)
    anchors: FxHashMap<String, PyNode>,
    /// Constructed objects (maps node id to Python object)
    constructed_objects: FxHashMap<usize, Py<PyAny>>,
    /// Recursive objects being constructed
    recursive_objects: FxHashMap<usize, bool>,
    /// Node ID counter
    next_node_id: usize,
    /// Map nodes to IDs
    node_ids: FxHashMap<usize, usize>,
}

#[pymethods]
impl RSafeLoader {
    #[new]
    pub fn new(source: String) -> Self {
        let mut parser = Parser::new();
        parser.set_input(Cursor::new(source));
        Self {
            parser,
            current_event: None,
            parsed_event: None,
            anchors: HashMap::with_hasher(FxBuildHasher),
            constructed_objects: HashMap::with_hasher(FxBuildHasher),
            recursive_objects: HashMap::with_hasher(FxBuildHasher),
            next_node_id: 0,
            node_ids: HashMap::with_hasher(FxBuildHasher),
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
        if self.check_node(py)?
            && let Some(node) = self.get_node(py)?
        {
            return Ok(Some(self.construct_document(py, node)?));
        }
        Ok(None)
    }

    /// Get a single document as a Python object
    pub fn get_single_data(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        if let Some(node) = self.get_single_node(py)? {
            return Ok(Some(self.construct_document(py, node)?));
        }
        Ok(None)
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

    fn get_node(&mut self, py: Python) -> PyResult<Option<PyNode>> {
        self._parse_next_event(py)?;
        if matches!(
            &self.parsed_event,
            Some(Event {
                data: EventData::StreamEnd,
                ..
            })
        ) {
            return Ok(None);
        }
        self._compose_document(py)
    }

    fn get_single_node(&mut self, py: Python) -> PyResult<Option<PyNode>> {
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
            self._compose_document(py)?
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

    /// Compose a document from events
    fn _compose_document(&mut self, py: Python) -> PyResult<Option<PyNode>> {
        // Eat document start event
        self.parsed_event = None;

        // Compose the root node
        let node = self._compose_node(py)?;

        // Eat document end event
        self._parse_next_event(py)?;
        self.parsed_event = None;

        // Clear anchors for next document
        self.anchors.clear();

        Ok(Some(node))
    }

    /// Compose a node from events
    fn _compose_node(&mut self, py: Python) -> PyResult<PyNode> {
        self._parse_next_event(py)?;
        let event = self.parsed_event.as_ref().unwrap();

        match &event.data {
            EventData::Alias { anchor } => {
                let anchor_str = anchor.clone();
                if let Some(node) = self.anchors.get(&anchor_str) {
                    self.parsed_event = None;
                    Ok(node.clone())
                } else {
                    Err(exception::composer_error(
                        py,
                        format!("found undefined alias '{}'", anchor_str),
                    ))
                }
            }
            EventData::Scalar { .. } => {
                let node = self._compose_scalar_node(py)?;
                Ok(node)
            }
            EventData::SequenceStart { .. } => {
                let node = self._compose_sequence_node(py)?;
                Ok(node)
            }
            EventData::MappingStart { .. } => {
                let node = self._compose_mapping_node(py)?;
                Ok(node)
            }
            _ => Err(exception::composer_error(
                py,
                format!("unexpected event: {:?}", event.data),
            )),
        }
    }

    /// Compose a scalar node
    fn _compose_scalar_node(&mut self, py: Python) -> PyResult<PyNode> {
        let event = self.parsed_event.as_ref().unwrap();

        if let EventData::Scalar {
            anchor,
            tag,
            value,
            plain_implicit,
            quoted_implicit,
            style,
        } = &event.data
        {
            let anchor_str = anchor.clone();
            let value_str = value.clone();

            // Determine the tag
            let resolved_tag = if let Some(t) = tag {
                t.clone()
            } else {
                // Use resolver to determine tag
                self.resolve_scalar_tag(&value_str, *plain_implicit, *quoted_implicit)
            };

            let style_char = match style {
                ScalarStyle::Plain => None,
                ScalarStyle::SingleQuoted => Some('\''),
                ScalarStyle::DoubleQuoted => Some('"'),
                ScalarStyle::Literal => Some('|'),
                ScalarStyle::Folded => Some('>'),
                _ => None,
            };

            let node = Py::new(
                py,
                PyScalarNode::new(resolved_tag, value_str, None, None, style_char),
            )?;
            let py_node = PyNode::Scalar(node);

            // Store anchor if present
            if let Some(anchor_name) = anchor_str {
                self.anchors.insert(anchor_name, py_node.clone());
            }

            self.parsed_event = None;
            Ok(py_node)
        } else {
            unreachable!()
        }
    }

    /// Compose a sequence node
    fn _compose_sequence_node(&mut self, py: Python) -> PyResult<PyNode> {
        let event = self.parsed_event.as_ref().unwrap();

        if let EventData::SequenceStart {
            anchor,
            tag,
            implicit: _,
            style,
        } = &event.data
        {
            let anchor_str = anchor.clone();

            // Determine the tag
            let resolved_tag = if let Some(t) = tag {
                t.clone()
            } else {
                "tag:yaml.org,2002:seq".to_string()
            };

            let flow_style = match style {
                SequenceStyle::Flow => Some(true),
                SequenceStyle::Block => Some(false),
                _ => None,
            };

            // Create the node with empty value first
            let node = Py::new(
                py,
                PySequenceNode::new(resolved_tag, vec![], None, None, flow_style),
            )?;
            let py_node = PyNode::Sequence(node.clone_ref(py));

            // Store anchor before recursing to handle circular references
            if let Some(anchor_name) = anchor_str {
                self.anchors.insert(anchor_name, py_node.clone());
            }

            // Eat the sequence start event
            self.parsed_event = None;

            // Compose child nodes
            let mut children = vec![];
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
                children.push(self._compose_node(py)?);
            }

            // Update the node with the children
            node.borrow_mut(py).value = children;

            // Eat the sequence end event
            self.parsed_event = None;

            Ok(py_node)
        } else {
            unreachable!()
        }
    }

    /// Compose a mapping node
    fn _compose_mapping_node(&mut self, py: Python) -> PyResult<PyNode> {
        let event = self.parsed_event.as_ref().unwrap();

        if let EventData::MappingStart {
            anchor,
            tag,
            implicit: _,
            style,
        } = &event.data
        {
            let anchor_str = anchor.clone();

            // Determine the tag
            let resolved_tag = if let Some(t) = tag {
                t.clone()
            } else {
                "tag:yaml.org,2002:map".to_string()
            };

            let flow_style = match style {
                MappingStyle::Flow => Some(true),
                MappingStyle::Block => Some(false),
                _ => None,
            };

            // Create the node with empty value first
            let node = Py::new(
                py,
                PyMappingNode::new(resolved_tag, vec![], None, None, flow_style),
            )?;
            let py_node = PyNode::Mapping(node.clone_ref(py));

            // Store anchor before recursing to handle circular references
            if let Some(anchor_name) = anchor_str {
                self.anchors.insert(anchor_name, py_node.clone());
            }

            // Eat the mapping start event
            self.parsed_event = None;

            // Compose key-value pairs
            let mut pairs = vec![];
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
                let key = self._compose_node(py)?;
                let value = self._compose_node(py)?;
                pairs.push((key, value));
            }

            // Update the node with the pairs
            node.borrow_mut(py).value = pairs;

            // Eat the mapping end event
            self.parsed_event = None;

            Ok(py_node)
        } else {
            unreachable!()
        }
    }

    /// Resolve tag for a scalar based on its value and implicit flags
    fn resolve_scalar_tag(
        &self,
        value: &str,
        plain_implicit: bool,
        _quoted_implicit: bool,
    ) -> String {
        resolver::resolve_scalar_tag(value, plain_implicit).to_string()
    }

    /// Construct a document from a node
    fn construct_document(&mut self, py: Python, node: PyNode) -> PyResult<Py<PyAny>> {
        let data = self.construct_object(py, node)?;

        // Clear state for next document
        self.constructed_objects.clear();
        self.recursive_objects.clear();

        Ok(data)
    }

    /// Get a unique ID for a node
    fn get_node_id(&mut self, node: &PyNode) -> usize {
        let ptr = match node {
            PyNode::Scalar(n) => n.as_ptr() as usize,
            PyNode::Sequence(n) => n.as_ptr() as usize,
            PyNode::Mapping(n) => n.as_ptr() as usize,
        };

        *self.node_ids.entry(ptr).or_insert_with(|| {
            let id = self.next_node_id;
            self.next_node_id += 1;
            id
        })
    }

    /// Construct a Python object from a node (SafeConstructor implementation)
    fn construct_object(&mut self, py: Python, node: PyNode) -> PyResult<Py<PyAny>> {
        let node_id = self.get_node_id(&node);

        // Check if already constructed
        if let Some(obj) = self.constructed_objects.get(&node_id) {
            return Ok(obj.clone_ref(py));
        }

        // Check for recursive construction
        if self.recursive_objects.contains_key(&node_id) {
            return Err(exception::constructor_error(
                py,
                "found unconstructable recursive node".to_string(),
            ));
        }

        self.recursive_objects.insert(node_id, true);

        // Get the tag and construct based on it
        let tag = node.get_tag(py)?;
        let result = match tag.as_str() {
            "tag:yaml.org,2002:null" => self.construct_yaml_null(py, &node),
            "tag:yaml.org,2002:bool" => self.construct_yaml_bool(py, &node),
            "tag:yaml.org,2002:int" => self.construct_yaml_int(py, &node),
            "tag:yaml.org,2002:float" => self.construct_yaml_float(py, &node),
            "tag:yaml.org,2002:str" => self.construct_yaml_str(py, &node),
            "tag:yaml.org,2002:seq" => self.construct_yaml_seq(py, &node),
            "tag:yaml.org,2002:map" => self.construct_yaml_map(py, &node),
            "tag:yaml.org,2002:set" => self.construct_yaml_set(py, &node),
            "tag:yaml.org,2002:timestamp" => self.construct_yaml_timestamp(py, &node),
            "tag:yaml.org,2002:merge" => {
                // Merge should be handled in flatten_mapping, not here
                Ok(py.None())
            }
            "tag:yaml.org,2002:value" => {
                // Value tag - extract the value from mapping
                self.construct_scalar(py, &node)
            }
            _ => {
                // Unknown tag - construct based on node type
                match &node {
                    PyNode::Scalar(_) => self.construct_yaml_str(py, &node),
                    PyNode::Sequence(_) => self.construct_yaml_seq(py, &node),
                    PyNode::Mapping(_) => self.construct_yaml_map(py, &node),
                }
            }
        }?;

        self.constructed_objects
            .insert(node_id, result.clone_ref(py));
        self.recursive_objects.remove(&node_id);

        Ok(result)
    }

    /// Extract scalar value from a node
    fn construct_scalar(&self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        match node {
            PyNode::Scalar(n) => Ok(PyString::new(py, &n.borrow(py).value).into_any().unbind()),
            PyNode::Mapping(n) => {
                // Handle value tag in mapping
                for (key_node, value_node) in &n.borrow(py).value {
                    if let Ok(tag) = key_node.get_tag(py)
                        && tag == "tag:yaml.org,2002:value"
                    {
                        return self.construct_scalar(py, value_node);
                    }
                }
                Err(exception::constructor_error(
                    py,
                    "expected a scalar node, but found mapping".to_string(),
                ))
            }
            _ => Err(exception::constructor_error(
                py,
                "expected a scalar node, but found sequence".to_string(),
            )),
        }
    }

    /// Construct null value
    fn construct_yaml_null(&self, py: Python, _node: &PyNode) -> PyResult<Py<PyAny>> {
        Ok(py.None())
    }

    /// Construct boolean value
    fn construct_yaml_bool(&self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        let value = self.construct_scalar(py, node)?.extract::<String>(py)?;
        let bool_val = match value.to_lowercase().as_str() {
            "yes" | "true" | "on" => true,
            "no" | "false" | "off" => false,
            _ => {
                return Err(exception::constructor_error(
                    py,
                    format!("invalid boolean value: {}", value),
                ));
            }
        };
        Ok(PyBool::new(py, bool_val).as_any().clone().unbind())
    }

    fn construct_yaml_int_fallback(&self, py: Python, mut value: String) -> PyResult<i64> {
        value = value.replace('_', "");

        let mut sign = 1i64;
        if value.starts_with('-') {
            sign = -1;
            value = value[1..].to_string();
        } else if value.starts_with('+') {
            value = value[1..].to_string();
        }

        let result = if value == "0" {
            0
        } else if let Some(bin) = value.strip_prefix("0b") {
            i64::from_str_radix(bin, 2).map_err(|e| {
                exception::constructor_error(py, format!("invalid binary integer: {}", e))
            })?
        } else if let Some(hex) = value.strip_prefix("0x") {
            i64::from_str_radix(hex, 16).map_err(|e| {
                exception::constructor_error(py, format!("invalid hex integer: {}", e))
            })?
        } else if value.starts_with('0') && !value.contains(':') {
            i64::from_str_radix(&value, 8).map_err(|e| {
                exception::constructor_error(py, format!("invalid octal integer: {}", e))
            })?
        } else if value.contains(':') {
            // Sexagesimal (base 60)
            let parts: Vec<&str> = value.split(':').collect();
            let mut result = 0i64;
            let mut base = 1i64;
            for part in parts.iter().rev() {
                let digit = part.parse::<i64>().map_err(|e| {
                    exception::constructor_error(py, format!("invalid sexagesimal: {}", e))
                })?;
                result += digit * base;
                base *= 60;
            }
            result
        } else {
            value
                .parse::<i64>()
                .map_err(|e| exception::constructor_error(py, format!("invalid integer: {}", e)))?
        };
        Ok(sign * result)
    }

    /// Construct integer value
    fn construct_yaml_int(&self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        let value = self.construct_scalar(py, node)?.extract::<String>(py)?;
        let i = match value.parse::<i64>() {
            Ok(v) => v,
            Err(_) => self.construct_yaml_int_fallback(py, value)?,
        };
        Ok(PyInt::new(py, i).into_any().unbind())
    }

    fn construct_yaml_float_fallback(&self, py: Python, mut value: String) -> PyResult<f64> {
        value = value.replace('_', "").to_lowercase();

        let mut sign = 1.0f64;
        if value.starts_with('-') {
            sign = -1.0;
            value = value[1..].to_string();
        } else if value.starts_with('+') {
            value = value[1..].to_string();
        }

        let result = if value == ".inf" {
            f64::INFINITY
        } else if value == ".nan" {
            f64::NAN
        } else if value.contains(':') {
            // Sexagesimal float
            let parts: Vec<&str> = value.split(':').collect();
            let mut result = 0.0f64;
            let mut base = 1.0f64;
            for part in parts.iter().rev() {
                let digit = part.parse::<f64>().map_err(|e| {
                    exception::constructor_error(py, format!("invalid sexagesimal float: {}", e))
                })?;
                result += digit * base;
                base *= 60.0;
            }
            result
        } else {
            value
                .parse::<f64>()
                .map_err(|e| exception::constructor_error(py, format!("invalid float: {}", e)))?
        };
        Ok(sign * result)
    }

    /// Construct float value
    fn construct_yaml_float(&self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        let value = self.construct_scalar(py, node)?.extract::<String>(py)?;
        let f = match value.parse::<f64>() {
            Ok(v) => v,
            Err(_) => self.construct_yaml_float_fallback(py, value)?
        };

        Ok(PyFloat::new(py, f).into_any().unbind())
    }

    /// Construct string value
    fn construct_yaml_str(&self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        self.construct_scalar(py, node)
    }

    /// Construct sequence (list) value
    fn construct_yaml_seq(&mut self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        match node {
            PyNode::Sequence(n) => {
                let list = PyList::empty(py);
                let node_id = self.get_node_id(node);

                // Store the list object before recursing to handle circular references
                self.constructed_objects
                    .insert(node_id, list.clone().unbind().into_any());

                for child in &n.borrow(py).value {
                    let item = self.construct_object(py, child.clone())?;
                    list.append(item)?;
                }
                Ok(list.unbind().into_any())
            }
            _ => Err(exception::constructor_error(
                py,
                "expected a sequence node, but found mapping or scalar".to_string(),
            )),
        }
    }

    /// Construct mapping (dict) value with merge key support
    fn construct_yaml_map(&mut self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        match node {
            PyNode::Mapping(n) => {
                // Flatten merge keys first
                let node_clone = node.clone();
                self.flatten_mapping(py, &node_clone)?;

                let dict = PyDict::new(py);
                let node_id = self.get_node_id(node);

                // Store the dict object before recursing to handle circular references
                self.constructed_objects
                    .insert(node_id, dict.clone().unbind().into_any());

                for (key_node, value_node) in &n.borrow(py).value {
                    // Skip merge keys as they've been flattened
                    if let Ok(tag) = key_node.get_tag(py) {
                        if tag == "tag:yaml.org,2002:merge" {
                            continue;
                        }
                        // Convert value tag keys to str
                        if tag == "tag:yaml.org,2002:value" {
                            key_node.set_tag(py, "tag:yaml.org,2002:str".to_string())?;
                        }
                    }

                    let key = self.construct_object(py, key_node.clone())?;
                    let value = self.construct_object(py, value_node.clone())?;

                    // Convert unhashable keys to tuples
                    let hashable_key = self.make_hashable(py, key)?;
                    dict.set_item(hashable_key, value)?;
                }
                Ok(dict.unbind().into_any())
            }
            _ => Err(exception::constructor_error(
                py,
                "expected a mapping node, but found sequence or scalar".to_string(),
            )),
        }
    }

    /// Construct set value (represented as dict with None values for JSON compatibility)
    fn construct_yaml_set(&mut self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        match node {
            PyNode::Mapping(n) => {
                let dict = PyDict::new(py);
                for (key_node, _value_node) in &n.borrow(py).value {
                    let key = self.construct_object(py, key_node.clone())?;
                    // Convert unhashable keys to tuples
                    let hashable_key = self.make_hashable(py, key)?;
                    dict.set_item(hashable_key, py.None())?;
                }
                Ok(dict.unbind().into_any())
            }
            _ => Err(exception::constructor_error(
                py,
                "expected a mapping node for set".to_string(),
            )),
        }
    }

    /// Construct timestamp value
    fn construct_yaml_timestamp(&self, py: Python, node: &PyNode) -> PyResult<Py<PyAny>> {
        // For now, just return as string
        // TODO: Parse into datetime object
        self.construct_scalar(py, node)
    }

    /// Flatten merge keys in a mapping (SafeConstructor.flatten_mapping)
    fn flatten_mapping(&mut self, py: Python, node: &PyNode) -> PyResult<()> {
        match node {
            PyNode::Mapping(n) => {
                let mut merge_pairs = vec![];

                let value = n.borrow(py).value.clone();
                let mut new_value = vec![];

                for (key_node, value_node) in value {
                    if let Ok(tag) = key_node.get_tag(py)
                        && tag == "tag:yaml.org,2002:merge"
                    {
                        // Process merge
                        match &value_node {
                            PyNode::Mapping(_) => {
                                self.flatten_mapping(py, &value_node)?;
                                if let PyNode::Mapping(m) = &value_node {
                                    merge_pairs.extend(m.borrow(py).value.clone());
                                }
                            }
                            PyNode::Sequence(s) => {
                                for subnode in s.borrow(py).value.iter().rev() {
                                    if let PyNode::Mapping(_) = subnode {
                                        self.flatten_mapping(py, subnode)?;
                                        if let PyNode::Mapping(m) = subnode {
                                            merge_pairs.extend(m.borrow(py).value.clone());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue; // Don't add merge key to result
                    }
                    new_value.push((key_node, value_node));
                }

                // Add merged pairs at the beginning (so they're overridden by later keys)
                merge_pairs.extend(new_value);
                n.borrow_mut(py).value = merge_pairs;

                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Convert unhashable types (dict, list) to tuples for use as dict keys
    fn make_hashable(&self, py: Python, obj: Py<PyAny>) -> PyResult<Py<PyAny>> {
        // Check if it's a dict
        if let Ok(dict) = obj.downcast_bound::<PyDict>(py) {
            // Convert dict to a tuple of tuples: ((k1, v1), (k2, v2), ...)
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

        // Check if it's a list
        if let Ok(list) = obj.downcast_bound::<PyList>(py) {
            // Convert list to tuple
            let mut items = Vec::new();
            for item in list.iter() {
                let hashable_item = self.make_hashable(py, item.unbind())?;
                items.push(hashable_item);
            }
            let tuple = pyo3::types::PyTuple::new(py, &items)?;
            return Ok(tuple.unbind().into_any());
        }

        // If already hashable, return as is
        Ok(obj)
    }
}

pub fn register_loader(m: &Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RSafeLoader>()?;
    Ok(())
}
