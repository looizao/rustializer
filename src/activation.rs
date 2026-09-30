//! One explicit, early activation boundary with version and import-order checks.
use pyo3::exceptions::{PyImportError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::runtime;

const MODULES: [&str; 14] = [
    "rest_framework.utils.html",
    "rest_framework.utils.representation",
    "rest_framework.utils.json",
    "rest_framework.utils.formatting",
    "rest_framework.utils.humanize_datetime",
    "rest_framework.utils.timezone",
    "rest_framework.utils.serializer_helpers",
    "rest_framework.exceptions",
    "rest_framework.validators",
    "rest_framework.fields",
    "rest_framework.relations",
    "rest_framework.utils.model_meta",
    "rest_framework.utils.field_mapping",
    "rest_framework.serializers",
];

#[pyfunction]
fn activate(py: Python<'_>) -> PyResult<()> {
    let extension = py.import("rustializer._native")?;
    let state: String = extension.getattr("_activation_state")?.extract()?;
    if state == "active" {
        return Ok(());
    }
    if state != "inactive" {
        return Err(PyRuntimeError::new_err(
            "Rustializer activation is already in progress",
        ));
    }
    let modules = py
        .import("sys")?
        .getattr("modules")?
        .cast_into::<PyDict>()?;
    for name in MODULES {
        if modules.contains(name)? {
            return Err(PyRuntimeError::new_err(format!(
                "Rustializer must activate before importing DRF engine modules; {name} is already imported"
            )));
        }
    }
    let drf = py.import("rest_framework")?;
    let version: String = drf.getattr("VERSION")?.extract()?;
    let source = match version.as_str() {
        "3.17.2" => include_str!("programs/drf-3.17.2.json"),
        "3.18.1" => include_str!("programs/drf-3.18.1.json"),
        _ => {
            return Err(PyImportError::new_err(format!(
                "Rustializer rejects unsupported DRF version {version}; reference versions are 3.17.2 and 3.18.1"
            )));
        }
    };
    let program: serde_json::Value = serde_json::from_str(source)
        .map_err(|e| PyRuntimeError::new_err(format!("invalid bundled serializer program: {e}")))?;
    extension.setattr("_activation_state", "activating")?;
    if let Err(error) = runtime::install(py, &program) {
        // No successful activation is reported with partially published types.
        for name in MODULES.into_iter().rev() {
            if modules.contains(name)? {
                modules.del_item(name)?;
            }
            let (parent, attr) = name.rsplit_once('.').unwrap();
            if let Some(parent) = modules.get_item(parent)?
                && parent.hasattr(attr)?
            {
                parent.delattr(attr)?;
            }
        }
        extension.setattr("_activation_state", "inactive")?;
        return Err(error);
    }
    extension.setattr("_activation_state", "active")?;
    extension.setattr("_reference_version", version)?;
    extension.setattr("_reference_commit", program["reference"].as_str().unwrap())?;
    Ok(())
}

pub fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(activate, module)?)?;
    module.add("_activation_state", "inactive")?;
    module.add("COMPATIBILITY_CERTIFIED", false)?;
    Ok(())
}
