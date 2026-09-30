//! Rust descriptors with Python's ordinary method binding conventions.

use std::ffi::CStr;

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::PyTypeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple, PyType};

#[pyclass(module = "rustializer._feasibility", name = "_NativeMethod")]
pub struct NativeMethod {
    owner: Option<Py<PyType>>,
    receiver: Option<Py<PyAny>>,
    function: ffi::PyCFunctionWithKeywords,
    pub name: String,
    doc: String,
    parameters: Option<Vec<String>>,
}

impl NativeMethod {
    pub fn new(owner: &Bound<'_, PyType>, definition: ffi::PyMethodDef) -> Self {
        // install_method accepts definitions made from static C strings and a
        // METH_VARARGS | METH_KEYWORDS callback. Copy text; retain no definition.
        let name = unsafe { CStr::from_ptr(definition.ml_name) }
            .to_str()
            .expect("ASCII method name")
            .to_owned();
        let doc = unsafe { CStr::from_ptr(definition.ml_doc) }
            .to_str()
            .expect("ASCII method documentation")
            .to_owned();
        let parameters = doc.split_once("\n--\n").and_then(|(signature, _)| {
            let (_, tail) = signature.split_once('(')?;
            let tail = tail.strip_suffix(')')?;
            Some(
                tail.split(',')
                    .map(|part| part.trim().trim_start_matches('$').to_owned())
                    .collect(),
            )
        });
        Self {
            owner: Some(owner.clone().unbind()),
            receiver: None,
            function: unsafe { definition.ml_meth.PyCFunctionWithKeywords },
            name,
            doc,
            parameters,
        }
    }
}

#[pymethods]
impl NativeMethod {
    fn __get__(
        slf: &Bound<'_, Self>,
        instance: Option<Bound<'_, PyAny>>,
        _owner: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let Some(instance) = instance.filter(|value| !value.is_none()) else {
            return Ok(slf.clone().into_any().unbind());
        };
        let method = slf.borrow();
        Ok(Py::new(
            py,
            Self {
                owner: method.owner.as_ref().map(|owner| owner.clone_ref(py)),
                receiver: Some(instance.unbind()),
                function: method.function,
                name: method.name.clone(),
                doc: method.doc.clone(),
                parameters: method.parameters.clone(),
            },
        )?
        .into_any())
    }

    #[pyo3(signature = (*args, **kwargs))]
    fn __call__(
        slf: &Bound<'_, Self>,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        // End the Rust borrow before arbitrary Python code can re-enter.
        let (function, receiver, parameters) = {
            let method = slf.borrow();
            (
                method.function,
                method.receiver.as_ref().map(|value| value.clone_ref(py)),
                method.parameters.clone(),
            )
        };
        let kwargs = match kwargs {
            Some(kwargs) => kwargs.copy()?,
            None => PyDict::new(py),
        };
        let (receiver, args) = match receiver {
            Some(receiver) => {
                if kwargs.contains("self")? {
                    return Err(PyTypeError::new_err("multiple values for argument 'self'"));
                }
                (receiver, args.clone())
            }
            None if !args.is_empty() => {
                if kwargs.contains("self")? {
                    return Err(PyTypeError::new_err("multiple values for argument 'self'"));
                }
                (
                    args.get_item(0)?.unbind(),
                    PyTuple::new(py, args.iter().skip(1))?,
                )
            }
            None => {
                let receiver = kwargs
                    .get_item("self")?
                    .ok_or_else(|| PyTypeError::new_err("missing argument: self"))?;
                kwargs.del_item("self")?;
                (receiver.unbind(), args.clone())
            }
        };
        // Fixed signatures in this experiment have required positional-or-
        // keyword parameters. Normalize both forms before invoking the loop.
        let args = if let Some(parameters) = parameters {
            let names = &parameters[1..];
            if args.len() > names.len() {
                return Err(PyTypeError::new_err("too many positional arguments"));
            }
            let mut values = Vec::with_capacity(names.len());
            for (index, name) in names.iter().enumerate() {
                if index < args.len() {
                    if kwargs.contains(name)? {
                        return Err(PyTypeError::new_err(format!(
                            "multiple values for argument '{name}'"
                        )));
                    }
                    values.push(args.get_item(index)?);
                } else {
                    let value = kwargs
                        .get_item(name)?
                        .ok_or_else(|| PyTypeError::new_err(format!("missing argument: {name}")))?;
                    kwargs.del_item(name)?;
                    values.push(value);
                }
            }
            if !kwargs.is_empty() {
                return Err(PyTypeError::new_err("unexpected keyword argument"));
            }
            PyTuple::new(py, values)?
        } else {
            args
        };
        unsafe {
            Bound::from_owned_ptr_or_err(
                py,
                function(receiver.as_ptr(), args.as_ptr(), kwargs.as_ptr()),
            )
        }
        .map(Bound::unbind)
    }

    #[getter]
    fn __name__(&self) -> &str {
        &self.name
    }

    #[getter]
    fn __doc__(&self) -> &str {
        &self.doc
    }

    #[getter]
    fn __qualname__(&self, py: Python<'_>) -> PyResult<String> {
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| PyTypeError::new_err("cleared method"))?;
        Ok(format!(
            "{}.{}",
            owner
                .bind(py)
                .getattr("__qualname__")?
                .extract::<String>()?,
            self.name
        ))
    }

    #[getter]
    fn __signature__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let inspect = py.import("inspect")?;
        let parameter = inspect.getattr("Parameter")?;
        let parameters = match &self.parameters {
            Some(names) => names
                .iter()
                .skip(usize::from(self.receiver.is_some()))
                .map(|name| parameter.call1((name, parameter.getattr("POSITIONAL_OR_KEYWORD")?)))
                .collect::<PyResult<Vec<_>>>()?,
            None => {
                let mut parameters = Vec::new();
                if self.receiver.is_none() {
                    parameters.push(
                        parameter.call1(("self", parameter.getattr("POSITIONAL_OR_KEYWORD")?))?,
                    );
                }
                parameters.push(parameter.call1(("args", parameter.getattr("VAR_POSITIONAL")?))?);
                parameters.push(parameter.call1(("kwargs", parameter.getattr("VAR_KEYWORD")?))?);
                parameters
            }
        };
        Ok(inspect.getattr("Signature")?.call1((parameters,))?.unbind())
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.owner)?;
        visit.call(&self.receiver)
    }

    fn __clear__(&mut self) {
        self.owner = None;
        self.receiver = None;
    }
}
