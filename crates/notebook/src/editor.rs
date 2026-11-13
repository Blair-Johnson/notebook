//! External editor command construction for cell handoff.

use std::path::Path;
use std::process::Command;

/// Build a shell command that treats the temporary file as a positional
/// argument, so editors containing multiple words (for example, `emacs -nw
/// -Q`) work without shell re-parsing the path.
pub fn editor_command(editor: &str, file: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg("nb")
        .arg(file);
    command
}

#[cfg(test)]
mod tests {
    use super::editor_command;

    #[test]
    fn command_keeps_the_file_as_a_separate_argument() {
        let path = std::env::temp_dir().join(format!(
            "nb-editor-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        std::fs::write(&path, "source").expect("write test file");
        let status = editor_command("cp /dev/null", &path)
            .status()
            .expect("run test editor");
        let source = std::fs::read_to_string(&path).expect("read test file");
        let _ = std::fs::remove_file(&path);
        assert!(status.success());
        assert!(source.is_empty());
    }
}
