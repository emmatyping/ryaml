//! Mark Python class which is duck-type compatible with pyyaml's Mark type.

use pyo3::prelude::*;

#[derive(Debug, Clone)]
#[pyclass(name = "Mark")]
pub struct PyMark {
    #[pyo3(get)]
    pub index: u64,
    #[pyo3(get)]
    pub line: u64,
    #[pyo3(get)]
    pub column: u64,
}

#[pymethods]
impl PyMark {
    #[new]
    pub fn new(index: u64, line: u64, column: u64) -> Self {
        Self {
            index,
            line,
            column,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "<Mark index={} line={} column={}>",
            self.index, self.line, self.column
        )
    }
}

impl From<libyaml_safer::Mark> for PyMark {
    fn from(mark: libyaml_safer::Mark) -> Self {
        Self {
            index: mark.index,
            line: mark.line,
            column: mark.column,
        }
    }
}

pub fn register_mark(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMark>()?;
    Ok(())
}
