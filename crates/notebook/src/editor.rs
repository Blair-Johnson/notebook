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

/// Like [`editor_command`], opening at a 1-based `line` in editors known to
/// take `+N` (vi, vim, nvim, nano, emacs, kak, micro).
pub fn editor_command_at(editor: &str, file: &Path, line: usize) -> Command {
    let program = editor
        .split_whitespace()
        .next()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !matches!(
        program,
        "vi" | "vim" | "nvim" | "nano" | "emacs" | "emacsclient" | "kak" | "micro"
    ) {
        return editor_command(editor, file);
    }
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg("nb")
        .arg(format!("+{line}"))
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
