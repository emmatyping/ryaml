//! Module implement pyyaml compatibility layer for ryaml via libyaml
//! Implements RLoader and RDumper which can load and dump YAML 1.1

use std::io::Cursor;

use libyaml_safer::{Event, EventData, Parser};
use pyo3::{exceptions::PyNotImplementedError, prelude::*};

use crate::event::PyEvent;
use crate::exception::InvalidYamlError;

#[pyclass(name = "_RSafeLoader")]
pub struct RSafeLoader {
    parser: Parser<Cursor<String>>,
    current_event: Option<Event>,
}

#[pymethods]
impl RSafeLoader {
    #[new]
    pub fn new(source: String) -> Self {
        let mut parser = Parser::new();
        parser.set_input(Cursor::new(source));
        Self {
            parser,
            current_event: None,
        }
    }

    fn _parse(&mut self) -> PyResult<PyEvent> {
        match self.parser.parse() {
            Ok(v) => Ok(v.into()),
            Err(e) => Err(InvalidYamlError::new_err(format!("{}", e))),
        }
    }

    pub fn peek_token(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Tokenizing is not implemented",
        ))
    }

    pub fn check_token(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Tokenizing is not implemented",
        ))
    }

    pub fn get_token(&self) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(
            "Tokenizing is not implemented",
        ))
    }

    pub fn get_event(&mut self) -> PyResult<PyEvent> {
        if let Some(event) = self.current_event.take() {
            Ok(event.into())
        } else {
            self._parse()
        }
    }

    pub fn peek_event(&mut self) -> PyResult<PyEvent> {
        Ok(match self.current_event.take() {
            Some(event) => event.into(),
            None => self._parse()?,
        })
    }

    pub fn check_event(&mut self) -> bool {
        unimplemented!()
    }

    pub fn check_node(&mut self) -> PyResult<bool> {
        let mut event = self.parser.parse();
        if let Ok(Event { data: EventData::StreamStart { .. }, .. }) = event {
            event = self.parser.parse();
        }
        if let Ok( Event { data: EventData::StreamEnd, .. }) = event {
            return Ok(false)
        }
        Ok(true)
    }

    pub fn get_node(&mut self) -> PyResult<Py<PyAny>> {
        unimplemented!()
    }
}
