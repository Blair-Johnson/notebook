//! `OutputItem::line_count` agrees with counting the lines of `plain_text`.

use nb_core::{MimeBundle, OutputData, OutputItem, StreamBuffer, StreamKind};
use serde_json::{Map, Value, json};

fn stream(text: &str) -> OutputItem {
    let mut buffer = StreamBuffer::default();
    buffer.push_chunk(text);
    OutputItem::new(
        OutputData::Stream {
            stream: StreamKind::Stdout,
            buffer,
        },
        false,
    )
}

fn rich(plain: Option<Value>) -> OutputItem {
    let mut bundle = MimeBundle::new();
    if let Some(plain) = plain {
        bundle.insert("text/plain".into(), plain);
    }
    bundle.insert("image/png".into(), json!("iVBOR"));
    OutputItem::new(
        OutputData::Rich {
            bundle,
            metadata: Map::new(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
        false,
    )
}

fn error(traceback: &[&str]) -> OutputItem {
    OutputItem::new(
        OutputData::Error {
            ename: "ValueError".into(),
            evalue: "bad\nvalue".into(),
            traceback: traceback.iter().map(|line| line.to_string()).collect(),
        },
        false,
    )
}

#[test]
fn line_count_matches_plain_text() {
    let items = [
        stream(""),
        stream("one"),
        stream("one\ntwo\n\n\n"),
        stream("\n\n"),
        stream("50%\r100%\ndone"),
        rich(Some(json!("a\nb"))),
        rich(Some(json!(["a\n", "b\n", "\n"]))),
        rich(Some(json!(["\n", "\n"]))),
        rich(None),
        error(&[]),
        error(&[
            "Traceback:",
            "  File \"x\", line 1\n    boom()",
            "ValueError: bad\n",
        ]),
        error(&["\n", "\n"]),
    ];
    for item in &items {
        assert_eq!(
            item.line_count(),
            item.plain_text().lines().count(),
            "{:?}",
            item.plain_text()
        );
    }
}
