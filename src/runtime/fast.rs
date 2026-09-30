//! Direct Rust implementations of frequent, pinned engine operations.
//! Every attribute/global lookup stays live and calls the ordinary Python hook.
use super::*;

#[derive(Clone)]
pub(super) enum Plan {
    Scalar(&'static str, u32),
    FieldValidation([u32; 3]),
    SerializerRepresentation([u32; 3]),
    ReadableFields([u32; 3]),
    ListRepresentation([u32; 2]),
    SimpleCallable(usize, u32),
    FieldAttribute(usize, u32),
    AttributePath(usize, [u32; 3]),
}

fn site(node: &Node, name: &str) -> u32 {
    if (op(node) == "Attribute" && s(node, "attr") == name)
        || (op(node) == "Name" && s(node, "id") == name)
    {
        return node["line"].as_u64().unwrap_or(1) as u32;
    }
    let children: Vec<_> = match node {
        Node::Object(values) => values.values().collect(),
        Node::Array(values) => values.iter().collect(),
        _ => Vec::new(),
    };
    children
        .into_iter()
        .map(|node| site(node, name))
        .find(|line| *line != 0)
        .unwrap_or(0)
}

impl Plan {
    pub(super) fn new(module: &str, name: &str, node: &Node) -> Option<Self> {
        match (module, name) {
            ("rest_framework.fields", "CharField.to_representation") => {
                Some(Self::Scalar("str", site(node, "str")))
            }
            ("rest_framework.fields", "IntegerField.to_representation") => {
                Some(Self::Scalar("int", site(node, "int")))
            }
            ("rest_framework.fields", "FloatField.to_representation") => {
                Some(Self::Scalar("float", site(node, "float")))
            }
            ("rest_framework.fields", "Field.run_validation") => Some(Self::FieldValidation([
                site(node, "validate_empty_values"),
                site(node, "to_internal_value"),
                site(node, "run_validators"),
            ])),
            ("rest_framework.serializers", "Serializer.to_representation") => {
                Some(Self::SerializerRepresentation([
                    site(node, "_readable_fields"),
                    site(node, "get_attribute"),
                    site(node, "to_representation"),
                ]))
            }
            ("rest_framework.serializers", "Serializer._readable_fields") => {
                let loop_node = array(node, "body").first()?;
                let filter = array(loop_node, "body").first()?;
                let emit = array(filter, "body").first()?;
                Some(Self::ReadableFields([
                    site(node, "values"),
                    site(node, "write_only"),
                    emit["line"].as_u64().unwrap_or(1) as u32,
                ]))
            }
            ("rest_framework.serializers", "ListSerializer.to_representation") => {
                Some(Self::ListRepresentation([
                    site(node, "all"),
                    site(node, "to_representation"),
                ]))
            }
            ("rest_framework.fields", "is_simple_callable") => {
                let index = array(node, "body")
                    .iter()
                    .position(|statement| op(statement) == "If")?;
                Some(Self::SimpleCallable(index + 1, site(node, "callable")))
            }
            ("rest_framework.fields", "Field.get_attribute") => {
                let index = array(node, "body").iter().position(|n| op(n) == "Try")?;
                Some(Self::FieldAttribute(index, site(node, "get_attribute")))
            }
            ("rest_framework.fields", "get_attribute") => {
                let index = array(node, "body").iter().position(|n| op(n) == "For")?;
                Some(Self::AttributePath(
                    index,
                    [
                        site(node, "isinstance"),
                        site(node, "getattr"),
                        site(node, "is_simple_callable"),
                    ],
                ))
            }
            _ => None,
        }
    }

    pub(super) fn execute(
        &self,
        py: Python<'_>,
        frame: &mut Frame,
        node: &Node,
    ) -> PyResult<Object> {
        match self {
            Self::ReadableFields(_) => Err(PyRuntimeError::new_err(
                "readable fields must execute as a generator",
            )),
            Self::FieldAttribute(index, line) => {
                crate::warnings::mark(*line);
                let callable = frame.lookup(py, "get_attribute")?;
                let instance = frame.lookup(py, "instance")?;
                let attrs = frame.lookup(py, "self")?.bind(py).getattr("source_attrs")?;
                match callable.bind(py).call1((instance.bind(py), attrs)) {
                    Ok(value) => Ok(value.unbind()),
                    Err(error) => returned(
                        py,
                        finish_try(py, frame, &array(node, "body")[*index], Err(error))?,
                    ),
                }
            }
            Self::AttributePath(index, lines) => {
                let loop_body = array(&array(node, "body")[*index], "body");
                let access_try = &loop_body[0];
                let call_try = &array(&loop_body[1], "body")[0];
                let mut instance = frame.lookup(py, "instance")?;
                let attrs = frame.lookup(py, "attrs")?;
                for attr in attrs.bind(py).try_iter()? {
                    let attr = attr?;
                    frame.locals.bind(py).set_item("attr", &attr)?;
                    crate::warnings::mark(lines[0]);
                    let access = (|| {
                        let isinstance = frame.lookup(py, "isinstance")?;
                        let mapping = frame.lookup(py, "Mapping")?;
                        if isinstance
                            .bind(py)
                            .call1((instance.bind(py), mapping.bind(py)))?
                            .is_truthy()?
                        {
                            instance.bind(py).get_item(&attr)
                        } else {
                            crate::warnings::mark(lines[1]);
                            frame
                                .lookup(py, "getattr")?
                                .bind(py)
                                .call1((instance.bind(py), &attr))
                        }
                    })();
                    instance = match access {
                        Ok(value) => value.unbind(),
                        Err(error) => {
                            return returned(py, finish_try(py, frame, access_try, Err(error))?);
                        }
                    };
                    frame
                        .locals
                        .bind(py)
                        .set_item("instance", instance.bind(py))?;
                    crate::warnings::mark(lines[2]);
                    if frame
                        .lookup(py, "is_simple_callable")?
                        .bind(py)
                        .call1((instance.bind(py),))?
                        .is_truthy()?
                    {
                        instance = match instance.bind(py).call0() {
                            Ok(value) => value.unbind(),
                            Err(error) => {
                                return returned(py, finish_try(py, frame, call_try, Err(error))?);
                            }
                        };
                        frame
                            .locals
                            .bind(py)
                            .set_item("instance", instance.bind(py))?;
                    }
                }
                Ok(instance)
            }
            Self::Scalar(name, line) => {
                crate::warnings::mark(*line);
                let callable = frame.lookup(py, name)?;
                Ok(callable
                    .bind(py)
                    .call1((frame.lookup(py, "value")?.bind(py),))?
                    .unbind())
            }
            Self::SimpleCallable(after, line) => {
                crate::warnings::mark(*line);
                let callable = frame.lookup(py, "callable")?;
                if !callable
                    .bind(py)
                    .call1((frame.lookup(py, "obj")?.bind(py),))?
                    .is_truthy()?
                {
                    return Ok(false.into_pyobject(py)?.to_owned().into_any().unbind());
                }
                match block(py, frame, &array(node, "body")[*after..])? {
                    Flow::Return(value) => Ok(value),
                    _ => Ok(py.None()),
                }
            }
            Self::FieldValidation(lines) => {
                let owner = frame.lookup(py, "self")?;
                let data = frame.lookup(py, "data")?;
                crate::warnings::mark(lines[0]);
                let validated = owner
                    .bind(py)
                    .call_method1("validate_empty_values", (data.bind(py),))?;
                let values = unpack(py, &validated, 2)?;
                if values[0].bind(py).is_truthy()? {
                    return Ok(values[1].clone_ref(py));
                }
                crate::warnings::mark(lines[1]);
                let value = owner
                    .bind(py)
                    .call_method1("to_internal_value", (values[1].bind(py),))?;
                crate::warnings::mark(lines[2]);
                owner.bind(py).call_method1("run_validators", (&value,))?;
                Ok(value.unbind())
            }
            Self::SerializerRepresentation(lines) => {
                let owner = frame.lookup(py, "self")?;
                let instance = frame.lookup(py, "instance")?;
                let result = PyDict::new(py);
                crate::warnings::mark(lines[0]);
                let fields = owner.bind(py).getattr("_readable_fields")?;
                for field in fields.try_iter()? {
                    let field = field?;
                    crate::warnings::mark(lines[1]);
                    let attribute = match field.call_method1("get_attribute", (instance.bind(py),))
                    {
                        Ok(value) => value,
                        Err(error)
                            if error.matches(py, frame.lookup(py, "SkipField")?.bind(py))? =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let pk_only = frame.lookup(py, "PKOnlyObject")?;
                    let isinstance = frame.lookup(py, "isinstance")?;
                    let check = if isinstance
                        .bind(py)
                        .call1((&attribute, pk_only.bind(py)))?
                        .is_truthy()?
                    {
                        attribute.getattr("pk")?
                    } else {
                        attribute.clone()
                    };
                    // Assignment evaluates the value before the destination key.
                    let value = if check.is_none() {
                        py.None().into_bound(py)
                    } else {
                        crate::warnings::mark(lines[2]);
                        field.call_method1("to_representation", (&attribute,))?
                    };
                    result.set_item(field.getattr("field_name")?, value)?;
                }
                Ok(result.into_any().unbind())
            }
            Self::ListRepresentation(lines) => {
                let owner = frame.lookup(py, "self")?;
                let data = frame.lookup(py, "data")?;
                let isinstance = frame.lookup(py, "isinstance")?;
                let manager = frame
                    .lookup(py, "models")?
                    .bind(py)
                    .getattr("manager")?
                    .getattr("BaseManager")?;
                let iterable = if isinstance
                    .bind(py)
                    .call1((data.bind(py), manager))?
                    .is_truthy()?
                {
                    crate::warnings::mark(lines[0]);
                    data.bind(py).call_method0("all")?
                } else {
                    data.bind(py).clone()
                };
                let result = PyList::empty(py);
                for item in iterable.try_iter()? {
                    let item = item?;
                    crate::warnings::mark(lines[1]);
                    // Re-read child on every iteration; callbacks can mutate it.
                    let child = owner.bind(py).getattr("child")?;
                    result.append(child.call_method1("to_representation", (item,))?)?;
                }
                Ok(result.into_any().unbind())
            }
        }
    }
}

// Keep the ordinary generator frame and protocol, replacing only the fixed
// loop's AST copies. The values view is created on first resume and retained
// across yields. Both the field and its write_only flag stay live.
pub(super) struct ReadableFields {
    pub(super) iterator: Option<Py<pyo3::types::PyIterator>>,
    lines: [u32; 3],
}
impl ReadableFields {
    pub(super) fn new(lines: [u32; 3]) -> Self {
        Self {
            iterator: None,
            lines,
        }
    }

    pub(super) fn resume(&mut self, py: Python<'_>, frame: &mut Frame) -> PyResult<Option<Object>> {
        if self.iterator.is_none() {
            crate::warnings::mark(self.lines[0]);
            self.iterator = Some(
                frame
                    .lookup(py, "self")?
                    .bind(py)
                    .getattr("fields")?
                    .call_method0("values")?
                    .try_iter()?
                    .unbind(),
            );
        }
        loop {
            crate::warnings::mark(self.lines[0]);
            let Some(field) = self.iterator.as_ref().unwrap().bind(py).clone().next() else {
                return Ok(None);
            };
            let field = field?;
            frame.locals.bind(py).set_item("field", &field)?;
            crate::warnings::mark(self.lines[1]);
            if !field.getattr("write_only")?.is_truthy()? {
                crate::warnings::mark(self.lines[2]);
                return Ok(Some(field.unbind()));
            }
        }
    }
}

fn returned(py: Python<'_>, flow: Flow) -> PyResult<Object> {
    match flow {
        Flow::Return(value) => Ok(value),
        _ => Ok(py.None()),
    }
}

pub(super) fn unpack(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    count: usize,
) -> PyResult<Vec<Object>> {
    let mut iterator = value.try_iter().map_err(|error| {
        // CPython distinguishes failed unpacking from a direct iter() call.
        // Keep exceptions raised by user __iter__ / __getitem__ hooks intact.
        let lacks_iterator = unsafe {
            pyo3::ffi::PyType_GetSlot(value.get_type().as_type_ptr(), pyo3::ffi::Py_tp_iter)
                .is_null()
                && pyo3::ffi::PySequence_Check(value.as_ptr()) == 0
        };
        if lacks_iterator && error.is_instance_of::<pyo3::exceptions::PyTypeError>(py) {
            pyo3::exceptions::PyTypeError::new_err(format!(
                "cannot unpack non-iterable {} object",
                value
                    .get_type()
                    .name()
                    .map_or_else(|_| "object".into(), |name| name.to_string())
            ))
        } else {
            error
        }
    })?;
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let Some(item) = iterator.next() else {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "not enough values to unpack (expected {count}, got {index})"
            )));
        };
        values.push(item?.unbind());
    }
    if let Some(item) = iterator.next() {
        item?;
        // CPython 3.14 adds the actual size for exact built-in containers.
        // Iterator and subclass errors still use the older wording.
        let version = unsafe { std::ffi::CStr::from_ptr(pyo3::ffi::Py_GetVersion()) }.to_bytes();
        let modern = version.starts_with(b"3.14.") || version.starts_with(b"3.15.");
        let exact_container = value.get_type().is(py.get_type::<PyTuple>())
            || value.get_type().is(py.get_type::<PyList>())
            || value.get_type().is(py.get_type::<PyDict>());
        if modern && exact_container {
            let length = value.len()?;
            if length > count {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "too many values to unpack (expected {count}, got {length})"
                )));
            }
        }
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "too many values to unpack (expected {count})"
        )));
    }
    Ok(values)
}
