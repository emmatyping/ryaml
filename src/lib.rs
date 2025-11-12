use pyo3::{create_exception, exceptions::PyValueError};


mod serde_bridge {

    use pyo3::prelude::*;
    use pythonize::Depythonizer;
    use serde::{Deserialize, Serialize};
    pub struct SerdeBridge {
        data: Py<PyAny>,
    }

    impl SerdeBridge {
        pub fn new(obj: Py<PyAny>) -> Self {
            Self {
                data: obj,
            }
        }

        pub fn into_inner(self) -> Py<PyAny> {
            self.data
        }
    }

    // TODO(emmatyping): try using pythonize_custom and override map behavior to handle null map values

    impl<'de> Deserialize<'de> for SerdeBridge {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de> {
            let transcoder = serde_transcode::Transcoder::new(deserializer);
            Python::attach(|py| {
                Ok(SerdeBridge { data: pythonize::pythonize(py, &transcoder).unwrap().unbind() })
            })
        }
    }

    impl Serialize for SerdeBridge {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer {
            Python::attach(|py| {
                let mut deserializer = Depythonizer::from_object(self.data.bind(py));
                let transcoder = serde_transcode::Transcoder::new(&mut deserializer);
                transcoder.serialize(serializer)
            })
        }
    }
}

#[pyo3::pymodule(gil_used = false)]
mod ryaml {
    use std::io::{Read, Write};

    use pyo3::types::PyList;
    use pyo3::Python;
    use pyo3::{exceptions::PyValueError, prelude::*};
    use pyo3_file::PyFileLikeObject;

    #[pymodule_export]
    use crate::InvalidYamlError;

    use crate::serde_bridge::SerdeBridge;

    fn read_file(file: Py<PyAny>) -> PyResult<String> {
        match PyFileLikeObject::with_requirements(file, true, false, false, false) {
            Ok(mut f) => {
                let mut str = String::new();
                f.read_to_string(&mut str)?;
                Ok(str)
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
    fn dump(file: Py<PyAny>, obj: Py<PyAny>) -> PyResult<()> {
        let str = dumps(obj)?;
        write_file(file, str)
    }

    #[pyfunction]
    fn loads(py: Python, str: String) -> PyResult<Py<PyAny>> {
        if str.is_empty() {
            Ok(Python::None(py))
        } else {
            match serde_saphyr::from_str::<SerdeBridge>(&str) {
                Ok(v) => Ok(v.into_inner()),
                Err(e) => Err(InvalidYamlError::new_err(e.to_string()))
            }
        }
    }

    #[pyfunction]
    fn loads_all(py: Python, str: String) -> PyResult<Py<PyAny>> {
        if str.is_empty() {
            Ok(Python::None(py))
        } else {
            match serde_saphyr::from_multiple::<SerdeBridge>(&str) {
                Ok(v) => {
                    let objs = v.into_iter().map(|i: SerdeBridge| i.into_inner());
                    PyList::new(py, objs).map(|l| l.unbind().into_any())
                },
                Err(e) => Err(InvalidYamlError::new_err(e.to_string()))
            }
        }
    }

    #[pyfunction]
    fn dumps(obj: Py<PyAny>) -> PyResult<String> {
        let bridge = vec![SerdeBridge::new(obj)];
        serde_saphyr::to_string_multiple(&bridge).map_err(|e| InvalidYamlError::new_err(e.to_string()))
    }
}

create_exception!(ryaml, InvalidYamlError, PyValueError);