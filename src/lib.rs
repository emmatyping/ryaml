pyo3::create_exception!(_ryaml, InvalidYamlError, pyo3::exceptions::PyValueError);

#[pyo3::pymodule(gil_used = false)]
mod _ryaml {
    use std::io::{Read, Write};

    use pyo3::Python;
    use pyo3::types::PyList;
    use pyo3::{exceptions::PyValueError, prelude::*};
    use pyo3_file::PyFileLikeObject;

    use pythonize::{depythonize, pythonize};
    use serde::Deserialize;
    use serde_yaml::Value;

    #[pymodule_export]
    use super::InvalidYamlError;

    fn read_file(file: Py<PyAny>) -> PyResult<String> {
        match PyFileLikeObject::with_requirements(file, true, false, false, false) {
            Ok(mut f) => {
                // If the file is in text mode pyo3-file wants a buffer with
                // at least 4 bytes, which means we cannot call `read_to_string`
                // Instead, we need to manually read the file ourselves.
                let mut working_buffer = Vec::new();
                let mut read_buffer = [0; 4096];
                loop {
                    let size = f.read(&mut read_buffer)?;
                    if size == 0 {
                        break;
                    }
                    working_buffer.extend_from_slice(&read_buffer[..size]);
                }
                Ok(String::from_utf8(working_buffer)?)
            }
            Err(_) => Err(PyValueError::new_err(
                "Argument 1 not a readable file-like object.",
            )),
        }
    }

    fn write_file(file: Py<PyAny>, str: String) -> PyResult<()> {
        match PyFileLikeObject::with_requirements(file, false, true, false, false) {
            Ok(mut f) => Ok(f.write_all(str.as_bytes())?),
            Err(_) => Err(PyValueError::new_err(
                "Argument 1 not a writable file-like object.",
            )),
        }
    }

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
    fn load(py: Python, file: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let str = read_file(file)?;
        loads(py, str)
    }

    #[pyfunction]
    fn load_all(py: Python, file: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let str = read_file(file)?;
        loads_all(py, str)
    }

    #[pyfunction]
    fn dump(py: Python, file: Py<PyAny>, obj: Py<PyAny>) -> PyResult<()> {
        let str = dumps(py, obj)?;
        write_file(file, str)
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
