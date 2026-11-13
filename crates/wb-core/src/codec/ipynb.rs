//! Lossless nbformat 4.5 reader/writer and autosave sidecar serializer.

use crate::cell::{Cell, CellKind};
use crate::codec::CoreError;
use crate::document::{Document, FormatBacking};
use crate::id::{CellId, OutputId};
use crate::output::{MimeBundle, OutputData, OutputItem, StreamBuffer, StreamKind};
use serde_json::{Map, Value};

/// Parse an nbformat 4.x JSON string, upgrading pre-4.5 cells with fresh IDs.
pub fn parse_ipynb(json: &str) -> Result<Document, CoreError> {
    let value: Value = serde_json::from_str(json)?;
    let Some(mut root) = value.as_object().cloned() else {
        return Err(CoreError::Percent(
            "notebook JSON root must be an object".into(),
        ));
    };
    let cells = root
        .remove("cells")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    root.remove("nbformat");
    root.remove("nbformat_minor");

    let percent_header = root
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .and_then(|metadata| metadata.remove("workbench_percent_header"))
        .and_then(|value| value.as_str().map(str::to_owned));

    let mut typed_cells = Vec::with_capacity(cells.len());
    for value in cells {
        let Some(object) = value.as_object() else {
            return Err(CoreError::Percent("notebook cell must be an object".into()));
        };
        let kind = match object
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("raw")
        {
            "code" => CellKind::Code,
            "markdown" => CellKind::Markdown,
            _ => CellKind::Raw,
        };
        let source = object
            .get("source")
            .map(join_multiline)
            .transpose()?
            .unwrap_or_default();
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .map(|id| CellId(id.to_owned()))
            .unwrap_or_else(CellId::new_random);
        let execution_count = object.get("execution_count").and_then(Value::as_i64);
        let outputs = object
            .get("outputs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(parse_output)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let mut extra = object.clone();
        for key in ["cell_type", "source", "id", "execution_count", "outputs"] {
            extra.remove(key);
        }
        let mut cell = Cell::new(kind, source);
        cell.id = id;
        cell.execution_count = execution_count;
        cell.outputs = outputs;
        cell.extra = extra;
        typed_cells.push(cell);
    }

    let backing = if let Some(header) = percent_header {
        FormatBacking::PercentPy {
            header: Some(header),
        }
    } else {
        FormatBacking::Ipynb { extra: root }
    };
    Ok(Document::from_cells(typed_cells, backing))
}

/// Serialize `Document` as canonical 1-space-indented, sorted-key nbformat 4.5 JSON.
///
/// When `is_autosave_sidecar` is true, `PercentPy` header info is embedded in
/// notebook metadata so crash recovery of a `.py` session restores both header and outputs.
pub fn serialize_ipynb(doc: &Document, is_autosave_sidecar: bool) -> Result<String, CoreError> {
    let mut root = match &doc.backing {
        FormatBacking::Ipynb { extra } => extra.clone(),
        FormatBacking::PercentPy { .. } => Map::new(),
    };
    let mut metadata = root
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.remove("workbench_percent_header");
    if is_autosave_sidecar
        && let FormatBacking::PercentPy {
            header: Some(header),
        } = &doc.backing
    {
        metadata.insert(
            "workbench_percent_header".into(),
            Value::String(header.clone()),
        );
    }

    let cells = doc.cells_in_order().map(serialize_cell).collect::<Vec<_>>();
    root.insert("cells".into(), Value::Array(cells));
    root.insert("metadata".into(), Value::Object(metadata));
    root.insert("nbformat".into(), Value::from(4));
    root.insert("nbformat_minor".into(), Value::from(5));

    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    serde::Serialize::serialize(&Value::Object(root), &mut serializer)?;
    bytes.push(b'\n');
    Ok(String::from_utf8(bytes).expect("serde_json always emits UTF-8"))
}

fn serialize_cell(cell: &Cell) -> Value {
    let mut object = cell.extra.clone();
    for key in ["cell_type", "source", "id", "execution_count", "outputs"] {
        object.remove(key);
    }
    object.insert(
        "cell_type".into(),
        Value::String(cell_kind_name(cell.kind).into()),
    );
    object.insert("id".into(), Value::String(cell.id.as_str().into()));
    object.insert("metadata".into(), metadata_value(&cell.extra));
    object.insert("source".into(), multiline_value(cell.buffer.as_str()));
    if cell.kind == CellKind::Code {
        object.insert(
            "execution_count".into(),
            cell.execution_count.map_or(Value::Null, Value::from),
        );
        object.insert(
            "outputs".into(),
            Value::Array(cell.outputs.iter().map(serialize_output).collect()),
        );
    } else {
        object.remove("outputs");
        object.remove("execution_count");
    }
    Value::Object(object)
}

fn serialize_output(item: &OutputItem) -> Value {
    let mut object = item.extra.clone();
    for key in [
        "output_type",
        "name",
        "text",
        "data",
        "metadata",
        "execution_count",
        "transient",
        "transient_late",
        "id",
        "rev",
    ] {
        object.remove(key);
    }
    match &item.data {
        OutputData::Stream { stream, buffer } => {
            object.insert("name".into(), Value::String(stream_name(*stream).into()));
            object.insert("output_type".into(), Value::String("stream".into()));
            object.insert("text".into(), multiline_value(buffer.as_str()));
        }
        OutputData::Rich {
            bundle,
            metadata,
            display_id: _,
            is_execute_result,
            execution_count,
        } => {
            let preserved_output_type = item
                .extra
                .get("output_type")
                .and_then(Value::as_str)
                .unwrap_or("display_data")
                .to_owned();
            let output_type = if *is_execute_result {
                "execute_result".to_owned()
            } else {
                preserved_output_type
            };
            object.insert(
                "data".into(),
                Value::Object(
                    bundle
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
            );
            object.insert("metadata".into(), Value::Object(metadata.clone()));
            object.insert("output_type".into(), Value::String(output_type));
            if *is_execute_result {
                object.insert(
                    "execution_count".into(),
                    execution_count.map_or(Value::Null, Value::from),
                );
            }
        }
        OutputData::Error {
            ename,
            evalue,
            traceback,
        } => {
            object.insert("ename".into(), Value::String(ename.clone()));
            object.insert("evalue".into(), Value::String(evalue.clone()));
            object.insert("output_type".into(), Value::String("error".into()));
            object.insert(
                "traceback".into(),
                Value::Array(traceback.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    Value::Object(object)
}

fn parse_output(value: &Value) -> Result<OutputItem, CoreError> {
    let Some(object) = value.as_object() else {
        return Err(CoreError::Percent(
            "notebook output must be an object".into(),
        ));
    };
    let output_type = object
        .get("output_type")
        .and_then(Value::as_str)
        .unwrap_or("display_data");
    let transient_late = object
        .get("transient_late")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let data = match output_type {
        "stream" => {
            let stream = if object.get("name").and_then(Value::as_str) == Some("stderr") {
                StreamKind::Stderr
            } else {
                StreamKind::Stdout
            };
            let mut buffer = StreamBuffer::default();
            if let Some(text) = object.get("text") {
                buffer.push_chunk(&join_multiline(text)?);
            }
            OutputData::Stream { stream, buffer }
        }
        "error" => OutputData::Error {
            ename: object
                .get("ename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            evalue: object
                .get("evalue")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            traceback: object
                .get("traceback")
                .map(traceback_lines)
                .transpose()?
                .unwrap_or_default(),
        },
        _ => {
            let bundle = object
                .get("data")
                .and_then(Value::as_object)
                .map(|data| {
                    data.iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<MimeBundle>()
                })
                .unwrap_or_default();
            let metadata = object
                .get("metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let display_id = object
                .get("transient")
                .and_then(Value::as_object)
                .and_then(|transient| transient.get("display_id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            OutputData::Rich {
                bundle,
                metadata,
                display_id,
                is_execute_result: output_type == "execute_result",
                execution_count: object.get("execution_count").and_then(Value::as_i64),
            }
        }
    };
    let mut extra = object.clone();
    match output_type {
        "stream" => {
            for key in ["output_type", "name", "text", "transient_late"] {
                extra.remove(key);
            }
        }
        "error" => {
            for key in [
                "output_type",
                "ename",
                "evalue",
                "traceback",
                "transient_late",
            ] {
                extra.remove(key);
            }
        }
        _ => {
            for key in [
                "data",
                "metadata",
                "execution_count",
                "transient",
                "transient_late",
            ] {
                extra.remove(key);
            }
            if matches!(output_type, "display_data" | "execute_result") {
                extra.remove("output_type");
            }
        }
    }
    if output_type == "stream" || output_type == "error" {
        extra.remove("transient");
    }
    if !matches!(
        output_type,
        "stream" | "error" | "display_data" | "execute_result"
    ) {
        extra.insert("output_type".into(), Value::String(output_type.into()));
    }
    Ok(OutputItem {
        id: OutputId::next(),
        rev: 1,
        transient_late,
        data,
        extra,
    })
}

fn join_multiline(value: &Value) -> Result<String, CoreError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(lines) => {
            let mut result = String::new();
            for line in lines {
                let Some(line) = line.as_str() else {
                    return Err(CoreError::Percent(
                        "multiline field contains a non-string".into(),
                    ));
                };
                result.push_str(line);
            }
            Ok(result)
        }
        _ => Err(CoreError::Percent(
            "multiline field must be a string or array".into(),
        )),
    }
}

fn traceback_lines(value: &Value) -> Result<Vec<String>, CoreError> {
    match value {
        Value::String(line) => Ok(vec![line.clone()]),
        Value::Array(lines) => lines
            .iter()
            .map(|line| {
                line.as_str().map(str::to_owned).ok_or_else(|| {
                    CoreError::Percent("traceback array contains a non-string".into())
                })
            })
            .collect(),
        _ => Err(CoreError::Percent(
            "traceback must be a string or array".into(),
        )),
    }
}

fn multiline_value(text: &str) -> Value {
    Value::Array(
        text.split_inclusive('\n')
            .map(|line| Value::String(line.to_owned()))
            .collect(),
    )
}

fn metadata_value(extra: &Map<String, Value>) -> Value {
    extra
        .get("metadata")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn cell_kind_name(kind: CellKind) -> &'static str {
    match kind {
        CellKind::Code => "code",
        CellKind::Markdown => "markdown",
        CellKind::Raw => "raw",
    }
}

fn stream_name(stream: StreamKind) -> &'static str {
    match stream {
        StreamKind::Stdout => "stdout",
        StreamKind::Stderr => "stderr",
    }
}
