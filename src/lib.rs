//! Native DRF engine and isolated Stable ABI object-model regression experiments.

mod abi;
mod activation;
mod feasibility;
mod loader;
mod method;
mod runtime;
mod warnings;

use pyo3::prelude::*;

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let experiment = PyModule::new(module.py(), "rustializer._feasibility")?;
    feasibility::register(&experiment)?;
    module
        .py()
        .import("sys")?
        .getattr("modules")?
        .set_item("rustializer._feasibility", &experiment)?;
    module.add("_feasibility", experiment)?;
    activation::register(module)
}
