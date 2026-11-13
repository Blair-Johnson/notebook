//! Where kernel connection files go, chosen the way Jupyter chooses.
//!
//! `jupyter_zmq_client::runtime_dir` prefers `$XDG_RUNTIME_DIR/jupyter`.
//! Jupyter itself stopped doing that (jupyter_core: "we no longer use
//! XDG_RUNTIME_DIR after various problems"): on clusters the variable
//! often names `/run/user/<uid>`, which does not exist on compute nodes or
//! is cleared on logout. Like `jupyter_core.paths.jupyter_runtime_dir`,
//! this uses `$JUPYTER_RUNTIME_DIR`, else `<jupyter data dir>/runtime`.

use std::ffi::OsString;
use std::path::PathBuf;

/// The directory for kernel connection files.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_from(&|key| std::env::var_os(key), std::env::consts::OS)
}

/// `runtime_dir` with the environment and OS passed in, for tests.
fn runtime_dir_from(env: &dyn Fn(&str) -> Option<OsString>, os: &str) -> PathBuf {
    let var = |key: &str| {
        env(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = var("JUPYTER_RUNTIME_DIR") {
        return dir;
    }
    let data_dir = if let Some(dir) = var("JUPYTER_DATA_DIR") {
        dir
    } else {
        let home = var("HOME").or_else(|| var("USERPROFILE"));
        let appdata = var("APPDATA");
        match (os, home, appdata) {
            ("macos", Some(home), _) => home.join("Library").join("Jupyter"),
            ("windows", _, Some(appdata)) => appdata.join("jupyter"),
            ("windows", Some(home), None) => home.join(".jupyter").join("data"),
            (_, home, _) => match (var("XDG_DATA_HOME"), home) {
                (Some(xdg), _) => xdg.join("jupyter"),
                (None, Some(home)) => home.join(".local").join("share").join("jupyter"),
                // No home at all: somewhere writable rather than nowhere.
                (None, None) => std::env::temp_dir().join("jupyter"),
            },
        }
    };
    data_dir.join("runtime")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn dir(vars: &[(&str, &str)], os: &str) -> PathBuf {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        runtime_dir_from(&|key| vars.get(key).cloned(), os)
    }

    #[test]
    fn ignores_xdg_runtime_dir_like_jupyter() {
        let linux = [("HOME", "/home/u"), ("XDG_RUNTIME_DIR", "/run/user/1000")];
        assert_eq!(
            dir(&linux, "linux"),
            Path::new("/home/u/.local/share/jupyter/runtime")
        );
    }

    #[test]
    fn follows_jupyter_overrides_and_platforms() {
        assert_eq!(
            dir(
                &[("JUPYTER_RUNTIME_DIR", "/scratch/rt"), ("HOME", "/home/u")],
                "linux"
            ),
            Path::new("/scratch/rt")
        );
        assert_eq!(
            dir(
                &[("JUPYTER_DATA_DIR", "/data/j"), ("HOME", "/home/u")],
                "linux"
            ),
            Path::new("/data/j/runtime")
        );
        assert_eq!(
            dir(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")], "linux"),
            Path::new("/xdg/jupyter/runtime")
        );
        assert_eq!(
            dir(&[("HOME", "/Users/u")], "macos"),
            Path::new("/Users/u/Library/Jupyter/runtime")
        );
        // Empty values count as unset, as in Jupyter.
        assert_eq!(
            dir(&[("JUPYTER_RUNTIME_DIR", ""), ("HOME", "/home/u")], "linux"),
            Path::new("/home/u/.local/share/jupyter/runtime")
        );
    }
}
