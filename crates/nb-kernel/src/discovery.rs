//! Local Python kernelspec discovery (`$VIRTUAL_ENV`, `$CONDA_PREFIX` / Pixi, `.venv`, global).

use crate::client::KernelError;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bound on `jupyter --paths --json`, which can hang behind a slow shim
/// (pyenv/asdf rehash, conda activation) and would otherwise stall startup.
const JUPYTER_PATHS_TIMEOUT: Duration = Duration::from_secs(5);

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
    requested_name: &str,
    working_dir: &Path,
) -> Result<ResolvedKernelspec, KernelError> {
    let virtual_env = env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    let conda_prefix = env::var_os("CONDA_PREFIX").map(PathBuf::from);
    resolve_kernelspec_with_roots(requested_name, working_dir, virtual_env, conda_prefix).await
}

async fn resolve_kernelspec_with_roots(
    requested_name: &str,
    working_dir: &Path,
    virtual_env: Option<PathBuf>,
    conda_prefix: Option<PathBuf>,
) -> Result<ResolvedKernelspec, KernelError> {
    let candidates = candidate_names(requested_name);
    let local_roots = [
        virtual_env,
        conda_prefix,
        Some(working_dir.join(".venv")),
        Some(working_dir.join("venv")),
    ];

    // Asked lazily and once: it spawns a Python process.
    let mut jupyter_dirs = None;
    for name in &candidates {
        for env_root in local_roots.iter().flatten() {
            let data_dir = env_root.join("share/jupyter");
            let specs = jupyter_zmq_client::read_kernelspec_jsons(&data_dir).await;
            if let Some(spec) = specs.into_iter().find(|spec| &spec.kernel_name == name) {
                return Ok(convert_spec(spec, Some(env_root)));
            }
        }

        let data_dirs = match &jupyter_dirs {
            Some(dirs) => dirs,
            None => jupyter_dirs.insert(jupyter_data_dirs().await),
        };
        for data_dir in data_dirs {
            let specs = jupyter_zmq_client::read_kernelspec_jsons(data_dir).await;
            if let Some(spec) = specs.into_iter().find(|spec| &spec.kernel_name == name) {
                return Ok(convert_spec(spec, None));
            }
        }
    }

    Err(KernelError::Kernelspec(format!(
        "no Jupyter kernel found (tried {candidates:?}); install ipykernel in this \
         environment, e.g. `pixi add ipykernel`"
    )))
}

/// Standard Jupyter data directories plus those `jupyter --paths` reports, or
/// just the standard ones when `jupyter` is missing or does not answer in time.
async fn jupyter_data_dirs() -> Vec<PathBuf> {
    tokio::time::timeout(
        JUPYTER_PATHS_TIMEOUT,
        jupyter_zmq_client::data_dirs_with_jupyter_paths(),
    )
    .await
    .unwrap_or_else(|_| jupyter_zmq_client::data_dirs())
}

fn candidate_names(requested_name: &str) -> Vec<String> {
    let mut candidates = vec![requested_name.to_owned()];
    if requested_name != "python3" {
        candidates.push("python3".to_owned());
    }
    candidates
}

fn convert_spec(
    mut spec: jupyter_zmq_client::KernelspecDir,
    environment_root: Option<&Path>,
) -> ResolvedKernelspec {
    if let Some(root) = environment_root
        && let Some(argv0) = spec.kernelspec.argv.first_mut()
        && is_relative_python(argv0)
    {
        *argv0 = root.join("bin/python").to_string_lossy().into_owned();
    }

    ResolvedKernelspec {
        kernel_name: spec.kernel_name,
        display_name: spec.kernelspec.display_name,
        language: spec.kernelspec.language,
        spec_dir: spec.path,
        argv: spec.kernelspec.argv,
        interrupt_via_message: spec.kernelspec.interrupt_mode.as_deref() == Some("message"),
    }
}

fn is_relative_python(argv0: &str) -> bool {
    let path = Path::new(argv0);
    path.is_relative()
        && path.components().count() == 1
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("python"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nb-kernel-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp directory");
        path
    }

    fn write_spec(root: &Path, name: &str, argv0: &str) {
        let dir = root.join("share/jupyter/kernels").join(name);
        fs::create_dir_all(&dir).expect("create kernelspec directory");
        let json = serde_json::json!({
            "argv": [argv0, "-m", "ipykernel_launcher", "-f", "{connection_file}"],
            "display_name": "Test Python",
            "language": "python",
            "interrupt_mode": "message"
        });
        fs::write(
            dir.join("kernel.json"),
            serde_json::to_vec(&json).expect("serialize kernelspec"),
        )
        .expect("write kernelspec");
    }

    #[tokio::test]
    async fn active_environment_roots_precede_project_venv() {
        let active = temp_dir("active");
        let project = temp_dir("project");
        write_spec(&active, "python3", "python3.12");
        write_spec(&project.join(".venv"), "python3", "python");

        let resolved =
            resolve_kernelspec_with_roots("missing-name", &project, Some(active.clone()), None)
                .await
                .expect("active environment kernelspec");

        assert_eq!(resolved.kernel_name, "python3");
        assert_eq!(
            resolved.spec_dir,
            active.join("share/jupyter/kernels/python3")
        );
        assert_eq!(
            resolved.argv[0],
            active.join("bin/python").to_string_lossy()
        );
        assert!(resolved.interrupt_via_message);

        let _ = fs::remove_dir_all(active);
        let _ = fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn local_venv_is_used_when_active_roots_are_absent() {
        let project = temp_dir("local");
        write_spec(&project.join("venv"), "python3", "/usr/bin/python3");

        let resolved = resolve_kernelspec_with_roots("python3", &project, None, None)
            .await
            .expect("local kernelspec");

        assert_eq!(
            resolved.spec_dir,
            project.join("venv/share/jupyter/kernels/python3")
        );
        assert_eq!(resolved.argv[0], "/usr/bin/python3");

        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn candidate_names_falls_back_to_python3_without_duplicates() {
        assert_eq!(candidate_names("custom"), ["custom", "python3"]);
        assert_eq!(candidate_names("python3"), ["python3"]);
    }

    #[test]
    fn only_relative_python_argv_entries_are_rewritten() {
        assert!(is_relative_python("python"));
        assert!(is_relative_python("python3.12"));
        assert!(!is_relative_python("./python"));
        assert!(!is_relative_python("/usr/bin/python3"));
        assert!(!is_relative_python("bash"));
    }
}
