pyo3::create_exception!(_ryaml, InvalidYamlError, pyo3::exceptions::PyValueError);

#[pyo3::pymodule(gil_used = false)]
mod _ryaml {

    use pyo3::Python;
    use pyo3::types::PyList;
    use pyo3::{exceptions::PyValueError, prelude::*};

    use pythonize::{depythonize, pythonize};
    use serde::Deserialize;
    use serde_yaml::Value;

    #[pymodule_export]
    use super::InvalidYamlError;

    fn deserialize_yaml(str: String) -> PyResult<Value> {
        match serde_yaml::from_str(&str) {
            Ok(val) => Ok(val),
            Err(err) => Err(InvalidYamlError::new_err(err.to_string())),
        }
    }

    fn deserialize_all_yaml(str: String) -> PyResult<Vec<Value>> {
        let mut documents = vec![];
        for document in serde_yaml::Deserializer::from_str(&str) {
            match Value::deserialize(document) {
                Ok(value) => documents.push(value),
                Err(e) => return Err(InvalidYamlError::new_err(e.to_string())),
            }
        }
        Ok(documents)
    }

    fn serialize_yaml(yaml: &Value) -> PyResult<String> {
        match serde_yaml::to_string(&yaml) {
            Ok(s) => Ok(s),
            Err(err) => Err(PyValueError::new_err(err.to_string())),
        }
    }

    fn yaml_to_pyobject(py: Python, yaml: &Value) -> PyResult<Py<PyAny>> {
        match pythonize(py, yaml) {
            Ok(obj) => Ok(obj.unbind()),
            Err(err) => Err(PyValueError::new_err(err.to_string())),
        }
    }

    fn pyobject_to_yaml(py: Python, obj: Py<PyAny>) -> PyResult<Value> {
        match depythonize(obj.bind(py)) {
            Ok(obj) => Ok(obj),
            Err(err) => Err(PyValueError::new_err(err.to_string())),
        }
    }

    #[pyfunction]
    fn loads(py: Python, str: String) -> PyResult<Py<PyAny>> {
        if str.is_empty() {
            Ok(Python::None(py))
        } else {
            let value = deserialize_yaml(str)?;
            yaml_to_pyobject(py, &value)
        }
    }

    #[pyfunction]
    fn loads_all(py: Python, str: String) -> PyResult<Py<PyAny>> {
        if str.is_empty() {
            Ok(Python::None(py))
        } else {
            let documents = deserialize_all_yaml(str)?;
            let mut pydocs = vec![];
            for doc in documents {
                pydocs.push(yaml_to_pyobject(py, &doc)?);
            }
            Ok(PyList::new(py, pydocs)?.into_any().unbind())
        }
    }

    #[pyfunction]
    fn dumps(py: Python, obj: Py<PyAny>) -> PyResult<String> {
        let yaml = pyobject_to_yaml(py, obj)?;
        serialize_yaml(&yaml)
    }
}
