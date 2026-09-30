//! The small boundary between native callbacks and the Python 3.10 Stable ABI.

use std::ffi::{CStr, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use pyo3::exceptions::PyRuntimeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple, PyType};

pub fn callback(body: impl FnOnce(Python<'_>) -> PyResult<Py<PyAny>>) -> *mut ffi::PyObject {
    // CPython invokes these callbacks while attached and holding the GIL.
    let py = unsafe { Python::assume_attached() };
    match catch_unwind(AssertUnwindSafe(|| body(py))) {
        Ok(Ok(value)) => value.into_ptr(),
        Ok(Err(error)) => {
            error.restore(py);
            std::ptr::null_mut()
        }
        Err(_) => {
            PyRuntimeError::new_err("Rust panic in native object-model experiment").restore(py);
            std::ptr::null_mut()
        }
    }
}

// Every pointer borrowed here is a non-null argument owned by CPython for the
// duration of the callback. Bound adds a reference, so Python re-entry is safe.
pub unsafe fn borrowed<'py>(py: Python<'py>, value: *mut ffi::PyObject) -> Bound<'py, PyAny> {
    unsafe { Bound::from_borrowed_ptr(py, value) }
}

pub unsafe fn arguments<'py>(
    py: Python<'py>,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> PyResult<(Bound<'py, PyTuple>, Bound<'py, PyDict>)> {
    let args = unsafe { borrowed(py, args) }.cast_into::<PyTuple>()?;
    let kwargs = if kwargs.is_null() {
        PyDict::new(py)
    } else {
        unsafe { borrowed(py, kwargs) }.cast_into::<PyDict>()?
    };
    Ok((args, kwargs))
}

pub fn method(
    name: &'static CStr,
    function: ffi::PyCFunctionWithKeywords,
    doc: &'static CStr,
) -> ffi::PyMethodDef {
    ffi::PyMethodDef {
        ml_name: name.as_ptr(),
        ml_meth: ffi::PyMethodDefPointer {
            PyCFunctionWithKeywords: function,
        },
        ml_flags: ffi::METH_VARARGS | ffi::METH_KEYWORDS,
        ml_doc: doc.as_ptr(),
    }
}

pub fn install_method(class: &Bound<'_, PyType>, definition: ffi::PyMethodDef) -> PyResult<()> {
    let method = crate::method::NativeMethod::new(class, definition);
    let name = method.name.clone();
    class.setattr(name, Bound::new(class.py(), method)?)
}

pub fn install_new(class: &Bound<'_, PyType>, definition: ffi::PyMethodDef) -> PyResult<()> {
    let definition = Box::leak(Box::new(definition));
    let py = class.py();
    let function = unsafe {
        Bound::from_owned_ptr_or_err(
            py,
            ffi::PyCFunction_NewEx(definition, std::ptr::null_mut(), std::ptr::null_mut()),
        )?
    };
    let static_method = py
        .import("builtins")?
        .getattr("staticmethod")?
        .call1((function,))?;
    class.setattr("__new__", static_method)
}

pub fn metaclass(py: Python<'_>, new: ffi::newfunc) -> PyResult<Bound<'_, PyType>> {
    let mut slots = [
        ffi::PyType_Slot {
            slot: ffi::Py_tp_new,
            pfunc: new as *mut c_void,
        },
        ffi::PyType_Slot {
            slot: 0,
            pfunc: std::ptr::null_mut(),
        },
    ];
    let mut spec = ffi::PyType_Spec {
        name: c"rustializer._feasibility.SerializerMetaclass".as_ptr(),
        // Inherit type's opaque layout; never size or dereference PyTypeObject.
        basicsize: 0,
        itemsize: 0,
        flags: ffi::Py_TPFLAGS_BASETYPE as u32,
        slots: slots.as_mut_ptr(),
    };
    let bases = PyTuple::new(py, [py.get_type::<PyType>()])?;
    unsafe {
        Bound::from_owned_ptr_or_err(py, ffi::PyType_FromSpecWithBases(&mut spec, bases.as_ptr()))?
    }
    .cast_into::<PyType>()
    .map_err(Into::into)
}

pub unsafe fn builtin_new<'py>(
    py: Python<'py>,
    base: &Bound<'py, PyType>,
    subtype: &Bound<'py, PyType>,
    args: &Bound<'py, PyTuple>,
    kwargs: &Bound<'py, PyDict>,
) -> PyResult<Bound<'py, PyAny>> {
    // Slot access is Stable ABI, including built-in types from Python 3.10.
    let slot = unsafe { ffi::PyType_GetSlot(base.as_type_ptr(), ffi::Py_tp_new) };
    if slot.is_null() {
        return Err(PyRuntimeError::new_err(
            "base type has no native constructor",
        ));
    }
    let new: ffi::newfunc = unsafe { std::mem::transmute(slot) };
    unsafe {
        Bound::from_owned_ptr_or_err(
            py,
            new(subtype.as_type_ptr(), args.as_ptr(), kwargs.as_ptr()),
        )
    }
}
