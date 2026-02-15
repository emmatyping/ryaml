//! Nodes representing YAML data, corresponding to PyYAML's nodes classes

use pyo3::prelude::*;

use crate::mark::PyMark;

#[derive(Debug, Clone)]
#[pyclass(name = "ScalarNode")]
pub struct PyScalarNode {
    #[pyo3(get)]
    pub tag: String,
    #[pyo3(get)]
    pub value: String,
    #[pyo3(get)]
    pub start_mark: Option<PyMark>,
    #[pyo3(get)]
    pub end_mark: Option<PyMark>,
    #[pyo3(get)]
    pub style: Option<char>,
}

#[pymethods]
impl PyScalarNode {
    #[new]
    pub fn new(
        tag: String,
        value: String,
        start_mark: Option<PyMark>,
        end_mark: Option<PyMark>,
        style: Option<char>,
    ) -> Self {
        Self {
            tag,
            value,
            start_mark,
            end_mark,
            style,
        }
    }

    #[getter]
    fn id(&self) -> &'static str {
        "scalar"
    }
}

#[derive(Debug, Clone)]
#[pyclass(name = "SequenceNode")]
pub struct PySequenceNode {
    #[pyo3(get)]
    pub tag: String,
    #[pyo3(get)]
    pub value: Vec<PyNode>,
    #[pyo3(get)]
    pub start_mark: Option<PyMark>,
    #[pyo3(get)]
    pub end_mark: Option<PyMark>,
    #[pyo3(get)]
    pub flow_style: Option<bool>,
}

#[pymethods]
impl PySequenceNode {
    #[new]
    pub fn new(
        tag: String,
        value: Vec<PyNode>,
        start_mark: Option<PyMark>,
        end_mark: Option<PyMark>,
        flow_style: Option<bool>,
    ) -> Self {
        Self {
            tag,
            value,
            start_mark,
            end_mark,
            flow_style,
        }
    }

    #[getter]
    fn id(&self) -> &'static str {
        "sequence"
    }
}

#[derive(Debug, Clone)]
#[pyclass(name = "MappingNode")]
pub struct PyMappingNode {
    #[pyo3(get)]
    pub tag: String,
    #[pyo3(get)]
    pub value: Vec<(PyNode, PyNode)>,
    #[pyo3(get)]
    pub start_mark: Option<PyMark>,
    #[pyo3(get)]
    pub end_mark: Option<PyMark>,
    #[pyo3(get)]
    pub flow_style: Option<bool>,
}

#[pymethods]
impl PyMappingNode {
    #[new]
    pub fn new(
        tag: String,
        value: Vec<(PyNode, PyNode)>,
        start_mark: Option<PyMark>,
        end_mark: Option<PyMark>,
        flow_style: Option<bool>,
    ) -> Self {
        Self {
            tag,
            value,
            start_mark,
            end_mark,
            flow_style,
        }
    }

    #[getter]
    fn id(&self) -> &'static str {
        "mapping"
    }
}

#[derive(Debug, Clone, FromPyObject)]
pub enum PyNode {
    Scalar(Py<PyScalarNode>),
    Sequence(Py<PySequenceNode>),
    Mapping(Py<PyMappingNode>),
}

impl PyNode {
    pub fn get_tag(&self, py: Python) -> PyResult<String> {
        match self {
            PyNode::Scalar(node) => Ok(node.borrow(py).tag.clone()),
            PyNode::Sequence(node) => Ok(node.borrow(py).tag.clone()),
            PyNode::Mapping(node) => Ok(node.borrow(py).tag.clone()),
        }
    }

    pub fn set_tag(&self, py: Python, tag: String) -> PyResult<()> {
        match self {
            PyNode::Scalar(node) => {
                node.borrow_mut(py).tag = tag;
            }
            PyNode::Sequence(node) => {
                node.borrow_mut(py).tag = tag;
            }
            PyNode::Mapping(node) => {
                node.borrow_mut(py).tag = tag;
            }
        }
        Ok(())
    }

    pub fn get_start_mark(&self, py: Python) -> PyResult<Option<PyMark>> {
        match self {
            PyNode::Scalar(node) => Ok(node.borrow(py).start_mark.clone()),
            PyNode::Sequence(node) => Ok(node.borrow(py).start_mark.clone()),
            PyNode::Mapping(node) => Ok(node.borrow(py).start_mark.clone()),
        }
    }
}

impl<'py> IntoPyObject<'py> for PyNode {
    type Target = PyAny;

    type Output = Bound<'py, Self::Target>;

    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        match self {
            PyNode::Scalar(node) => Ok(node.bind(py).as_any().clone()),
            PyNode::Sequence(node) => Ok(node.bind(py).as_any().clone()),
            PyNode::Mapping(node) => Ok(node.bind(py).as_any().clone()),
        }
    }
}

pub fn register_nodes(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyScalarNode>()?;
    m.add_class::<PySequenceNode>()?;
    m.add_class::<PyMappingNode>()?;
    Ok(())
}
