//! Native execution of the pinned serializer programs. No Python engine code is
//! compiled, evaluated, retained as a fallback, or called by this runtime.
use std::collections::HashSet;
use std::sync::Arc;

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{
    PyAssertionError, PyNameError, PyRuntimeError, PyTypeError, PyUnboundLocalError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyModule, PySet, PyString, PyTuple, PyType};
use serde_json::Value;

use crate::abi;
mod fast;

// Native handlers participate in Python's handled-exception state so callbacks
// see the same sys.exc_info() and chained exceptions as reference callbacks.
struct HandledException {
    previous: [*mut pyo3::ffi::PyObject; 3],
}
impl HandledException {
    fn enter(py: Python<'_>, error: &PyErr) -> Self {
        let mut previous = [std::ptr::null_mut(); 3];
        unsafe {
            pyo3::ffi::PyErr_GetExcInfo(&mut previous[0], &mut previous[1], &mut previous[2]);
        }
        let kind = error.get_type(py).into_any().unbind();
        let value = error.value(py).clone().into_any().unbind();
        let traceback = error
            .traceback(py)
            .map(|v| v.into_any().unbind())
            .unwrap_or_else(|| py.None());
        unsafe {
            pyo3::ffi::PyErr_SetExcInfo(kind.into_ptr(), value.into_ptr(), traceback.into_ptr());
        }
        Self { previous }
    }
}
impl Drop for HandledException {
    fn drop(&mut self) {
        unsafe {
            pyo3::ffi::PyErr_SetExcInfo(self.previous[0], self.previous[1], self.previous[2]);
        }
    }
}
type Node = Value;
type Object = Py<PyAny>;
fn op(n: &Node) -> &str {
    n["_node"].as_str().unwrap_or("")
}
fn s<'a>(n: &'a Node, key: &str) -> &'a str {
    n[key].as_str().unwrap_or("")
}
fn array<'a>(n: &'a Node, key: &str) -> &'a [Node] {
    n[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}

#[pyclass(subclass, dict, weakref, module = "rustializer._native")]
pub struct EngineState;
#[pymethods]
impl EngineState {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
    fn __reduce__(slf: &Bound<'_, Self>) -> PyResult<Object> {
        reduce_state(slf.py(), slf.as_any())
    }
    fn __traverse__(&self, _visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        Ok(())
    }
}

#[pyclass(subclass, module = "rustializer._native")]
struct EngineSlotState;
#[pymethods]
impl EngineSlotState {
    #[new]
    #[pyo3(signature=(*_args,**_kwargs))]
    fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
    fn __reduce__(slf: &Bound<'_, Self>) -> PyResult<Object> {
        reduce_state(slf.py(), slf.as_any())
    }
}
fn reduce_state(py: Python<'_>, slf: &Bound<'_, PyAny>) -> PyResult<Object> {
    let copyreg = py.import("copyreg")?;
    let slots = copyreg.getattr("_slotnames")?.call1((slf.get_type(),))?;
    let state = slf
        .getattr("__dict__")
        .unwrap_or_else(|_| py.None().into_bound(py));
    let slot_state = PyDict::new(py);
    if !slots.is_none() {
        for name in slots.try_iter()? {
            let name = name?;
            if let Ok(value) = slf.getattr(name.cast::<PyString>()?) {
                slot_state.set_item(name, value)?;
            }
        }
    }
    let state = if slot_state.is_empty() {
        state
    } else {
        PyTuple::new(py, [state, slot_state.into_any()])?.into_any()
    };
    Ok((
        copyreg.getattr("__newobj__")?,
        PyTuple::new(py, [slf.get_type()])?,
        state,
    )
        .into_pyobject(py)?
        .into_any()
        .unbind())
}

struct Frame {
    globals: Py<PyDict>,
    locals: Py<PyDict>,
    closure: Option<Py<PyList>>,
    class_cell: Option<Py<PyList>>,
    local_names: Arc<HashSet<String>>,
    first: Option<Object>,
    exception: Option<PyErr>,
    in_class: bool,
}
impl Frame {
    fn child(&self, py: Python<'_>) -> PyResult<Self> {
        Ok(Self {
            globals: self.globals.clone_ref(py),
            locals: self.locals.bind(py).copy()?.unbind(),
            closure: self.closure.as_ref().map(|x| x.clone_ref(py)),
            class_cell: self.class_cell.as_ref().map(|x| x.clone_ref(py)),
            local_names: Arc::new(HashSet::new()),
            first: self.first.as_ref().map(|x| x.clone_ref(py)),
            exception: self.exception.as_ref().map(|x| x.clone_ref(py)),
            in_class: self.in_class,
        })
    }
    fn lookup(&self, py: Python<'_>, name: &str) -> PyResult<Object> {
        if name == "__class__"
            && let Some(cell) = &self.class_cell
            && !cell.bind(py).is_empty()
        {
            return Ok(cell.bind(py).get_item(0)?.unbind());
        }
        if let Some(value) = self.locals.bind(py).get_item(name)? {
            return Ok(value.unbind());
        }
        if self.local_names.contains(name) {
            return Err(PyUnboundLocalError::new_err(format!(
                "cannot access local variable '{name}' where it is not associated with a value"
            )));
        }
        if let Some(closure) = &self.closure {
            for namespace in closure.bind(py).iter() {
                if let Some(value) = namespace.cast::<PyDict>()?.get_item(name)? {
                    return Ok(value.unbind());
                }
            }
        }
        if let Some(value) = self.globals.bind(py).get_item(name)? {
            return Ok(value.unbind());
        }
        py.import("builtins")?
            .getattr(name)
            .map(Bound::unbind)
            .map_err(|_| PyNameError::new_err(format!("name '{name}' is not defined")))
    }
    fn visit(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.globals)?;
        visit.call(&self.locals)?;
        visit.call(&self.closure)?;
        visit.call(&self.class_cell)?;
        visit.call(&self.first)?;
        Ok(())
    }
}

#[pyclass(dict, weakref, module = "rustializer._native")]
struct EngineFunction {
    node: Arc<Node>,
    local_names: Arc<HashSet<String>>,
    is_generator: bool,
    plan: Option<fast::Plan>,
    globals: Option<Py<PyDict>>,
    closure: Option<Py<PyList>>,
    class_cell: Option<Py<PyList>>,
    defaults: Option<Py<PyTuple>>,
    kw_defaults: Option<Py<PyDict>>,
    annotations: Option<Py<PyDict>>,
    receiver: Option<Object>,
    underlying: Option<Object>,
    name: String,
    qualname: String,
    name_object: Option<Py<PyString>>,
    qualname_object: Option<Py<PyString>>,
    module: Option<Object>,
    doc: Option<Object>,
}
fn missing_arguments(name: &str, kind: &str, names: &[&str]) -> PyErr {
    let quoted: Vec<_> = names.iter().map(|name| format!("'{name}'")).collect();
    let description = match quoted.len() {
        1 => quoted[0].clone(),
        2 => format!("{} and {}", quoted[0], quoted[1]),
        n => format!("{}, and {}", quoted[..n - 1].join(", "), quoted[n - 1]),
    };
    PyTypeError::new_err(format!(
        "{name}() missing {} required {kind} argument{}: {description}",
        names.len(),
        if names.len() == 1 { "" } else { "s" }
    ))
}
impl EngineFunction {
    fn copied(&self, py: Python<'_>, receiver: Option<Object>) -> Self {
        Self {
            node: self.node.clone(),
            local_names: self.local_names.clone(),
            is_generator: self.is_generator,
            plan: self.plan.clone(),
            globals: self.globals.as_ref().map(|v| v.clone_ref(py)),
            closure: self.closure.as_ref().map(|v| v.clone_ref(py)),
            class_cell: self.class_cell.as_ref().map(|v| v.clone_ref(py)),
            defaults: self.defaults.as_ref().map(|v| v.clone_ref(py)),
            kw_defaults: self.kw_defaults.as_ref().map(|v| v.clone_ref(py)),
            annotations: None,
            receiver,
            underlying: self.underlying.as_ref().map(|v| v.clone_ref(py)),
            name: self.name.clone(),
            qualname: self.qualname.clone(),
            // Invocation snapshots must not retain unrelated mutable metadata.
            name_object: None,
            qualname_object: None,
            module: None,
            doc: None,
        }
    }
    fn bind(
        &self,
        py: Python<'_>,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Frame> {
        let spec = &self.node["args"];
        let positional_parameters = array(spec, "posonlyargs").iter().chain(array(spec, "args"));
        // Exact positional calls cannot consult defaults or invoke keyword-key
        // hooks. Keep the full binder for every other call and its error paths.
        if self.receiver.is_none()
            && kwargs.is_none_or(|values| values.is_empty())
            && spec["vararg"].is_null()
            && spec["kwarg"].is_null()
            && array(spec, "kwonlyargs").is_empty()
            && positional_parameters.clone().count() == args.len()
        {
            let locals = PyDict::new(py);
            for (parameter, value) in positional_parameters.zip(args.iter()) {
                locals.set_item(s(parameter, "arg"), value)?;
            }
            return Ok(Frame {
                globals: self
                    .globals
                    .as_ref()
                    .ok_or_else(|| PyRuntimeError::new_err("cleared function"))?
                    .clone_ref(py),
                locals: locals.unbind(),
                closure: self.closure.as_ref().map(|value| value.clone_ref(py)),
                class_cell: self.class_cell.as_ref().map(|value| value.clone_ref(py)),
                local_names: self.local_names.clone(),
                first: args.iter().next().map(Bound::unbind),
                exception: None,
                in_class: false,
            });
        }
        let defaults: Vec<_> = self
            .defaults
            .as_ref()
            .map(|v| v.bind(py).iter().collect())
            .unwrap_or_default();
        let globals = self
            .globals
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("cleared function"))?
            .clone_ref(py);
        let locals = PyDict::new(py);
        let kwargs = match kwargs {
            Some(v) => v.copy()?,
            None => PyDict::new(py),
        };
        let names: Vec<&str> = array(spec, "posonlyargs")
            .iter()
            .chain(array(spec, "args"))
            .map(|v| s(v, "arg"))
            .collect();
        let positional: Vec<Bound<'_, PyAny>> = self
            .receiver
            .iter()
            .map(|v| v.bind(py).clone())
            .chain(args.iter())
            .collect();
        for (name, value) in names.iter().zip(&positional) {
            locals.set_item(name, value)?;
        }
        let mut keyword_only_given = 0;
        let remaining = PyDict::new(py);
        for (key, value) in kwargs.iter() {
            let name = key
                .extract::<String>()
                .map_err(|_| PyTypeError::new_err("keywords must be strings"))?;
            let positional_index = names.iter().position(|candidate| **candidate == name);
            let is_posonly =
                positional_index.is_some_and(|index| index < array(spec, "posonlyargs").len());
            let is_kwonly = array(spec, "kwonlyargs")
                .iter()
                .any(|n| s(n, "arg") == name);
            if positional_index.is_some() && !is_posonly || is_kwonly {
                if locals.contains(&name)? {
                    return Err(PyTypeError::new_err(format!(
                        "{}() got multiple values for argument '{name}'",
                        self.qualname
                    )));
                }
                locals.set_item(&name, value)?;
                keyword_only_given += usize::from(is_kwonly);
            } else if !spec["kwarg"].is_null() {
                remaining.set_item(key, value)?;
            } else {
                let posonly: Vec<_> = array(spec, "posonlyargs")
                    .iter()
                    .filter(|n| kwargs.contains(s(n, "arg")).unwrap_or(false))
                    .map(|n| s(n, "arg"))
                    .collect();
                if !posonly.is_empty() {
                    return Err(PyTypeError::new_err(format!(
                        "{}() got some positional-only arguments passed as keyword arguments: '{}'",
                        self.qualname,
                        posonly.join(", ")
                    )));
                }
                return Err(PyTypeError::new_err(format!(
                    "{}() got an unexpected keyword argument '{name}'",
                    self.qualname
                )));
            }
        }
        if positional.len() > names.len() && spec["vararg"].is_null() {
            let minimum = names.len().saturating_sub(defaults.len());
            let expected = if minimum == names.len() {
                names.len().to_string()
            } else {
                format!("from {minimum} to {}", names.len())
            };
            let plural = if names.len() == 1 && defaults.is_empty() {
                ""
            } else {
                "s"
            };
            let given = if keyword_only_given > 0 {
                format!(
                    "{} positional arguments (and {keyword_only_given} keyword-only argument{})",
                    positional.len(),
                    if keyword_only_given == 1 { "" } else { "s" }
                )
            } else {
                positional.len().to_string()
            };
            return Err(PyTypeError::new_err(format!(
                "{}() takes {expected} positional argument{plural} but {given} {} given",
                self.qualname,
                if positional.len() == 1 && keyword_only_given == 0 {
                    "was"
                } else {
                    "were"
                }
            )));
        }
        if !spec["vararg"].is_null() {
            locals.set_item(
                s(&spec["vararg"], "arg"),
                PyTuple::new(py, positional.iter().skip(names.len()))?,
            )?;
        }
        if !spec["kwarg"].is_null() {
            locals.set_item(s(&spec["kwarg"], "arg"), remaining)?;
        }
        let mut missing = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if !locals.contains(name)? {
                if i + defaults.len() >= names.len() {
                    locals.set_item(name, &defaults[i + defaults.len() - names.len()])?;
                } else {
                    missing.push(*name);
                }
            }
        }
        if !missing.is_empty() {
            return Err(missing_arguments(&self.qualname, "positional", &missing));
        }
        missing.clear();
        for item in array(spec, "kwonlyargs") {
            let name = s(item, "arg");
            if !locals.contains(name)? {
                if let Some(value) = self
                    .kw_defaults
                    .as_ref()
                    .map(|v| v.bind(py).get_item(name))
                    .transpose()?
                    .flatten()
                {
                    locals.set_item(name, value)?;
                } else {
                    missing.push(name);
                }
            }
        }
        if !missing.is_empty() {
            return Err(missing_arguments(&self.qualname, "keyword-only", &missing));
        }
        let first = names
            .first()
            .map(|name| locals.get_item(name))
            .transpose()?
            .flatten()
            .map(Bound::unbind);
        Ok(Frame {
            globals,
            locals: locals.unbind(),
            closure: self.closure.as_ref().map(|v| v.clone_ref(py)),
            class_cell: self.class_cell.as_ref().map(|v| v.clone_ref(py)),
            local_names: self.local_names.clone(),
            first,
            exception: None,
            in_class: false,
        })
    }
}
#[pymethods]
impl EngineFunction {
    fn __get__(
        slf: &Bound<'_, Self>,
        instance: Option<Bound<'_, PyAny>>,
        _owner: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Object> {
        let Some(instance) = instance.filter(|v| !v.is_none()) else {
            return Ok(slf.clone().into_any().unbind());
        };
        Ok(slf
            .py()
            .import("types")?
            .getattr("MethodType")?
            .call1((slf, instance))?
            .unbind())
    }
    #[pyo3(signature = (*args, **kwargs))]
    fn __call__(
        slf: &Bound<'_, Self>,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Object> {
        let py = slf.py();
        let mut function = slf
            .borrow()
            .copied(py, slf.borrow().receiver.as_ref().map(|v| v.clone_ref(py)));
        let mut frame = function.bind(py, args, kwargs)?;
        // Values needed by this invocation are now owned by its locals.
        // Releasing default containers here preserves finalizer timing when
        // callbacks subsequently replace the function's defaults.
        function.defaults = None;
        function.kw_defaults = None;
        if function.is_generator {
            return Ok(Py::new(
                py,
                EngineGenerator {
                    frame: Some(frame),
                    tasks: match &function.plan {
                        Some(fast::Plan::ReadableFields(lines)) => {
                            vec![Task::ReadableFields(fast::ReadableFields::new(*lines))]
                        }
                        _ => vec![Task::Block(array(&function.node, "body").to_vec(), 0)],
                    },
                    running: false,
                    started: false,
                },
            )?
            .into_any());
        }
        let _context = crate::warnings::Context::enter(
            py,
            frame.globals.bind(py),
            function.node["line"].as_u64().unwrap_or(1) as u32,
        )?;
        if let Some(plan) = &function.plan {
            return plan.execute(py, &mut frame, &function.node);
        }
        if op(&function.node) == "Lambda" {
            return eval(py, &mut frame, &function.node["body"]);
        }
        match block(py, &mut frame, array(&function.node, "body"))? {
            Flow::Return(value) => Ok(value),
            _ => Ok(py.None()),
        }
    }
    #[getter]
    fn __name__(&self, py: Python<'_>) -> Py<PyString> {
        self.name_object.as_ref().unwrap().clone_ref(py)
    }
    #[setter(__name__)]
    fn set_name(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = value
            .cast::<PyString>()
            .map_err(|_| PyTypeError::new_err("__name__ must be set to a string object"))?;
        let old = {
            let mut function = slf.borrow_mut();
            function.name = value.extract()?;
            function.name_object.replace(value.clone().unbind())
        };
        drop(old);
        Ok(())
    }
    #[getter]
    fn __qualname__(&self, py: Python<'_>) -> Py<PyString> {
        self.qualname_object.as_ref().unwrap().clone_ref(py)
    }
    #[setter(__qualname__)]
    fn set_qualname(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = value
            .cast::<PyString>()
            .map_err(|_| PyTypeError::new_err("__qualname__ must be set to a string object"))?;
        let old = {
            let mut function = slf.borrow_mut();
            function.qualname = value.extract()?;
            function.qualname_object.replace(value.clone().unbind())
        };
        drop(old);
        Ok(())
    }
    #[getter]
    fn __module__(&self, py: Python<'_>) -> Object {
        self.module
            .as_ref()
            .map_or_else(|| py.None(), |v| v.clone_ref(py))
    }
    #[setter(__module__)]
    fn set_module(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) {
        let old = { slf.borrow_mut().module.replace(value.clone().unbind()) };
        // A replaced metadata object may run a finalizer that re-enters us.
        drop(old);
    }
    #[getter]
    fn __globals__(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        self.globals
            .as_ref()
            .map(|v| v.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("cleared function"))
    }
    #[setter(__globals__)]
    fn set_globals(&self, _value: &Bound<'_, PyAny>) -> PyResult<()> {
        Err(pyo3::exceptions::PyAttributeError::new_err(
            "readonly attribute",
        ))
    }
    #[getter]
    fn __self__(&self, py: Python<'_>) -> PyResult<Object> {
        self.receiver
            .as_ref()
            .map(|v| v.clone_ref(py))
            .ok_or_else(|| {
                pyo3::exceptions::PyAttributeError::new_err("native function has no __self__")
            })
    }
    #[getter]
    fn __func__(&self, py: Python<'_>) -> PyResult<Object> {
        self.underlying
            .as_ref()
            .map(|v| v.clone_ref(py))
            .ok_or_else(|| {
                pyo3::exceptions::PyAttributeError::new_err(
                    "unbound native function has no __func__",
                )
            })
    }
    #[getter]
    fn __doc__(&self, py: Python<'_>) -> Object {
        self.doc
            .as_ref()
            .map_or_else(|| py.None(), |v| v.clone_ref(py))
    }
    #[setter(__doc__)]
    fn set_doc(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) {
        let old = { slf.borrow_mut().doc.replace(value.clone().unbind()) };
        // A replaced metadata object may run a finalizer that re-enters us.
        drop(old);
    }
    #[getter]
    fn __defaults__(&self, py: Python<'_>) -> Option<Py<PyTuple>> {
        self.defaults.as_ref().map(|v| v.clone_ref(py))
    }
    #[setter(__defaults__)]
    fn set_defaults(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = if value.is_none() {
            None
        } else {
            Some(
                value
                    .cast::<PyTuple>()
                    .map_err(|_| {
                        PyTypeError::new_err("__defaults__ must be set to a tuple object")
                    })?
                    .clone()
                    .unbind(),
            )
        };
        let old = { std::mem::replace(&mut slf.borrow_mut().defaults, value) };
        drop(old);
        Ok(())
    }
    #[getter]
    fn __kwdefaults__(&self, py: Python<'_>) -> Option<Py<PyDict>> {
        self.kw_defaults.as_ref().map(|v| v.clone_ref(py))
    }
    #[setter(__kwdefaults__)]
    fn set_kwdefaults(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = if value.is_none() {
            None
        } else {
            Some(
                value
                    .cast::<PyDict>()
                    .map_err(|_| {
                        PyTypeError::new_err("__kwdefaults__ must be set to a dict object")
                    })?
                    .clone()
                    .unbind(),
            )
        };
        let old = { std::mem::replace(&mut slf.borrow_mut().kw_defaults, value) };
        drop(old);
        Ok(())
    }
    #[getter]
    fn __annotations__(&self, py: Python<'_>) -> Py<PyDict> {
        self.annotations.as_ref().unwrap().clone_ref(py)
    }
    #[setter(__annotations__)]
    fn set_annotations(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = Some(if value.is_none() {
            PyDict::new(value.py()).unbind()
        } else {
            value
                .cast::<PyDict>()
                .map_err(|_| PyTypeError::new_err("__annotations__ must be set to a dict object"))?
                .clone()
                .unbind()
        });
        let old = { std::mem::replace(&mut slf.borrow_mut().annotations, value) };
        drop(old);
        Ok(())
    }
    fn __setattr__(slf: &Bound<'_, Self>, name: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let name = PyString::new(slf.py(), name);
        let result = unsafe {
            pyo3::ffi::PyObject_GenericSetAttr(slf.as_ptr(), name.as_ptr(), value.as_ptr())
        };
        if result == -1 {
            Err(PyErr::fetch(slf.py()))
        } else {
            Ok(())
        }
    }
    fn __delattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<()> {
        let py = slf.py();
        match name {
            "__name__" | "__qualname__" => Err(PyTypeError::new_err(format!(
                "{name} must be set to a string object"
            ))),
            "__globals__" => Err(pyo3::exceptions::PyAttributeError::new_err(
                "readonly attribute",
            )),
            "__dict__" => Err(PyTypeError::new_err("cannot delete __dict__")),
            "__defaults__" => Self::set_defaults(slf, &py.None().into_bound(py)),
            "__kwdefaults__" => Self::set_kwdefaults(slf, &py.None().into_bound(py)),
            "__annotations__" => Self::set_annotations(slf, &py.None().into_bound(py)),
            "__doc__" => {
                Self::set_doc(slf, &py.None().into_bound(py));
                Ok(())
            }
            "__module__" => {
                Self::set_module(slf, &py.None().into_bound(py));
                Ok(())
            }
            _ => {
                let name = PyString::new(py, name);
                let result = unsafe {
                    pyo3::ffi::PyObject_GenericSetAttr(
                        slf.as_ptr(),
                        name.as_ptr(),
                        std::ptr::null_mut(),
                    )
                };
                if result == -1 {
                    Err(PyErr::fetch(py))
                } else {
                    Ok(())
                }
            }
        }
    }
    fn __reduce__(&self, py: Python<'_>) -> PyResult<Object> {
        Ok(self.qualname.clone().into_pyobject(py)?.into_any().unbind())
    }
    #[getter]
    fn __text_signature__(&self) -> Option<String> {
        let spec = &self.node["args"];
        // The pinned JSON wrappers deliberately accept only variadic arguments.
        // inspect uses this when follow_wrapped=False; normal inspection unwraps.
        if array(spec, "args").is_empty()
            && array(spec, "posonlyargs").is_empty()
            && array(spec, "kwonlyargs").is_empty()
            && !spec["vararg"].is_null()
            && !spec["kwarg"].is_null()
        {
            Some(format!(
                "(*{}, **{})",
                s(&spec["vararg"], "arg"),
                s(&spec["kwarg"], "arg")
            ))
        } else {
            None
        }
    }
    #[getter]
    fn __signature__(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Object> {
        let inspect = py.import("inspect")?;
        if slf.hasattr("__wrapped__")? {
            return Err(pyo3::exceptions::PyAttributeError::new_err(
                "wrapped native function uses its wrapped signature",
            ));
        }
        let this = slf.borrow();
        let self_ = &*this;
        let defaults: Vec<_> = self_
            .defaults
            .as_ref()
            .map(|v| v.bind(py).iter().collect())
            .unwrap_or_default();
        let parameter = inspect.getattr("Parameter")?;
        let spec = &self_.node["args"];
        let mut parameters = Vec::new();
        let positional: Vec<_> = array(spec, "posonlyargs")
            .iter()
            .chain(array(spec, "args"))
            .collect();
        for (i, node) in positional
            .iter()
            .enumerate()
            .skip(usize::from(self_.receiver.is_some()))
        {
            let kwargs = PyDict::new(py);
            if i + defaults.len() >= positional.len() {
                kwargs.set_item("default", &defaults[i + defaults.len() - positional.len()])?;
            }
            if let Some(annotation) = self_
                .annotations
                .as_ref()
                .unwrap()
                .bind(py)
                .get_item(s(node, "arg"))?
            {
                kwargs.set_item("annotation", annotation)?;
            }
            let kind = if i < array(spec, "posonlyargs").len() {
                "POSITIONAL_ONLY"
            } else {
                "POSITIONAL_OR_KEYWORD"
            };
            parameters
                .push(parameter.call((s(node, "arg"), parameter.getattr(kind)?), Some(&kwargs))?);
        }
        if !spec["vararg"].is_null() {
            parameters.push(parameter.call1((
                s(&spec["vararg"], "arg"),
                parameter.getattr("VAR_POSITIONAL")?,
            ))?);
        }
        for node in array(spec, "kwonlyargs") {
            let kwargs = PyDict::new(py);
            if let Some(value) = self_
                .kw_defaults
                .as_ref()
                .map(|v| v.bind(py).get_item(s(node, "arg")))
                .transpose()?
                .flatten()
            {
                kwargs.set_item("default", value)?;
            }
            parameters.push(parameter.call(
                (s(node, "arg"), parameter.getattr("KEYWORD_ONLY")?),
                Some(&kwargs),
            )?);
        }
        if !spec["kwarg"].is_null() {
            parameters.push(
                parameter.call1((s(&spec["kwarg"], "arg"), parameter.getattr("VAR_KEYWORD")?))?,
            );
        }
        let kwargs = PyDict::new(py);
        if let Some(annotation) = self_
            .annotations
            .as_ref()
            .unwrap()
            .bind(py)
            .get_item("return")?
        {
            kwargs.set_item("return_annotation", annotation)?;
        }
        Ok(inspect
            .getattr("Signature")?
            .call((parameters,), Some(&kwargs))?
            .unbind())
    }
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.globals)?;
        visit.call(&self.closure)?;
        visit.call(&self.class_cell)?;
        visit.call(&self.receiver)?;
        visit.call(&self.underlying)?;
        visit.call(&self.defaults)?;
        visit.call(&self.kw_defaults)?;
        visit.call(&self.annotations)?;
        visit.call(&self.name_object)?;
        visit.call(&self.qualname_object)?;
        visit.call(&self.module)?;
        visit.call(&self.doc)?;
        Ok(())
    }
    fn __clear__(&mut self) {
        self.globals = None;
        self.closure = None;
        self.class_cell = None;
        self.receiver = None;
        self.underlying = None;
        self.defaults = None;
        self.kw_defaults = None;
        self.annotations = None;
        self.name_object = None;
        self.qualname_object = None;
        self.module = None;
        self.doc = None;
    }
}
fn contains_yield(node: &Node, root: bool) -> bool {
    if matches!(op(node), "Yield" | "YieldFrom") {
        return true;
    }
    if !root && matches!(op(node), "FunctionDef" | "Lambda" | "ClassDef") {
        return false;
    }
    match node {
        Node::Object(map) => map.values().any(|n| contains_yield(n, false)),
        Node::Array(a) => a.iter().any(|n| contains_yield(n, false)),
        _ => false,
    }
}
fn collect_locals(node: &Node, names: &mut HashSet<String>, root: bool) {
    if !root && matches!(op(node), "FunctionDef" | "ClassDef") {
        names.insert(s(node, "name").to_owned());
        return;
    }
    if matches!(
        op(node),
        "ListComp" | "DictComp" | "SetComp" | "GeneratorExp" | "Lambda"
    ) {
        return;
    }
    for key in ["targets", "target"] {
        if let Some(n) = node.get(key) {
            collect_targets(n, names);
        }
    }
    if op(node) == "ExceptHandler"
        && let Some(name) = node["name"].as_str()
    {
        names.insert(name.to_owned());
    }
    match node {
        Node::Object(map) => {
            for n in map.values() {
                collect_locals(n, names, false)
            }
        }
        Node::Array(a) => {
            for n in a {
                collect_locals(n, names, false)
            }
        }
        _ => {}
    }
}
fn collect_targets(node: &Node, names: &mut HashSet<String>) {
    if op(node) == "Name" {
        names.insert(s(node, "id").to_owned());
    }
    if let Some(a) = node.as_array() {
        for n in a {
            collect_targets(n, names);
        }
    }
    for n in array(node, "elts") {
        collect_targets(n, names);
    }
}
fn lexical_scope(py: Python<'_>, f: &Frame, include_locals: bool) -> PyResult<Option<Py<PyList>>> {
    let namespaces = PyList::empty(py);
    if include_locals && f.locals.as_ptr() != f.globals.as_ptr() {
        namespaces.append(f.locals.bind(py))?;
    }
    if let Some(closure) = &f.closure {
        for namespace in closure.bind(py).iter() {
            namespaces.append(namespace)?;
        }
    }
    Ok(if namespaces.is_empty() {
        None
    } else {
        Some(namespaces.unbind())
    })
}
fn function(py: Python<'_>, frame: &mut Frame, node: &Node, in_class: bool) -> PyResult<Object> {
    let defaults = array(&node["args"], "defaults")
        .iter()
        .map(|n| eval(py, frame, n))
        .collect::<PyResult<Vec<_>>>()?;
    let defaults = if defaults.is_empty() {
        None
    } else {
        Some(PyTuple::new(py, defaults)?.unbind())
    };
    let kw_defaults = PyDict::new(py);
    for (argument, value) in array(&node["args"], "kwonlyargs")
        .iter()
        .zip(array(&node["args"], "kw_defaults"))
    {
        if !value.is_null() {
            kw_defaults.set_item(s(argument, "arg"), eval(py, frame, value)?)?;
        }
    }
    let kw_defaults = if kw_defaults.is_empty() {
        None
    } else {
        Some(kw_defaults.unbind())
    };
    let annotations = PyDict::new(py);
    for argument in array(&node["args"], "posonlyargs")
        .iter()
        .chain(array(&node["args"], "args"))
        .chain(array(&node["args"], "kwonlyargs"))
    {
        if !argument["annotation"].is_null() {
            annotations.set_item(
                s(argument, "arg"),
                eval(py, frame, &argument["annotation"])?,
            )?;
        }
    }
    if !node["returns"].is_null() {
        annotations.set_item("return", eval(py, frame, &node["returns"])?)?;
    }
    let module = frame
        .globals
        .bind(py)
        .get_item("__name__")?
        .unwrap()
        .extract::<String>()?;
    let name = if op(node) == "Lambda" {
        "<lambda>"
    } else {
        s(node, "name")
    }
    .to_owned();
    let prefix = frame
        .locals
        .bind(py)
        .get_item("__qualname__")?
        .and_then(|v| v.extract::<String>().ok());
    let qualname = prefix
        .map(|p| format!("{p}.{name}"))
        .unwrap_or_else(|| name.clone());
    let plan = fast::Plan::new(&module, &qualname, node);
    let mut local_names = HashSet::new();
    collect_locals(node, &mut local_names, true);
    Ok(Py::new(
        py,
        EngineFunction {
            node: Arc::new(node.clone()),
            local_names: Arc::new(local_names),
            is_generator: contains_yield(node, true),
            plan,
            globals: Some(frame.globals.clone_ref(py)),
            closure: lexical_scope(py, frame, !in_class)?,
            class_cell: frame.class_cell.as_ref().map(|v| v.clone_ref(py)),
            defaults,
            kw_defaults,
            annotations: Some(annotations.unbind()),
            receiver: None,
            underlying: None,
            doc: array(node, "body")
                .first()
                .filter(|v| op(v) == "Expr" && op(&v["value"]) == "Constant")
                .and_then(|v| {
                    v["value"]["value"]
                        .as_str()
                        .map(|v| PyString::new(py, v).into_any().unbind())
                }),
            name_object: Some(PyString::new(py, &name).unbind()),
            qualname_object: Some(PyString::new(py, &qualname).unbind()),
            qualname,
            name,
            module: Some(PyString::new(py, &module).into_any().unbind()),
        },
    )?
    .into_any())
}

fn binary(
    py: Python<'_>,
    name: &str,
    left: &Object,
    right: &Object,
    inplace: bool,
) -> PyResult<Object> {
    use pyo3::ffi;
    let left = left.as_ptr();
    let right = right.as_ptr();
    if name == "Pow" {
        let result = unsafe {
            if inplace {
                ffi::PyNumber_InPlacePower(left, right, py.None().as_ptr())
            } else {
                ffi::PyNumber_Power(left, right, py.None().as_ptr())
            }
        };
        return unsafe { Bound::from_owned_ptr_or_err(py, result) }.map(Bound::unbind);
    }
    let operation: unsafe extern "C" fn(
        *mut ffi::PyObject,
        *mut ffi::PyObject,
    ) -> *mut ffi::PyObject = match (name, inplace) {
        ("Add", false) => ffi::PyNumber_Add,
        ("Add", true) => ffi::PyNumber_InPlaceAdd,
        ("Sub", false) => ffi::PyNumber_Subtract,
        ("Sub", true) => ffi::PyNumber_InPlaceSubtract,
        ("Mult", false) => ffi::PyNumber_Multiply,
        ("Mult", true) => ffi::PyNumber_InPlaceMultiply,
        ("Div", false) => ffi::PyNumber_TrueDivide,
        ("Div", true) => ffi::PyNumber_InPlaceTrueDivide,
        ("FloorDiv", false) => ffi::PyNumber_FloorDivide,
        ("FloorDiv", true) => ffi::PyNumber_InPlaceFloorDivide,
        ("Mod", false) => ffi::PyNumber_Remainder,
        ("Mod", true) => ffi::PyNumber_InPlaceRemainder,
        ("BitOr", false) => ffi::PyNumber_Or,
        ("BitOr", true) => ffi::PyNumber_InPlaceOr,
        ("BitAnd", false) => ffi::PyNumber_And,
        ("BitAnd", true) => ffi::PyNumber_InPlaceAnd,
        ("BitXor", false) => ffi::PyNumber_Xor,
        ("BitXor", true) => ffi::PyNumber_InPlaceXor,
        ("LShift", false) => ffi::PyNumber_Lshift,
        ("LShift", true) => ffi::PyNumber_InPlaceLshift,
        ("RShift", false) => ffi::PyNumber_Rshift,
        ("RShift", true) => ffi::PyNumber_InPlaceRshift,
        _ => {
            return Err(PyRuntimeError::new_err(format!(
                "unknown binary operation {name}"
            )));
        }
    };
    // These Stable ABI operations implement Python's reflected/in-place dispatch
    // directly, without importing the independently mutable operator module.
    unsafe { Bound::from_owned_ptr_or_err(py, operation(left, right)) }.map(Bound::unbind)
}
fn constant(py: Python<'_>, n: &Node) -> PyResult<Object> {
    match n {
        Node::Null => Ok(py.None()),
        Node::Bool(v) => Ok(v.into_pyobject(py)?.to_owned().into_any().unbind()),
        Node::String(v) => Ok(v.into_pyobject(py)?.into_any().unbind()),
        Node::Number(v) => {
            if let Some(i) = v.as_i64() {
                Ok(i.into_pyobject(py)?.into_any().unbind())
            } else if let Some(u) = v.as_u64() {
                Ok(u.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(v.as_f64().unwrap().into_pyobject(py)?.into_any().unbind())
            }
        }
        _ if op(n) == "Bytes" => Ok(PyBytes::new(
            py,
            &array(n, "value")
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect::<Vec<_>>(),
        )
        .into_any()
        .unbind()),
        _ if op(n) == "Ellipsis" => Ok(py.Ellipsis()),
        _ => Err(PyRuntimeError::new_err("unknown constant")),
    }
}
fn compare(py: Python<'_>, f: &mut Frame, n: &Node, conditional: bool) -> PyResult<Object> {
    let mut left = eval(py, f, &n["left"])?;
    let mut result = true.into_pyobject(py)?.to_owned().into_any().unbind();
    for (index, (operator, other)) in array(n, "ops")
        .iter()
        .zip(array(n, "comparators"))
        .enumerate()
    {
        let right = eval(py, f, other)?;
        result = match op(operator) {
            "Is" | "IsNot" => {
                let v = left.bind(py).is(right.bind(py)) ^ (op(operator) == "IsNot");
                v.into_pyobject(py)?.to_owned().into_any().unbind()
            }
            "In" | "NotIn" => {
                let v = right.bind(py).contains(left.bind(py))? ^ (op(operator) == "NotIn");
                v.into_pyobject(py)?.to_owned().into_any().unbind()
            }
            name => {
                let operation = match name {
                    "Eq" => pyo3::class::basic::CompareOp::Eq,
                    "NotEq" => pyo3::class::basic::CompareOp::Ne,
                    "Lt" => pyo3::class::basic::CompareOp::Lt,
                    "LtE" => pyo3::class::basic::CompareOp::Le,
                    "Gt" => pyo3::class::basic::CompareOp::Gt,
                    "GtE" => pyo3::class::basic::CompareOp::Ge,
                    _ => return Err(PyRuntimeError::new_err("unknown comparison")),
                };
                left.bind(py)
                    .rich_compare(right.bind(py), operation)?
                    .unbind()
            }
        };
        if (conditional || index + 1 < array(n, "ops").len()) && !result.bind(py).is_truthy()? {
            if conditional {
                return Ok(false.into_pyobject(py)?.to_owned().into_any().unbind());
            }
            break;
        }
        left = right;
    }
    if conditional {
        Ok(true.into_pyobject(py)?.to_owned().into_any().unbind())
    } else {
        Ok(result)
    }
}
fn truth(py: Python<'_>, f: &mut Frame, n: &Node) -> PyResult<bool> {
    if op(n) == "BoolOp" {
        let conjunction = op(&n["op"]) == "And";
        for value in array(n, "values") {
            let value = truth(py, f, value)?;
            if value != conjunction {
                return Ok(value);
            }
        }
        return Ok(conjunction);
    }
    if op(n) == "Compare" {
        return compare(py, f, n, true)?.bind(py).is_truthy();
    }
    if op(n) == "UnaryOp" && op(&n["op"]) == "Not" {
        return Ok(!truth(py, f, &n["operand"])?);
    }
    eval(py, f, n)?.bind(py).is_truthy()
}
fn eval(py: Python<'_>, f: &mut Frame, n: &Node) -> PyResult<Object> {
    if n.is_null() {
        return Ok(py.None());
    }
    match op(n) {
        "Constant" => constant(py, &n["value"]),
        "Name" => f.lookup(py, s(n, "id")),
        "Attribute" => Ok(eval(py, f, &n["value"])?
            .bind(py)
            .getattr(s(n, "attr"))?
            .unbind()),
        "Subscript" => Ok(eval(py, f, &n["value"])?
            .bind(py)
            .get_item(eval(py, f, &n["slice"])?.bind(py))?
            .unbind()),
        "Slice" => {
            let lo = eval(py, f, &n["lower"])?;
            let hi = eval(py, f, &n["upper"])?;
            let step = eval(py, f, &n["step"])?;
            Ok(unsafe {
                Bound::<PyAny>::from_owned_ptr_or_err(
                    py,
                    pyo3::ffi::PySlice_New(lo.as_ptr(), hi.as_ptr(), step.as_ptr()),
                )?
            }
            .unbind())
        }
        "List" | "Tuple" | "Set" => {
            let mut values = Vec::new();
            for v in array(n, "elts") {
                if op(v) == "Starred" {
                    for x in eval(py, f, &v["value"])?.bind(py).try_iter()? {
                        values.push(x?.unbind());
                    }
                } else {
                    values.push(eval(py, f, v)?);
                }
            }
            match op(n) {
                "List" => Ok(PyList::new(py, values)?.into_any().unbind()),
                "Tuple" => Ok(PyTuple::new(py, values)?.into_any().unbind()),
                _ => Ok(PySet::new(py, &values)?.into_any().unbind()),
            }
        }
        "Dict" => {
            let result = PyDict::new(py);
            for (key, value) in array(n, "keys").iter().zip(array(n, "values")) {
                if key.is_null() {
                    result.call_method1("update", (eval(py, f, value)?.bind(py),))?;
                } else {
                    let key = eval(py, f, key)?;
                    let value = eval(py, f, value)?;
                    result.set_item(key.bind(py), value.bind(py))?;
                }
            }
            Ok(result.into_any().unbind())
        }
        "BinOp" => {
            let left = eval(py, f, &n["left"])?;
            let right = eval(py, f, &n["right"])?;
            binary(py, op(&n["op"]), &left, &right, false)
        }
        "UnaryOp" => {
            if op(&n["op"]) == "Not" {
                return Ok((!truth(py, f, &n["operand"])?)
                    .into_pyobject(py)?
                    .to_owned()
                    .into_any()
                    .unbind());
            }
            let value = eval(py, f, &n["operand"])?;
            let result = unsafe {
                match op(&n["op"]) {
                    "USub" => pyo3::ffi::PyNumber_Negative(value.as_ptr()),
                    "UAdd" => pyo3::ffi::PyNumber_Positive(value.as_ptr()),
                    "Invert" => pyo3::ffi::PyNumber_Invert(value.as_ptr()),
                    _ => return Err(PyRuntimeError::new_err("unknown unary operation")),
                }
            };
            unsafe { Bound::from_owned_ptr_or_err(py, result) }.map(Bound::unbind)
        }
        "BoolOp" => {
            let mut result = py.None();
            let values = array(n, "values");
            for (index, value) in values.iter().enumerate() {
                result = eval(py, f, value)?;
                if index + 1 == values.len() {
                    break;
                }
                let truth = result.bind(py).is_truthy()?;
                if (op(&n["op"]) == "And" && !truth) || (op(&n["op"]) == "Or" && truth) {
                    break;
                }
            }
            Ok(result)
        }
        "Compare" => compare(py, f, n, false),
        "IfExp" => {
            if truth(py, f, &n["test"])? {
                eval(py, f, &n["body"])
            } else {
                eval(py, f, &n["orelse"])
            }
        }
        "Call" => {
            if op(&n["func"]) == "Name"
                && s(&n["func"], "id") == "super"
                && array(n, "args").is_empty()
                && array(n, "keywords").is_empty()
            {
                let cls = f.lookup(py, "__class__")?;
                let first = f
                    .first
                    .as_ref()
                    .ok_or_else(|| PyRuntimeError::new_err("super(): no arguments"))?;
                return Ok(py
                    .import("builtins")?
                    .getattr("super")?
                    .call1((cls.bind(py), first.bind(py)))?
                    .unbind());
            }
            let callable = eval(py, f, &n["func"])?;
            let mut args = Vec::new();
            let kwargs = PyDict::new(py);
            for arg in array(n, "args") {
                if op(arg) == "Starred" {
                    for value in eval(py, f, &arg["value"])?.bind(py).try_iter()? {
                        args.push(value?.unbind());
                    }
                } else {
                    args.push(eval(py, f, arg)?);
                }
            }
            for kw in array(n, "keywords") {
                let value = eval(py, f, &kw["value"])?;
                if kw["arg"].is_null() {
                    for key in value.bind(py).call_method0("keys")?.try_iter()? {
                        let key = key?;
                        if kwargs.contains(&key)? {
                            return Err(PyTypeError::new_err(format!(
                                "got multiple values for keyword argument '{}'",
                                key.str()?
                            )));
                        }
                        kwargs.set_item(&key, value.bind(py).get_item(&key)?)?;
                    }
                } else {
                    let key = s(kw, "arg");
                    if kwargs.contains(key)? {
                        return Err(PyTypeError::new_err(format!(
                            "got multiple values for keyword argument '{key}'"
                        )));
                    }
                    kwargs.set_item(key, value.bind(py))?;
                }
            }
            if args.len() == 1
                && kwargs.is_empty()
                && op(&n["func"]) == "Attribute"
                && op(&n["func"]["value"]) == "Name"
                && s(&n["func"]["value"], "id") == "inspect"
                && let Ok(native) = args[0].bind(py).cast::<EngineFunction>()
                // Honor application replacements of the inspection callbacks.
                // Only adapt the original stdlib function classification for
                // our native descriptors, whose Python bytecode is excluded.
                && callable.bind(py).get_type().is(py.import("types")?.getattr("FunctionType")?)
                && callable.bind(py).getattr("__globals__")?.is(py.import("inspect")?.dict())
                && callable.bind(py).getattr("__code__")?.getattr("co_name")?.extract::<String>()? == s(&n["func"], "attr")
            {
                let bound = native.borrow().receiver.is_some();
                let value = match s(&n["func"], "attr") {
                    "isfunction" => Some(!bound),
                    "ismethod" => Some(bound),
                    _ => None,
                };
                if let Some(value) = value {
                    return Ok(value.into_pyobject(py)?.to_owned().into_any().unbind());
                }
            }
            crate::warnings::mark(n["line"].as_u64().unwrap_or(1) as u32);
            if op(&n["func"]) == "Attribute"
                && op(&n["func"]["value"]) == "Name"
                && s(&n["func"]["value"], "id") == "warnings"
                && s(&n["func"], "attr") == "warn"
                && callable
                    .bind(py)
                    .is_instance_of::<pyo3::types::PyCFunction>()
                && callable
                    .bind(py)
                    .getattr("__module__")?
                    .extract::<String>()?
                    == "_warnings"
            {
                return crate::warnings::warn(py, &args, &kwargs);
            }
            Ok(callable
                .bind(py)
                .call(PyTuple::new(py, args)?, Some(&kwargs))?
                .unbind())
        }
        "Lambda" => function(py, f, n, false),
        "ListComp" | "SetComp" | "DictComp" | "GeneratorExp" => comprehension(py, f, n),
        "JoinedStr" => {
            let mut result = String::new();
            for value in array(n, "values") {
                let v = eval(py, f, value)?;
                result.push_str(v.bind(py).cast::<PyString>()?.to_str()?);
            }
            Ok(result.into_pyobject(py)?.into_any().unbind())
        }
        "FormattedValue" => {
            let mut value = eval(py, f, &n["value"])?;
            match n["conversion"].as_i64() {
                Some(114) => value = value.bind(py).repr()?.into_any().unbind(),
                Some(115) => value = value.bind(py).str()?.into_any().unbind(),
                Some(97) => {
                    value = py
                        .import("builtins")?
                        .getattr("ascii")?
                        .call1((value.bind(py),))?
                        .unbind()
                }
                _ => {}
            }
            let spec = if n["format_spec"].is_null() {
                "".into_pyobject(py)?.into_any().unbind()
            } else {
                eval(py, f, &n["format_spec"])?
            };
            Ok(py
                .import("builtins")?
                .getattr("format")?
                .call1((value.bind(py), spec.bind(py)))?
                .unbind())
        }
        _ => Err(PyRuntimeError::new_err(format!(
            "unsupported native expression {}: {n}",
            op(n)
        ))),
    }
}
fn assign(py: Python<'_>, f: &mut Frame, target: &Node, value: &Object) -> PyResult<()> {
    match op(target) {
        "Name" => f.locals.bind(py).set_item(s(target, "id"), value.bind(py)),
        "Attribute" => eval(py, f, &target["value"])?
            .bind(py)
            .setattr(s(target, "attr"), value.bind(py)),
        "Subscript" => {
            let owner = eval(py, f, &target["value"])?;
            let key = eval(py, f, &target["slice"])?;
            owner.bind(py).set_item(key.bind(py), value.bind(py))
        }
        "Tuple" | "List" => {
            let targets = array(target, "elts");
            let items = fast::unpack(py, value.bind(py), targets.len())?;
            for (target, value) in targets.iter().zip(items) {
                assign(py, f, target, &value)?;
            }
            Ok(())
        }
        _ => Err(PyRuntimeError::new_err(format!(
            "unsupported native assignment {}",
            op(target)
        ))),
    }
}
fn delete(py: Python<'_>, f: &mut Frame, target: &Node) -> PyResult<()> {
    match op(target) {
        "Name" => f.locals.bind(py).del_item(s(target, "id")),
        "Attribute" => eval(py, f, &target["value"])?
            .bind(py)
            .delattr(s(target, "attr")),
        "Subscript" => {
            let owner = eval(py, f, &target["value"])?;
            let key = eval(py, f, &target["slice"])?;
            owner.bind(py).del_item(key.bind(py))
        }
        _ => Err(PyRuntimeError::new_err("unsupported deletion")),
    }
}

enum Flow {
    Next,
    Return(Object),
    Break,
    Continue,
}
fn block(py: Python<'_>, f: &mut Frame, body: &[Node]) -> PyResult<Flow> {
    for n in body {
        let result = statement(py, f, n)?;
        if !matches!(result, Flow::Next) {
            return Ok(result);
        }
    }
    Ok(Flow::Next)
}
// Resume the ordinary native exception handlers after a direct successful-path operation.
fn finish_try(py: Python<'_>, f: &mut Frame, n: &Node, result: PyResult<Flow>) -> PyResult<Flow> {
    let mut result = match result {
        Ok(Flow::Next) => block(py, f, array(n, "orelse")),
        Ok(flow) => Ok(flow),
        Err(error) => {
            let mut handler = None;
            for candidate in array(n, "handlers") {
                if candidate["type"].is_null()
                    || error.matches(py, eval(py, f, &candidate["type"])?.bind(py))?
                {
                    handler = Some(candidate);
                    break;
                }
            }
            if let Some(handler) = handler {
                let _handled = HandledException::enter(py, &error);
                let previous = f.exception.take();
                f.exception = Some(error.clone_ref(py));
                if let Some(name) = handler["name"].as_str() {
                    f.locals.bind(py).set_item(name, error.value(py))?;
                }
                let result = block(py, f, array(handler, "body"));
                if let Some(name) = handler["name"].as_str()
                    && f.locals.bind(py).contains(name)?
                {
                    f.locals.bind(py).del_item(name)?;
                }
                f.exception = previous;
                result
            } else {
                Err(error)
            }
        }
    };
    let previous = f.exception.take();
    if let Err(error) = &result {
        f.exception = Some(error.clone_ref(py));
    }
    let _handled = f
        .exception
        .as_ref()
        .map(|error| HandledException::enter(py, error));
    match block(py, f, array(n, "finalbody")) {
        Ok(Flow::Next) => {}
        other => result = other,
    }
    f.exception = previous;
    result
}
fn statement(py: Python<'_>, f: &mut Frame, n: &Node) -> PyResult<Flow> {
    match op(n) {
        "Pass" => {}
        "Expr" => {
            eval(py, f, &n["value"])?;
        }
        "Assign" => {
            let value = eval(py, f, &n["value"])?;
            for t in array(n, "targets") {
                assign(py, f, t, &value)?;
            }
        }
        "AugAssign" => {
            let left = eval(py, f, &n["target"])?;
            let right = eval(py, f, &n["value"])?;
            let value = binary(py, op(&n["op"]), &left, &right, true)?;
            assign(py, f, &n["target"], &value)?;
        }
        "Delete" => {
            for t in array(n, "targets") {
                delete(py, f, t)?;
            }
        }
        "Return" => return Ok(Flow::Return(eval(py, f, &n["value"])?)),
        "Break" => return Ok(Flow::Break),
        "Continue" => return Ok(Flow::Continue),
        "If" => {
            let truth = truth(py, f, &n["test"])?;
            return block(
                py,
                f,
                if truth {
                    array(n, "body")
                } else {
                    array(n, "orelse")
                },
            );
        }
        "For" => {
            let mut broke = false;
            for value in eval(py, f, &n["iter"])?.bind(py).try_iter()? {
                assign(py, f, &n["target"], &value?.unbind())?;
                match block(py, f, array(n, "body"))? {
                    Flow::Break => {
                        broke = true;
                        break;
                    }
                    Flow::Return(value) => return Ok(Flow::Return(value)),
                    _ => {}
                }
            }
            if !broke {
                return block(py, f, array(n, "orelse"));
            }
        }
        "While" => {
            let mut broke = false;
            while truth(py, f, &n["test"])? {
                match block(py, f, array(n, "body"))? {
                    Flow::Break => {
                        broke = true;
                        break;
                    }
                    Flow::Return(value) => return Ok(Flow::Return(value)),
                    _ => {}
                }
            }
            if !broke {
                return block(py, f, array(n, "orelse"));
            }
        }
        "Assert" => {
            if !truth(py, f, &n["test"])? {
                return Err(PyErr::from_value(
                    py.get_type::<PyAssertionError>()
                        .call1((eval(py, f, &n["msg"])?.bind(py),))?,
                ));
            }
        }
        "Raise" => {
            if n["exc"].is_null() {
                return Err(f
                    .exception
                    .as_ref()
                    .ok_or_else(|| PyRuntimeError::new_err("No active exception to reraise"))?
                    .clone_ref(py));
            }
            let value = eval(py, f, &n["exc"])?;
            let exception = if value.bind(py).is_instance_of::<PyType>() {
                value.bind(py).call0()?.unbind()
            } else {
                value
            };
            let error = PyErr::from_value(exception.bind(py).clone());
            let context = py
                .import("sys")?
                .getattr("exc_info")?
                .call0()?
                .get_item(1)?;
            if !context.is_none() && !error.value(py).is(&context) {
                error.value(py).setattr("__context__", context)?;
            }
            if !n["cause"].is_null() {
                let value = eval(py, f, &n["cause"])?;
                let cause = if value.bind(py).is_none() {
                    None
                } else if value.bind(py).is_instance_of::<PyType>() {
                    Some(PyErr::from_value(value.bind(py).call0()?))
                } else {
                    Some(PyErr::from_value(value.bind(py).clone()))
                };
                error.set_cause(py, cause);
            }
            return Err(error);
        }
        "Try" => {
            let result = block(py, f, array(n, "body"));
            return finish_try(py, f, n, result);
        }
        "With" => return with_items(py, f, array(n, "items"), array(n, "body")),
        "FunctionDef" => {
            let in_class = f.locals.bind(py).contains("__qualname__")?;
            let mut value = function(py, f, n, in_class)?;
            for decorator in array(n, "decorator_list").iter().rev() {
                value = eval(py, f, decorator)?
                    .bind(py)
                    .call1((value.bind(py),))?
                    .unbind();
            }
            if in_class && array(n, "decorator_list").is_empty() {
                let wrapper = match s(n, "name") {
                    "__new__" => Some("staticmethod"),
                    "__class_getitem__" | "__init_subclass__" => Some("classmethod"),
                    _ => None,
                };
                if let Some(wrapper) = wrapper {
                    value = py
                        .import("builtins")?
                        .getattr(wrapper)?
                        .call1((value.bind(py),))?
                        .unbind();
                }
            }
            f.locals.bind(py).set_item(s(n, "name"), value.bind(py))?;
        }
        "ClassDef" => {
            let cls = class(py, f, n)?;
            f.locals.bind(py).set_item(s(n, "name"), cls.bind(py))?;
        }
        "Import" => {
            for alias in array(n, "names") {
                let name = s(alias, "name");
                let module = py.import(name)?;
                if let Some(asname) = alias["asname"].as_str() {
                    f.locals.bind(py).set_item(asname, module)?;
                } else {
                    let root = name.split('.').next().unwrap();
                    f.locals.bind(py).set_item(root, py.import(root)?)?;
                }
            }
        }
        "ImportFrom" => {
            let module = py.import(s(n, "module"))?;
            for alias in array(n, "names") {
                let name = s(alias, "name");
                let value = match module.getattr(name) {
                    Ok(v) => v,
                    Err(e) if e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {
                        py.import(format!("{}.{name}", s(n, "module")))?.into_any()
                    }
                    Err(e) => return Err(e),
                };
                let alias = alias["asname"].as_str().unwrap_or(name);
                f.locals.bind(py).set_item(alias, value)?;
            }
        }
        _ => {
            return Err(PyRuntimeError::new_err(format!(
                "unsupported native statement {}",
                op(n)
            )));
        }
    }
    Ok(Flow::Next)
}
fn with_items(py: Python<'_>, f: &mut Frame, items: &[Node], body: &[Node]) -> PyResult<Flow> {
    let Some((item, tail)) = items.split_first() else {
        return block(py, f, body);
    };
    let manager = eval(py, f, &item["context_expr"])?;
    let owner = manager.bind(py).get_type();
    let exit = owner.getattr("__exit__")?;
    let entered = owner
        .getattr("__enter__")?
        .call1((manager.bind(py),))?
        .unbind();
    if !item["optional_vars"].is_null() {
        assign(py, f, &item["optional_vars"], &entered)?;
    }
    match with_items(py, f, tail, body) {
        Ok(flow) => {
            exit.call1((manager.bind(py), py.None(), py.None(), py.None()))?;
            Ok(flow)
        }
        Err(error) => {
            let _handled = HandledException::enter(py, &error);
            let suppressed = exit
                .call1((
                    manager.bind(py),
                    error.get_type(py),
                    error.value(py),
                    error.traceback(py),
                ))?
                .is_truthy()?;
            if suppressed {
                Ok(Flow::Next)
            } else {
                Err(error)
            }
        }
    }
}
fn class(py: Python<'_>, f: &mut Frame, n: &Node) -> PyResult<Object> {
    let mut bases = array(n, "bases")
        .iter()
        .map(|n| eval(py, f, n))
        .collect::<PyResult<Vec<_>>>()?;
    if bases.is_empty() {
        bases.push(py.get_type::<EngineState>().into_any().unbind());
    }
    if s(n, "name") == "BindingDict" {
        bases.push(py.get_type::<EngineState>().into_any().unbind());
    }
    let kwargs = PyDict::new(py);
    let mut meta = None;
    for kw in array(n, "keywords") {
        let value = eval(py, f, &kw["value"])?;
        if s(kw, "arg") == "metaclass" {
            meta = Some(value);
        } else {
            kwargs.set_item(s(kw, "arg"), value.bind(py))?;
        }
    }
    let meta = if let Some(meta) = meta {
        meta
    } else {
        let mut meta = py.get_type::<PyType>().into_any().unbind();
        for base in &bases {
            let candidate = base.bind(py).get_type();
            let current = meta.bind(py).cast::<PyType>()?;
            if candidate.is_subclass(current)? {
                meta = candidate.into_any().unbind();
            } else if !current.is_subclass(&candidate)? {
                return Err(PyTypeError::new_err("metaclass conflict"));
            }
        }
        meta
    };
    let namespace = PyDict::new(py);
    namespace.set_item(
        "__module__",
        f.globals.bind(py).get_item("__name__")?.unwrap(),
    )?;
    namespace.set_item("__qualname__", s(n, "name"))?;
    if let Some(first) = array(n, "body").first()
        && op(first) == "Expr"
        && op(&first["value"]) == "Constant"
        && first["value"]["value"].is_string()
    {
        namespace.set_item("__doc__", s(&first["value"], "value"))?;
    }
    let cell = PyList::empty(py);
    let mut cf = Frame {
        globals: f.globals.clone_ref(py),
        locals: namespace.clone().unbind(),
        closure: lexical_scope(py, f, !f.in_class)?,
        class_cell: Some(cell.clone().unbind()),
        local_names: Arc::new(HashSet::new()),
        first: None,
        exception: None,
        in_class: true,
    };
    block(py, &mut cf, array(n, "body"))?;
    if bases.len() == 1
        && bases[0].bind(py).is(py.get_type::<EngineState>())
        && namespace.contains("__slots__")?
    {
        bases[0] = py.get_type::<EngineSlotState>().into_any().unbind();
    }
    let cls = if s(n, "name") == "SerializerMetaclass" {
        let cls = abi::metaclass(py, engine_metaclass_new)?;
        cls.setattr("__name__", s(n, "name"))?;
        for (key, value) in namespace.iter() {
            cls.setattr(key.cast::<PyString>()?, value)?;
        }
        cls.into_any().unbind()
    } else {
        meta.bind(py)
            .call(
                (s(n, "name"), PyTuple::new(py, bases)?, namespace),
                Some(&kwargs),
            )?
            .unbind()
    };
    cell.append(cls.bind(py))?;
    let mut cls = cls;
    for decorator in array(n, "decorator_list").iter().rev() {
        cls = eval(py, f, decorator)?
            .bind(py)
            .call1((cls.bind(py),))?
            .unbind();
    }
    Ok(cls)
}
unsafe extern "C" fn engine_metaclass_new(
    meta: *mut pyo3::ffi::PyTypeObject,
    args: *mut pyo3::ffi::PyObject,
    kwargs: *mut pyo3::ffi::PyObject,
) -> *mut pyo3::ffi::PyObject {
    abi::callback(|py| {
        let meta = unsafe { abi::borrowed(py, meta.cast()) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        let all = PyTuple::new(
            py,
            std::iter::once(meta.clone())
                .chain(args.iter())
                .collect::<Vec<_>>(),
        )?;
        Ok(meta.getattr("__new__")?.call(all, Some(&kwargs))?.unbind())
    })
}

// Suspension is held in native iterator objects. Yielding functions and
// generator expressions keep their Python values alive and are GC traversable.
enum Task {
    ReadableFields(fast::ReadableFields),
    Block(Vec<Node>, usize),
    For(Node, Object),
    While(Node),
    Comp(Node, usize, Object),
    Emit(Node),
}
#[pyclass(weakref, module = "rustializer._native")]
struct EngineGenerator {
    frame: Option<Frame>,
    tasks: Vec<Task>,
    running: bool,
    started: bool,
}
#[pymethods]
impl EngineGenerator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }
    fn __next__(slf: &Bound<'_, Self>) -> PyResult<Option<Object>> {
        let py = slf.py();
        let (mut frame, mut tasks) = {
            let mut generator = slf.borrow_mut();
            if generator.running {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "generator already executing",
                ));
            }
            let Some(frame) = generator.frame.take() else {
                return Ok(None);
            };
            generator.running = true;
            generator.started = true;
            (frame, std::mem::take(&mut generator.tasks))
        };
        let _context = crate::warnings::Context::enter(py, frame.globals.bind(py), 1)?;
        let result = resume(py, &mut frame, &mut tasks);
        let mut generator = slf.borrow_mut();
        generator.running = false;
        if matches!(&result, Ok(Some(_))) {
            generator.frame = Some(frame);
            generator.tasks = tasks;
        }
        match result {
            Err(error) if error.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {
                let replacement = PyRuntimeError::new_err("generator raised StopIteration");
                replacement
                    .value(py)
                    .setattr("__context__", error.value(py))?;
                replacement.set_cause(py, Some(error));
                Err(replacement)
            }
            other => other,
        }
    }
    fn send(slf: &Bound<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<Object> {
        if slf.borrow().frame.is_some() && !slf.borrow().started && !value.is_none() {
            return Err(PyTypeError::new_err(
                "can't send non-None value to a just-started generator",
            ));
        }
        Self::__next__(slf)?.ok_or_else(|| pyo3::exceptions::PyStopIteration::new_err(()))
    }
    fn close(&mut self) -> PyResult<()> {
        if self.running {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "generator already executing",
            ));
        }
        self.frame = None;
        self.tasks.clear();
        Ok(())
    }
    #[pyo3(signature=(typ, value=None, traceback=None))]
    fn throw(
        &mut self,
        py: Python<'_>,
        typ: &Bound<'_, PyAny>,
        value: Option<&Bound<'_, PyAny>>,
        traceback: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Object> {
        if self.running {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "generator already executing",
            ));
        }
        let error = if typ.is_instance_of::<PyType>() {
            let value = value.filter(|v| !v.is_none());
            let exception = if let Some(value) = value {
                if value.is_instance(typ)? {
                    value.clone()
                } else if let Ok(args) = value.cast::<PyTuple>() {
                    typ.call(args, None)?
                } else {
                    typ.call1((value,))?
                }
            } else {
                typ.call0()?
            };
            PyErr::from_value(exception)
        } else {
            if value.is_some_and(|v| !v.is_none()) {
                return Err(PyTypeError::new_err(
                    "instance exception may not have a separate value",
                ));
            }
            PyErr::from_value(typ.clone())
        };
        if let Some(traceback) = traceback.filter(|v| !v.is_none()) {
            error.value(py).setattr("__traceback__", traceback)?;
        }
        self.frame = None;
        self.tasks.clear();
        Err(error)
    }
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(frame) = &self.frame {
            frame.visit(&visit)?;
        }
        for task in &self.tasks {
            match task {
                Task::ReadableFields(fields) => visit.call(&fields.iterator)?,
                Task::For(_, v) | Task::Comp(_, _, v) => visit.call(v)?,
                _ => {}
            }
        }
        Ok(())
    }
    fn __clear__(&mut self) {
        self.frame = None;
        self.tasks.clear();
    }
}
fn resume(py: Python<'_>, f: &mut Frame, tasks: &mut Vec<Task>) -> PyResult<Option<Object>> {
    while let Some(task) = tasks.pop() {
        match task {
            Task::ReadableFields(mut fields) => {
                let next = fields.resume(py, f)?;
                if next.is_some() {
                    tasks.push(Task::ReadableFields(fields));
                }
                return Ok(next);
            }
            Task::Block(nodes, index) => {
                let Some(n) = nodes.get(index).cloned() else {
                    continue;
                };
                tasks.push(Task::Block(nodes, index + 1));
                match op(&n) {
                    "Expr" if op(&n["value"]) == "Yield" => {
                        return Ok(Some(eval(py, f, &n["value"]["value"])?));
                    }
                    "If" => {
                        let body = if truth(py, f, &n["test"])? {
                            array(&n, "body")
                        } else {
                            array(&n, "orelse")
                        };
                        tasks.push(Task::Block(body.to_vec(), 0));
                    }
                    "For" => {
                        let iterator = eval(py, f, &n["iter"])?
                            .bind(py)
                            .try_iter()?
                            .into_any()
                            .unbind();
                        tasks.push(Task::For(n, iterator));
                    }
                    "While" => tasks.push(Task::While(n)),
                    "Return" => {
                        tasks.clear();
                        return Ok(None);
                    }
                    "Break" => {
                        while let Some(t) = tasks.pop() {
                            if matches!(t, Task::For(_, _) | Task::While(_)) {
                                break;
                            }
                        }
                    }
                    "Continue" => {
                        while let Some(t) = tasks.pop() {
                            if matches!(t, Task::For(_, _) | Task::While(_)) {
                                tasks.push(t);
                                break;
                            }
                        }
                    }
                    _ => {
                        statement(py, f, &n)?;
                    }
                }
            }
            Task::For(n, iterator) => {
                let next = iterator.bind(py).call_method0("__next__");
                match next {
                    Ok(value) => {
                        assign(py, f, &n["target"], &value.unbind())?;
                        let body = array(&n, "body").to_vec();
                        tasks.push(Task::For(n, iterator));
                        tasks.push(Task::Block(body, 0));
                    }
                    Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {
                        tasks.push(Task::Block(array(&n, "orelse").to_vec(), 0))
                    }
                    Err(e) => return Err(e),
                }
            }
            Task::While(n) => {
                if truth(py, f, &n["test"])? {
                    let body = array(&n, "body").to_vec();
                    tasks.push(Task::While(n));
                    tasks.push(Task::Block(body, 0));
                } else {
                    tasks.push(Task::Block(array(&n, "orelse").to_vec(), 0));
                }
            }
            Task::Comp(n, index, iterator) => match iterator.bind(py).call_method0("__next__") {
                Ok(value) => {
                    let generator = &array(&n, "generators")[index];
                    assign(py, f, &generator["target"], &value.unbind())?;
                    let mut accepted = true;
                    for test in array(generator, "ifs") {
                        if !truth(py, f, test)? {
                            accepted = false;
                            break;
                        }
                    }
                    tasks.push(Task::Comp(n.clone(), index, iterator));
                    if accepted {
                        if index + 1 == array(&n, "generators").len() {
                            tasks.push(Task::Emit(n));
                        } else {
                            let next = eval(py, f, &array(&n, "generators")[index + 1]["iter"])?
                                .bind(py)
                                .try_iter()?
                                .into_any()
                                .unbind();
                            tasks.push(Task::Comp(n, index + 1, next));
                        }
                    }
                }
                Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
                Err(e) => return Err(e),
            },
            Task::Emit(n) => {
                return if op(&n) == "DictComp" {
                    Ok(Some(
                        PyTuple::new(py, [eval(py, f, &n["key"])?, eval(py, f, &n["value"])?])?
                            .into_any()
                            .unbind(),
                    ))
                } else {
                    Ok(Some(eval(py, f, &n["elt"])?))
                };
            }
        }
    }
    Ok(None)
}
fn comprehension(py: Python<'_>, f: &mut Frame, n: &Node) -> PyResult<Object> {
    let first = eval(py, f, &array(n, "generators")[0]["iter"])?
        .bind(py)
        .try_iter()?
        .into_any()
        .unbind();
    let frame = f.child(py)?;
    let generator = Py::new(
        py,
        EngineGenerator {
            frame: Some(frame),
            tasks: vec![Task::Comp(n.clone(), 0, first)],
            running: false,
            started: false,
        },
    )?;
    match op(n) {
        "GeneratorExp" => Ok(generator.into_any()),
        "ListComp" => Ok(py
            .import("builtins")?
            .getattr("list")?
            .call1((generator,))?
            .unbind()),
        "SetComp" => Ok(py
            .import("builtins")?
            .getattr("set")?
            .call1((generator,))?
            .unbind()),
        _ => Ok(py
            .import("builtins")?
            .getattr("dict")?
            .call1((generator,))?
            .unbind()),
    }
}

pub fn execute(py: Python<'_>, module: &Bound<'_, PyModule>, definition: &Node) -> PyResult<()> {
    let namespace = module.dict();
    namespace.set_item(
        "__package__",
        s(definition, "name").rsplit_once('.').unwrap().0,
    )?;
    namespace.set_item("__builtins__", py.import("builtins")?.dict())?;
    let mut frame = Frame {
        globals: namespace.clone().unbind(),
        locals: namespace.unbind(),
        closure: None,
        class_cell: None,
        local_names: Arc::new(HashSet::new()),
        first: None,
        exception: None,
        in_class: false,
    };
    block(py, &mut frame, array(definition, "body"))?;
    Ok(())
}
pub fn install(py: Python<'_>, program: &Node) -> PyResult<()> {
    let modules = py
        .import("sys")?
        .getattr("modules")?
        .cast_into::<PyDict>()?;
    let definitions: Vec<_> = array(program, "modules")
        .iter()
        .map(|definition| Arc::new(definition.clone()))
        .collect();
    for definition in &definitions {
        let name = s(definition, "name");
        let module = PyModule::new(py, name)?;
        crate::loader::metadata(py, &module, definition.clone())?;
        execute(py, &module, definition)?;
        modules.set_item(name, &module)?;
        let (parent, attr) = name.rsplit_once('.').unwrap();
        py.import(parent)?.setattr(attr, &module)?;
    }
    crate::loader::install(py, definitions)?;
    Ok(())
}
