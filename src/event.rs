//! Loads YAML from the parser backend into Python objects

use std::{collections::HashMap, sync::LazyLock};

use libyaml_safer::{Event, EventData};
use pyo3::prelude::*;

// To speed up instatiating these classes, cache the type objects
static EVENT_TYPES: LazyLock<HashMap<String, Py<PyAny>>> = LazyLock::new(|| {
    let map = HashMap::new();
    // TODO: insert pyyaml events
    map
});

pub struct PyEvent(Event);

impl From<Event> for PyEvent {
    fn from(value: Event) -> Self {
        PyEvent(value)
    }
}

impl<'py> IntoPyObject<'py> for PyEvent {
    type Target = PyAny;

    type Output = Bound<'py, Self::Target>;

    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(match self.0 {
            Event {
                data: EventData::StreamStart { encoding },
                start_mark,
                end_mark,
                ..
            } => EVENT_TYPES
                .get("StreamStartEvent")
                .unwrap()
                .bind(py)
                .call_method("__init__", (), None)?,
            Event {
                data: EventData::StreamEnd,
                start_mark,
                end_mark,
                ..
            } => EVENT_TYPES
                .get("StreamEndEvent")
                .unwrap()
                .bind(py)
                .call_method("__init__", (), None)?,
            // TODO(emmatyping): fill out the rest of these
            _ => unimplemented!(),
        })
    }
}
