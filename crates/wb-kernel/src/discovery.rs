//! Local Python kernelspec discovery (`$VIRTUAL_ENV`, `$CONDA_PREFIX` / Pixi, `.venv`, global).

use crate::client::KernelError;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct KernelLaunchSpec {
    pub requested_name: String,
    pub working_dir: PathBuf,
    pub existing_connection_file: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ResolvedKernelspec {
    pub kernel_name: String,
    pub display_name: String,
    pub language: String,
    pub spec_dir: PathBuf,
    pub argv: Vec<String>,
    pub interrupt_via_message: bool,
}

/// Resolve a Python kernelspec prioritizing active virtual/Pixi environments
/// (`$VIRTUAL_ENV`, `$CONDA_PREFIX`), then `.venv`/`venv` next to `working_dir`,
/// then standard Jupyter paths, rewriting relative `python` `argv[0]` entries.
pub async fn resolve_kernelspec(
    _requested_name: &str,
    _working_dir: &Path,
) -> Result<ResolvedKernelspec, KernelError> {
    todo!("wb-kernel::discovery: probe VIRTUAL_ENV, CONDA_PREFIX, local .venv, and jupyter paths")
}
