//! Preserve warning locations while native frames are absent from Python's stack.
use std::cell::RefCell;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

#[derive(Clone)]
struct Site {
    module: String,
    line: u32,
    caller: usize,
}
thread_local! { static SITES: RefCell<Vec<Site>> = const { RefCell::new(Vec::new()) }; }

pub struct Context;
impl Context {
    pub fn enter(py: Python<'_>, globals: &Bound<'_, PyDict>, line: u32) -> PyResult<Self> {
        let module = globals.get_item("__name__")?.unwrap().extract()?;
        // Stable ABI; the pointer is used only as an identity while its caller
        // is on the stack. It is never dereferenced or retained past this call.
        let caller = unsafe { pyo3::ffi::PyEval_GetFrame() } as usize;
        SITES.with(|sites| {
            sites.borrow_mut().push(Site {
                module,
                line,
                caller,
            })
        });
        let _ = py;
        Ok(Self)
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        SITES.with(|sites| {
            sites.borrow_mut().pop();
        });
    }
}
pub fn mark(line: u32) {
    SITES.with(|sites| {
        if let Some(site) = sites.borrow_mut().last_mut() {
            site.line = line;
        }
    });
}

pub fn warn(py: Python<'_>, args: &[Py<PyAny>], kwargs: &Bound<'_, PyDict>) -> PyResult<Py<PyAny>> {
    let sites = SITES.with(|sites| sites.borrow().clone());
    let mut level: i64 = if args.len() > 2 {
        args[2].bind(py).extract()?
    } else {
        kwargs
            .get_item("stacklevel")?
            .map(|v| v.extract())
            .transpose()?
            .unwrap_or(1)
    };
    level = level.max(1);
    let mut frame = py.import("sys")?.getattr("_getframe")?.call0()?;
    let mut pending = sites.iter().rev().peekable();
    let mut location = None;
    while !frame.is_none() {
        while pending
            .peek()
            .is_some_and(|site| site.caller == frame.as_ptr() as usize)
        {
            let site = pending.next().unwrap();
            if level == 1 {
                let parts: Vec<_> = site.module.split('.').collect();
                let root = py
                    .import("rest_framework")?
                    .getattr("__path__")?
                    .get_item(0)?;
                let tail = parts[1..].join(std::path::MAIN_SEPARATOR_STR) + ".py";
                let filename = py.import("os.path")?.getattr("join")?.call1((root, tail))?;
                location = Some((filename.unbind(), site.line, site.module.clone()));
                break;
            }
            level -= 1;
        }
        if location.is_some() {
            break;
        }
        if level == 1 {
            let filename = frame.getattr("f_code")?.getattr("co_filename")?;
            let line: u32 = frame.getattr("f_lineno")?.extract()?;
            let module: String = frame
                .getattr("f_globals")?
                .get_item("__name__")?
                .extract()?;
            location = Some((filename.unbind(), line, module));
            break;
        }
        level -= 1;
        frame = frame.getattr("f_back")?;
    }
    let warnings = py.import("warnings")?;
    let Some((filename, line, module)) = location else {
        return Ok(warnings
            .getattr("warn")?
            .call(
                PyTuple::new(py, args.iter().map(|v| v.bind(py)))?,
                Some(kwargs),
            )?
            .unbind());
    };
    let message = if let Some(message) = args.first() {
        message.bind(py).clone()
    } else {
        kwargs
            .get_item("message")?
            .ok_or_else(|| pyo3::exceptions::PyTypeError::new_err("missing warning message"))?
    };
    let category = if args.len() > 1 {
        args[1].bind(py).clone()
    } else {
        kwargs
            .get_item("category")?
            .unwrap_or_else(|| py.get_type::<pyo3::exceptions::PyUserWarning>().into_any())
    };
    let explicit = PyDict::new(py);
    explicit.set_item("module", &module)?;
    if let Ok(owner) = py.import("sys")?.getattr("modules")?.get_item(&module) {
        let registry = match owner.getattr("__warningregistry__") {
            Ok(registry) => registry,
            Err(_) => {
                let registry = PyDict::new(py);
                owner.setattr("__warningregistry__", &registry)?;
                registry.into_any()
            }
        };
        explicit.set_item("registry", registry)?;
    }
    if let Some(source) = kwargs.get_item("source")? {
        explicit.set_item("source", source)?;
    }
    Ok(warnings
        .getattr("warn_explicit")?
        .call(
            (message, category, filename.bind(py), line),
            Some(&explicit),
        )?
        .unbind())
}
