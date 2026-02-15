mod dumper;
mod exception;
mod loader;
mod mark;
mod nodes;
mod resolver;

const TAG_NULL: &str = "tag:yaml.org,2002:null";
const TAG_BOOL: &str = "tag:yaml.org,2002:bool";
const TAG_INT: &str = "tag:yaml.org,2002:int";
const TAG_FLOAT: &str = "tag:yaml.org,2002:float";
const TAG_STR: &str = "tag:yaml.org,2002:str";
const TAG_BINARY: &str = "tag:yaml.org,2002:binary";
const TAG_TIMESTAMP: &str = "tag:yaml.org,2002:timestamp";
const TAG_SEQ: &str = "tag:yaml.org,2002:seq";
const TAG_MAP: &str = "tag:yaml.org,2002:map";
const TAG_SET: &str = "tag:yaml.org,2002:set";
const TAG_MERGE: &str = "tag:yaml.org,2002:merge";
const TAG_VALUE: &str = "tag:yaml.org,2002:value";

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
            while loader.check_data(py)? {
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
