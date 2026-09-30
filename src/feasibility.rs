//! Test types, not an activation API or a partial replacement for DRF.

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::PyTypeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple, PyType};

use crate::abi;

const MODULE: &str = "rustializer._feasibility";

#[pyclass(
    subclass,
    dict,
    weakref,
    name = "_NativeState",
    module = "rustializer._feasibility"
)]
struct NativeState;

#[pymethods]
impl NativeState {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }

    fn __traverse__(&self, _visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        // PyO3 traverses the instance dictionary. No additional Rust references.
        Ok(())
    }
}

fn class<'py>(
    meta: &Bound<'py, PyType>,
    name: &str,
    bases: &[Bound<'py, PyType>],
) -> PyResult<Bound<'py, PyType>> {
    let py = meta.py();
    let namespace = PyDict::new(py);
    namespace.set_item("__module__", MODULE)?;
    meta.call1((name, PyTuple::new(py, bases)?, namespace))?
        .cast_into::<PyType>()
        .map_err(Into::into)
}

fn one<'py>(
    args: &Bound<'py, PyTuple>,
    kwargs: &Bound<'py, PyDict>,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    if args.len() > 1 || kwargs.len() > usize::from(args.is_empty()) {
        return Err(PyTypeError::new_err(format!(
            "expected one argument: {name}"
        )));
    }
    if args.len() == 1 {
        args.get_item(0)
    } else {
        kwargs
            .get_item(name)?
            .ok_or_else(|| PyTypeError::new_err(format!("missing argument: {name}")))
    }
}

fn initialize(
    slf: &Bound<'_, PyAny>,
    args: &Bound<'_, PyTuple>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<()> {
    slf.setattr("_args", args)?;
    slf.setattr("_kwargs", kwargs.copy()?)?;
    for (key, value) in kwargs.iter() {
        slf.setattr(key.cast::<PyString>()?, value)?;
    }
    let root = slf.getattr("_field_root")?;
    let counter: usize = root.getattr("_creation_counter")?.extract()?;
    slf.setattr("_creation_counter", counter)?;
    root.setattr("_creation_counter", counter + 1)
}

unsafe extern "C" fn field_init(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        initialize(&slf, &args, &kwargs)?;
        Ok(py.None())
    })
}

unsafe extern "C" fn field_representation(
    _slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        Ok(one(&args, &kwargs, "value")?.unbind())
    })
}

unsafe extern "C" fn field_bind(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        if args.len() != 2 || !kwargs.is_empty() {
            return Err(PyTypeError::new_err("bind expects field_name and parent"));
        }
        slf.setattr("field_name", args.get_item(0)?)?;
        slf.setattr("parent", args.get_item(1)?)?;
        Ok(py.None())
    })
}

unsafe extern "C" fn field_deepcopy(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        let memo = one(&args, &kwargs, "memo")?;
        let deepcopy = py.import("copy")?.getattr("deepcopy")?;
        let copied_args = deepcopy
            .call1((slf.getattr("_args")?, &memo))?
            .cast_into::<PyTuple>()?;
        let copied_kwargs = PyDict::new(py);
        for (key, value) in slf.getattr("_kwargs")?.cast_into::<PyDict>()?.iter() {
            let name: String = key.extract()?;
            let value = if matches!(name.as_str(), "validators" | "regex") {
                value
            } else {
                deepcopy.call1((value, &memo))?
            };
            copied_kwargs.set_item(key, value)?;
        }
        Ok(slf
            .get_type()
            .call(copied_args, Some(&copied_kwargs))?
            .unbind())
    })
}

unsafe extern "C" fn field_reduce(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        if !args.is_empty() || !kwargs.is_empty() {
            return Err(PyTypeError::new_err("__reduce__ expects no arguments"));
        }
        let reconstruct = py.import("copyreg")?.getattr("__newobj__")?;
        let new_args = PyTuple::new(py, [slf.get_type()])?;
        Ok((reconstruct, new_args, slf.getattr("__dict__")?)
            .into_pyobject(py)?
            .into_any()
            .unbind())
    })
}

unsafe extern "C" fn metaclass_new(
    meta: *mut ffi::PyTypeObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let meta = unsafe { abi::borrowed(py, meta.cast()) }.cast_into::<PyType>()?;
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        if args.len() != 3 {
            return Err(PyTypeError::new_err(
                "metaclass expects name, bases, and namespace",
            ));
        }
        let namespace = args.get_item(2)?.cast_into::<PyDict>()?.copy()?;
        let field_type = meta.getattr("_field_type")?;
        let mut declared = Vec::new();
        for (name, value) in namespace.iter() {
            if value.is_instance(&field_type)? {
                let counter: usize = value.getattr("_creation_counter")?.extract()?;
                declared.push((counter, name, value));
            }
        }
        declared.sort_by_key(|(counter, _, _)| *counter);
        for (_, name, _) in &declared {
            namespace.del_item(name)?;
        }
        let fields = PyDict::new(py);
        let known = namespace.copy()?;
        let bases = args.get_item(1)?.cast_into::<PyTuple>()?;
        for base in bases.iter() {
            match base.getattr("_declared_fields") {
                Ok(inherited) => {
                    for (name, value) in inherited.cast_into::<PyDict>()?.iter() {
                        if !known.contains(&name)? {
                            known.set_item(&name, py.None())?;
                            fields.set_item(name, value)?;
                        }
                    }
                }
                Err(error) if error.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {}
                Err(error) => return Err(error),
            }
        }
        for (_, name, value) in declared {
            fields.set_item(name, value)?;
        }
        namespace.set_item("_declared_fields", fields)?;
        let type_args = (args.get_item(0)?, bases, namespace).into_pyobject(py)?;
        Ok(
            unsafe { abi::builtin_new(py, &py.get_type::<PyType>(), &meta, &type_args, &kwargs) }?
                .unbind(),
        )
    })
}

unsafe extern "C" fn serializer_init(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        initialize(&slf, &args, &kwargs)?;
        let fields = py
            .import("copy")?
            .getattr("deepcopy")?
            .call1((slf.getattr("_declared_fields")?,))?;
        for (name, field) in fields.cast::<PyDict>()?.iter() {
            field.call_method1("bind", (name, &slf))?;
        }
        slf.setattr("fields", fields)?;
        Ok(py.None())
    })
}

unsafe extern "C" fn serializer_representation(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        let instance = one(&args, &kwargs, "instance")?;
        let result = PyDict::new(py);
        // Late Python dispatch and no held Rust borrow allow overrides and re-entry.
        for (name, field) in slf.getattr("fields")?.cast::<PyDict>()?.iter() {
            let value = if instance.is_instance_of::<PyDict>() {
                instance.get_item(&name)?
            } else {
                instance.getattr(name.cast::<PyString>()?)?
            };
            result.set_item(name, field.call_method1("to_representation", (value,))?)?;
        }
        Ok(result.into_any().unbind())
    })
}

unsafe extern "C" fn list_representation(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let slf = unsafe { abi::borrowed(py, slf) };
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        let data = one(&args, &kwargs, "data")?;
        let child = slf.getattr("child")?;
        let result = PyList::empty(py);
        for value in data.try_iter()? {
            result.append(child.call_method1("to_representation", (value?,))?)?;
        }
        Ok(result.into_any().unbind())
    })
}

unsafe extern "C" fn error_detail_new(
    _slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| {
        let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
        if !(2..=3).contains(&args.len()) || kwargs.len() > 1 {
            return Err(PyTypeError::new_err(
                "ErrorDetail expects value and optional code",
            ));
        }
        let subtype = args.get_item(0)?.cast_into::<PyType>()?;
        let value_args = PyTuple::new(py, [args.get_item(1)?])?;
        let value = unsafe {
            abi::builtin_new(
                py,
                &py.get_type::<PyString>(),
                &subtype,
                &value_args,
                &PyDict::new(py),
            )
        }?;
        let code = if args.len() == 3 {
            if !kwargs.is_empty() {
                return Err(PyTypeError::new_err("duplicate code argument"));
            }
            args.get_item(2)?
        } else {
            kwargs
                .get_item("code")?
                .unwrap_or_else(|| py.None().into_bound(py))
        };
        value.setattr("code", code)?;
        Ok(value.unbind())
    })
}

fn container_init(
    py: Python<'_>,
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
    base: &Bound<'_, PyType>,
) -> PyResult<Py<PyAny>> {
    let slf = unsafe { abi::borrowed(py, slf) };
    let (args, kwargs) = unsafe { abi::arguments(py, args, kwargs) }?;
    let kwargs = kwargs.copy()?;
    let serializer = kwargs
        .get_item("serializer")?
        .unwrap_or_else(|| py.None().into_bound(py));
    if kwargs.contains("serializer")? {
        kwargs.del_item("serializer")?;
    }
    let mut init_args = vec![slf.clone()];
    init_args.extend(args.iter());
    base.getattr("__init__")?
        .call(PyTuple::new(py, init_args)?, Some(&kwargs))?;
    slf.setattr("serializer", serializer)?;
    Ok(py.None())
}

unsafe extern "C" fn dict_init(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| container_init(py, slf, args, kwargs, &py.get_type::<PyDict>()))
}

unsafe extern "C" fn list_init(
    slf: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| container_init(py, slf, args, kwargs, &py.get_type::<PyList>()))
}

fn container_reduce(
    py: Python<'_>,
    slf: *mut ffi::PyObject,
    base: &Bound<'_, PyType>,
) -> PyResult<Py<PyAny>> {
    let value = base.call1((unsafe { abi::borrowed(py, slf) },))?;
    Ok((base, (value,)).into_pyobject(py)?.into_any().unbind())
}

unsafe extern "C" fn dict_reduce(
    slf: *mut ffi::PyObject,
    _args: *mut ffi::PyObject,
    _kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| container_reduce(py, slf, &py.get_type::<PyDict>()))
}

unsafe extern "C" fn list_reduce(
    slf: *mut ffi::PyObject,
    _args: *mut ffi::PyObject,
    _kwargs: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    abi::callback(|py| container_reduce(py, slf, &py.get_type::<PyList>()))
}

pub fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add_class::<NativeState>()?;
    let root = py.get_type::<NativeState>();
    let field = class(&py.get_type::<PyType>(), "Field", &[root])?;
    field.setattr("_field_root", &field)?;
    field.setattr("_creation_counter", 0)?;
    for definition in [
        abi::method(
            c"__init__",
            field_init,
            c"Initialize constructor state in the instance dictionary.",
        ),
        abi::method(
            c"to_representation",
            field_representation,
            c"to_representation($self, value)\n--\n\nReturn a value through a native descriptor.",
        ),
        abi::method(
            c"bind",
            field_bind,
            c"bind($self, field_name, parent)\n--\n\nBind a field to its parent.",
        ),
        abi::method(
            c"__deepcopy__",
            field_deepcopy,
            c"Reconstruct from constructor arguments.",
        ),
        abi::method(
            c"__reduce__",
            field_reduce,
            c"Preserve instance state for pickle.",
        ),
    ] {
        abi::install_method(&field, definition)?;
    }
    module.add("Field", &field)?;

    let meta = abi::metaclass(py, metaclass_new)?;
    meta.setattr("_field_type", &field)?;
    module.add("SerializerMetaclass", &meta)?;
    let base = class(&py.get_type::<PyType>(), "BaseSerializer", &[field])?;
    let serializer = class(&meta, "Serializer", std::slice::from_ref(&base))?;
    abi::install_method(
        &serializer,
        abi::method(
            c"__init__",
            serializer_init,
            c"Clone and bind declared fields.",
        ),
    )?;
    abi::install_method(&serializer, abi::method(c"to_representation", serializer_representation, c"to_representation($self, instance)\n--\n\nExercise native loops with Python override dispatch."))?;
    let model = class(&meta, "ModelSerializer", std::slice::from_ref(&serializer))?;
    let list = class(
        &py.get_type::<PyType>(),
        "ListSerializer",
        std::slice::from_ref(&base),
    )?;
    abi::install_method(
        &list,
        abi::method(
            c"to_representation",
            list_representation,
            c"to_representation($self, data)\n--\n\nExercise child dispatch.",
        ),
    )?;
    module.add("BaseSerializer", base)?;
    module.add("Serializer", serializer)?;
    module.add("ModelSerializer", model)?;
    module.add("ListSerializer", list)?;

    let detail = class(
        &py.get_type::<PyType>(),
        "ErrorDetail",
        &[py.get_type::<PyString>()],
    )?;
    abi::install_new(
        &detail,
        abi::method(
            c"__new__",
            error_detail_new,
            c"Construct a string subclass without knowing its layout.",
        ),
    )?;
    module.add("ErrorDetail", detail)?;
    let dict = class(
        &py.get_type::<PyType>(),
        "ReturnDict",
        &[py.get_type::<PyDict>()],
    )?;
    abi::install_method(
        &dict,
        abi::method(
            c"__init__",
            dict_init,
            c"Initialize a dict subclass with a serializer backlink.",
        ),
    )?;
    abi::install_method(
        &dict,
        abi::method(
            c"__reduce__",
            dict_reduce,
            c"Drop the backlink when pickling.",
        ),
    )?;
    module.add("ReturnDict", dict)?;
    let list = class(
        &py.get_type::<PyType>(),
        "ReturnList",
        &[py.get_type::<PyList>()],
    )?;
    abi::install_method(
        &list,
        abi::method(
            c"__init__",
            list_init,
            c"Initialize a list subclass with a serializer backlink.",
        ),
    )?;
    abi::install_method(
        &list,
        abi::method(
            c"__reduce__",
            list_reduce,
            c"Drop the backlink when pickling.",
        ),
    )?;
    module.add("ReturnList", list)?;
    Ok(())
}
