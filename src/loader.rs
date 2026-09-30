//! Native module loading also keeps reloads and cache eviction native.
use pyo3::prelude::*;
use pyo3::types::PyModule;
use serde_json::Value;
use std::sync::Arc;

#[pyclass(module = "rustializer._native")]
struct EngineLoader {
    definition: Arc<Value>,
}
#[pymethods]
impl EngineLoader {
    fn create_module(&self, py: Python<'_>, _spec: &Bound<'_, PyAny>) -> Py<PyAny> {
        py.None()
    }
    fn exec_module(&self, module: &Bound<'_, PyModule>) -> PyResult<()> {
        crate::runtime::execute(module.py(), module, &self.definition)
    }
    fn get_source(&self, py: Python<'_>, _fullname: &str) -> Py<PyAny> {
        py.None()
    }
    fn get_code(&self, py: Python<'_>, _fullname: &str) -> Py<PyAny> {
        py.None()
    }
    fn get_filename(&self, py: Python<'_>, _fullname: &str) -> PyResult<Py<PyAny>> {
        filename(py, self.definition["name"].as_str().unwrap())
    }
    fn is_package(&self, _fullname: &str) -> bool {
        false
    }
}
#[pyclass(module = "rustializer._native")]
struct EngineFinder {
    definitions: Vec<Arc<Value>>,
}
#[pymethods]
impl EngineFinder {
    #[pyo3(signature=(fullname,path=None,target=None))]
    fn find_spec(
        &self,
        py: Python<'_>,
        fullname: &str,
        path: Option<&Bound<'_, PyAny>>,
        target: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let _ = (path, target);
        let definition = self
            .definitions
            .iter()
            .find(|n| n["name"].as_str() == Some(fullname));
        match definition {
            Some(definition) => spec(py, definition.clone()),
            None => Ok(py.None()),
        }
    }
}
fn filename(py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let root = py
        .import("rest_framework")?
        .getattr("__path__")?
        .get_item(0)?;
    let tail = name
        .strip_prefix("rest_framework.")
        .unwrap()
        .replace('.', std::path::MAIN_SEPARATOR_STR)
        + ".py";
    Ok(py
        .import("os.path")?
        .getattr("join")?
        .call1((root, tail))?
        .unbind())
}
fn spec(py: Python<'_>, definition: Arc<Value>) -> PyResult<Py<PyAny>> {
    let name = definition["name"].as_str().unwrap().to_owned();
    let loader = Py::new(py, EngineLoader { definition })?;
    let spec = py
        .import("importlib.machinery")?
        .getattr("ModuleSpec")?
        .call1((&name, loader))?;
    spec.setattr("origin", filename(py, &name)?)?;
    spec.setattr("has_location", true)?;
    Ok(spec.unbind())
}
pub fn metadata(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    definition: Arc<Value>,
) -> PyResult<()> {
    let spec = spec(py, definition)?;
    module.setattr("__file__", spec.bind(py).getattr("origin")?)?;
    module.setattr("__loader__", spec.bind(py).getattr("loader")?)?;
    module.setattr("__spec__", spec)?;
    Ok(())
}
pub fn install(py: Python<'_>, definitions: Vec<Arc<Value>>) -> PyResult<()> {
    let finder = Py::new(py, EngineFinder { definitions })?;
    py.import("sys")?
        .getattr("meta_path")?
        .call_method1("insert", (0, finder))?;
    // Keep the loader types importable for ordinary type inspection.
    let module = py.import("rustializer._native")?;
    module.add("EngineLoader", py.get_type::<EngineLoader>())?;
    module.add("EngineFinder", py.get_type::<EngineFinder>())?;
    Ok(())
}
