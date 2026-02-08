pyo3::create_exception!(_ryaml, InvalidYamlError, pyo3::exceptions::PyValueError);

mod loader;
mod exception;
mod event;

#[pyo3::pymodule(gil_used = false)]
mod _ryaml {

    use pyo3::Python;
    use pyo3::prelude::*;

    #[pyfunction]
    fn loads(py: Python, str: String) -> PyResult<Py<PyAny>> {
        unimplemented!()
    }

    #[pyfunction]
    fn loads_all(py: Python, str: String) -> PyResult<Py<PyAny>> {
        unimplemented!()
    }

    #[pyfunction]
    fn dumps(py: Python, obj: Py<PyAny>) -> PyResult<String> {
        unimplemented!()
    }
}
