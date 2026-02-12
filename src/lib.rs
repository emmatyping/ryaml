mod dumper;
mod exception;
mod loader;
mod mark;
mod nodes;
mod resolver;

#[pyo3::pymodule(gil_used = false)]
mod _ryaml {

    use pyo3::Python;
    use pyo3::prelude::*;
    use pyo3::types::PyList;

    use crate::dumper::register_dumper;
    use crate::loader::register_loader;
    use crate::mark::register_mark;
    use crate::nodes::register_nodes;

    #[pymodule_export]
    use crate::exception::InvalidYamlError;

    #[pymodule_export]
    use crate::loader::RSafeLoader;

    #[pymodule_export]
    use crate::dumper::RSafeDumper;

    #[pymodule_export]
    use crate::mark::PyMark;

    #[pymodule_export]
    use crate::nodes::PyScalarNode;

    #[pymodule_export]
    use crate::nodes::PySequenceNode;

    #[pymodule_export]
    use crate::nodes::PyMappingNode;

    #[pyfunction]
    fn loads(py: Python, str: String) -> PyResult<Option<Py<PyAny>>> {
        RSafeLoader::new(str).get_single_data(py)
    }

    #[pyfunction]
    fn loads_all(py: Python, str: String) -> PyResult<Option<Py<PyAny>>> {
        if str.is_empty() {
            Ok(Some(Python::None(py)))
        } else {
            let mut loader = RSafeLoader::new(str);
            let mut docs = Vec::new();
            while loader.check_data()? {
                docs.push(loader.get_data(py)?)
            }
            Ok(Some(PyList::new(py, docs)?.into()))
        }
    }

    #[pyfunction]
    fn dumps(py: Python, obj: Py<PyAny>) -> PyResult<String> {
        crate::dumper::dumps_to_string(py, obj.bind(py))
    }

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        register_nodes(m)?;
        register_loader(m)?;
        register_mark(m)?;
        register_dumper(m)?;
        Ok(())
    }
}
