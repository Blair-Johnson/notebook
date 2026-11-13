//! `TextBuffer::line` and `line_count` stay right through every kind of edit.

use nb_core::{CursorPos, TextBuffer};

fn assert_lines(buffer: &TextBuffer, step: &str) {
    let expected: Vec<&str> = buffer.as_str().split('\n').collect();
    assert_eq!(buffer.line_count(), expected.len(), "{step}");
    for (row, line) in expected.iter().enumerate() {
        assert_eq!(buffer.line(row), Some(*line), "{step} row {row}");
    }
    assert_eq!(buffer.line(expected.len()), None, "{step}");
}

#[test]
fn line_lookups_follow_edits() {
    let mut buffer = TextBuffer::new("");
    assert_lines(&buffer, "empty");
    buffer.insert_str("alpha\nbeta\n\ngamma é漢");
    assert_lines(&buffer, "insert");
    buffer.set_cursor(CursorPos { row: 1, col: 2 });
    buffer.checkpoint();
    buffer.insert_newline();
    assert_lines(&buffer, "newline");
    buffer.delete_char_backward();
    assert_lines(&buffer, "backspace");
    buffer.delete_char_forward();
    assert_lines(&buffer, "delete");
    buffer.replace_char('\n');
    assert_lines(&buffer, "replace char");
    buffer.delete_line_range(0..1);
    assert_lines(&buffer, "delete lines");
    buffer.delete_span(CursorPos { row: 0, col: 0 }, CursorPos { row: 1, col: 1 });
    assert_lines(&buffer, "delete span");
    buffer.rollback_chars(1);
    assert_lines(&buffer, "rollback");
    buffer.undo();
    assert_lines(&buffer, "undo");
    buffer.redo();
    assert_lines(&buffer, "redo");
    buffer.replace_all("x\ny\n");
    assert_lines(&buffer, "replace all");
    let copy = buffer.clone();
    assert_lines(&copy, "clone");
}
