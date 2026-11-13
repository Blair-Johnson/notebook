//! Jupytext `# %%` percent-format and plain `.py` script reader/writer.

use crate::codec::CoreError;
use crate::document::Document;

/// Parse a `# %%` percent-formatted `.py` file (or plain Python script) into cells,
/// uncommenting IPython magics and preserving any leading `# ---` YAML header.
pub fn parse_percent(_source: &str) -> Result<Document, CoreError> {
    todo!("wb-core::codec::percent: parse YAML header, `# %% [markdown]` / `# %%` blocks, magic comments")
}

/// Serialize `Document` cells back to clean `# %%` percent format (sources only, no outputs).
pub fn serialize_percent(_doc: &Document) -> String {
    todo!("wb-core::codec::percent: emit header + `# %%` cell markers + commented IPython magics")
}
