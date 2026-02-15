use pyo3::prelude::*;
use pyo3::types::PyType;

pyo3::create_exception!(_ryaml, InvalidYamlError, pyo3::exceptions::PyValueError);

/// Raise one of the exception classes defined in ``ryaml.error``.
///
/// Falls back to ``InvalidYamlError`` if the import fails (e.g. the pure-Python
/// package has not been installed alongside the native extension).
pub fn yaml_error(py: Python, class_name: &str, message: String) -> PyErr {
    if let Ok(module) = py.import("ryaml.error")
        && let Ok(attr) = module.getattr(class_name)
        && let Ok(tp) = attr.downcast_into::<PyType>()
    {
        return PyErr::from_type(tp, (message,));
    }
    InvalidYamlError::new_err(message)
}

pub fn scanner_error(py: Python, message: String) -> PyErr {
    yaml_error(py, "ScannerError", message)
}

pub fn composer_error(py: Python, message: String) -> PyErr {
    yaml_error(py, "ComposerError", message)
}

pub fn constructor_error(py: Python, message: String) -> PyErr {
    yaml_error(py, "ConstructorError", message)
}

pub fn emitter_error(py: Python, message: String) -> PyErr {
    yaml_error(py, "EmitterError", message)
}

pub fn serializer_error(py: Python, message: String) -> PyErr {
    yaml_error(py, "SerializerError", message)
}

pub fn representer_error(py: Python, message: String) -> PyErr {
    yaml_error(py, "RepresenterError", message)
}
