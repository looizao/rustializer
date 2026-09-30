//! Native object-model experiments, isolated from the eventual DRF engine.

mod abi;
mod feasibility;
mod method;

use pyo3::prelude::*;

#[pymodule]
fn _feasibility(module: &Bound<'_, PyModule>) -> PyResult<()> {
    feasibility::register(module)
}
