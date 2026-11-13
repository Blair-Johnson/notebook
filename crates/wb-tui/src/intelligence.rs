//! Modular code-intelligence controllers: completion popup, signature help, and inspect pager.

use std::collections::HashSet;

use wb_kernel::CompletionCandidate;

#[derive(Clone, Debug)]
pub struct CompletionPopup {
    pub req_id: u64,
    pub replace_start_char: usize,
    pub replace_end_char: usize,
    pub all_candidates: Vec<CompletionCandidate>,
    pub filtered: Vec<CompletionCandidate>,
    pub selected: usize,
    pub scroll_top: usize,
}

impl CompletionPopup {
    /// Create a popup and apply the prefix that was already typed at request time.
    pub fn new<S: AsRef<str>>(
        req_id: u64,
        replace_start_char: usize,
        replace_end_char: usize,
        candidates: Vec<CompletionCandidate>,
        typed_prefix: S,
    ) -> Self {
        let typed_prefix = typed_prefix.as_ref();
        let filtered = candidates
            .iter()
            .filter(|candidate| candidate_matches(candidate, typed_prefix))
            .cloned()
            .collect();
        Self {
            req_id,
            replace_start_char,
            replace_end_char,
            all_candidates: candidates,
            filtered,
            selected: 0,
            scroll_top: 0,
        }
    }

    /// Re-filter the candidates using the source text currently under the cursor.
    ///
    /// `replace_start_char` and `cursor_char_pos` are Unicode-scalar offsets, not
    /// byte offsets.  A completion popup is single-line by design, so moving
    /// before its start or across a newline invalidates it.
    pub fn update_filter(&mut self, source: &str, cursor_char_pos: usize) -> bool {
        let chars = source.chars().collect::<Vec<_>>();
        if cursor_char_pos < self.replace_start_char
            || self.replace_start_char > chars.len()
            || cursor_char_pos > chars.len()
        {
            return false;
        }
        let typed_prefix = chars[self.replace_start_char..cursor_char_pos]
            .iter()
            .collect::<String>();
        if typed_prefix.contains(['\n', '\r']) {
            return false;
        }

        self.replace_end_char = cursor_char_pos;
        self.filtered = self
            .all_candidates
            .iter()
            .filter(|candidate| candidate_matches(candidate, &typed_prefix))
            .cloned()
            .collect();
        self.clamp_selection();
        true
    }

    pub fn select_next(&mut self, window_rows: usize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_top = 0;
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
        self.ensure_visible(window_rows);
    }

    pub fn select_prev(&mut self, window_rows: usize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_top = 0;
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
        self.ensure_visible(window_rows);
    }

    pub fn selected_candidate(&self) -> Option<&CompletionCandidate> {
        self.filtered.get(self.selected)
    }

    fn clamp_selection(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_top = 0;
            return;
        }
        self.selected = self.selected.min(self.filtered.len() - 1);
        self.scroll_top = self.scroll_top.min(self.filtered.len() - 1);
    }

    fn ensure_visible(&mut self, window_rows: usize) {
        let rows = window_rows.max(1);
        let max_top = self.filtered.len().saturating_sub(rows);
        self.scroll_top = self.scroll_top.min(max_top);
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        } else if self.selected >= self.scroll_top + rows {
            self.scroll_top = self.selected + 1 - rows;
        }
        self.scroll_top = self.scroll_top.min(max_top);
    }
}

fn candidate_matches(candidate: &CompletionCandidate, typed_prefix: &str) -> bool {
    let typed = typed_prefix.to_lowercase();
    if typed.is_empty() {
        return true;
    }
    let text = candidate.replace_text.to_lowercase();
    let label = text.strip_prefix('.').unwrap_or(&text);
    text.starts_with(&typed) || label.starts_with(&typed)
}

/// Scan Python-ish source for identifiers suitable for fallback word completion.
///
/// Strings (including triple-quoted strings), comments, and quote prefixes are
/// skipped.  The result is de-duplicated while preserving first-seen order.
pub fn scan_notebook_identifiers(sources: &[&str], min_len: usize) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for source in sources {
        scan_source_identifiers(source, min_len, &mut seen, &mut found);
    }
    found
}

#[derive(Clone, Copy)]
enum PythonQuote {
    Single(char),
    Triple(char),
}

fn scan_source_identifiers(
    source: &str,
    min_len: usize,
    seen: &mut HashSet<String>,
    found: &mut Vec<String>,
) {
    let chars = source.chars().collect::<Vec<_>>();
    let mut i = 0;
    let mut quote = None;
    let mut comment = false;
    while i < chars.len() {
        if comment {
            if chars[i] == '\n' {
                comment = false;
            }
            i += 1;
            continue;
        }
        if let Some(active) = quote {
            match active {
                PythonQuote::Single(delimiter) => {
                    if chars[i] == '\\' {
                        i = (i + 2).min(chars.len());
                    } else if chars[i] == delimiter {
                        quote = None;
                        i += 1;
                    } else if chars[i] == '\n' {
                        quote = None;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                PythonQuote::Triple(delimiter) => {
                    if chars[i] == '\\' {
                        i = (i + 2).min(chars.len());
                    } else if chars[i..].starts_with(&[delimiter, delimiter, delimiter]) {
                        quote = None;
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
            }
            continue;
        }

        if chars[i] == '#' {
            comment = true;
            i += 1;
            continue;
        }
        if matches!(chars[i], '\'' | '"') {
            quote = if chars[i..].starts_with(&[chars[i], chars[i], chars[i]]) {
                Some(PythonQuote::Triple(chars[i]))
            } else {
                Some(PythonQuote::Single(chars[i]))
            };
            i += if matches!(quote, Some(PythonQuote::Triple(_))) {
                3
            } else {
                1
            };
            continue;
        }

        if is_identifier_start(chars[i]) {
            let start = i;
            i += 1;
            while i < chars.len() && is_identifier_continue(chars[i]) {
                i += 1;
            }
            if i < chars.len()
                && matches!(chars[i], '\'' | '"')
                && is_quote_prefix(&chars[start..i])
            {
                quote = if chars[i..].starts_with(&[chars[i], chars[i], chars[i]]) {
                    Some(PythonQuote::Triple(chars[i]))
                } else {
                    Some(PythonQuote::Single(chars[i]))
                };
                i += if matches!(quote, Some(PythonQuote::Triple(_))) {
                    3
                } else {
                    1
                };
                continue;
            }
            let word = chars[start..i].iter().collect::<String>();
            if word.chars().count() >= min_len && seen.insert(word.clone()) {
                found.push(word);
            }
        } else {
            i += 1;
        }
    }
}

fn is_quote_prefix(chars: &[char]) -> bool {
    !chars.is_empty()
        && chars
            .iter()
            .all(|c| matches!(c, 'r' | 'R' | 'u' | 'U' | 'b' | 'B' | 'f' | 'F'))
}

fn is_identifier_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_identifier_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    /// The callee expression immediately before the opening parenthesis.
    pub callee: String,
    /// Character offset immediately after the callee, suitable for Inspect.
    pub callee_end_char: usize,
    /// Number of top-level commas before the active argument.
    pub active_arg: usize,
    /// Keyword name when the active argument starts with `name=`.
    pub kwarg_name: Option<String>,
    /// Character offset of the opening parenthesis.
    pub open_paren_char: usize,
    /// Compatibility aliases useful to callers that model the Jotter API.
    pub open: usize,
    pub arg: usize,
    pub keyword: Option<String>,
    pub keyword_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SignaturePopup {
    pub signature_text: String,
    pub active_param_byte_range: Option<std::ops::Range<usize>>,
}

impl SignaturePopup {
    /// Find the innermost open call containing a character cursor position.
    pub fn parse_enclosing_call(source: &str, cursor_char_pos: usize) -> Option<CallSite> {
        let chars = source.chars().collect::<Vec<_>>();
        if cursor_char_pos > chars.len() {
            return None;
        }
        let prefix = &chars[..cursor_char_pos];
        let mut stack = Vec::<BracketFrame>::new();
        let mut i = 0;
        while i < prefix.len() {
            match prefix[i] {
                '#' => {
                    while i < prefix.len() && prefix[i] != '\n' {
                        i += 1;
                    }
                }
                '\'' | '"' => {
                    i = skip_python_string(prefix, i)?;
                }
                '(' | '[' | '{' => {
                    stack.push(BracketFrame {
                        opener: prefix[i],
                        position: i,
                        comma_count: 0,
                        argument_start: i + 1,
                        named_before: false,
                    });
                    i += 1;
                }
                ')' | ']' | '}' => {
                    let expected = matching_open(prefix[i]);
                    let frame = stack.pop()?;
                    if frame.opener != expected {
                        return None;
                    }
                    i += 1;
                }
                ',' => {
                    if let Some(frame) = stack.last_mut() {
                        let argument = prefix[frame.argument_start..i].iter().collect::<String>();
                        frame.named_before |= keyword_name(&argument).is_some()
                            || argument.trim_start().starts_with("**");
                        frame.comma_count += 1;
                        frame.argument_start = i + 1;
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }

        let frame_index = stack.iter().rposition(|frame| frame.opener == '(')?;
        let frame = stack[frame_index];
        let callee = callee_before(&chars, frame.position)?;
        let kwarg_name = if stack
            .last()
            .is_some_and(|last| last.position == frame.position)
        {
            let argument = prefix[frame.argument_start..].iter().collect::<String>();
            keyword_name(&argument)
        } else {
            None
        };
        Some(CallSite {
            callee,
            callee_end_char: frame.position,
            active_arg: frame.comma_count,
            kwarg_name: kwarg_name.clone(),
            open_paren_char: frame.position,
            open: frame.position,
            arg: frame.comma_count,
            keyword: kwarg_name.clone(),
            keyword_name: kwarg_name,
        })
    }

    /// Extract a callable signature from IPython/Jupyter inspect output and
    /// locate the parameter active at the supplied positional/keyword argument.
    pub fn from_inspect_doc(
        doc: &str,
        active_arg: usize,
        kwarg_name: Option<&str>,
    ) -> Option<SignaturePopup> {
        let plain = strip_ansi(doc);
        let signature = extract_signature(&plain)?;
        let parameter_ranges = signature_parameter_ranges(&signature);
        let active_param_byte_range = if let Some(keyword) = kwarg_name {
            parameter_ranges.iter().find_map(|range| {
                let parameter = &signature[range.clone()];
                (parameter_name(parameter).as_deref() == Some(keyword)).then(|| range.clone())
            })
        } else {
            positional_parameter_range(&signature, &parameter_ranges, active_arg)
        };
        Some(SignaturePopup {
            signature_text: signature,
            active_param_byte_range,
        })
    }
}

#[derive(Clone, Copy)]
struct BracketFrame {
    opener: char,
    position: usize,
    comma_count: usize,
    argument_start: usize,
    named_before: bool,
}

fn matching_open(close: char) -> char {
    match close {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => close,
    }
}

fn skip_python_string(chars: &[char], start: usize) -> Option<usize> {
    let delimiter = chars[start];
    let triple = chars[start..].starts_with(&[delimiter, delimiter, delimiter]);
    let width = if triple { 3 } else { 1 };
    let mut i = start + width;
    while i < chars.len() {
        if chars[i] == '\\' {
            i = (i + 2).min(chars.len());
        } else if chars[i] == '\n' && !triple {
            return None;
        } else if chars[i..].starts_with(&[delimiter; 3]) && triple {
            return Some(i + 3);
        } else if chars[i] == delimiter && !triple {
            return Some(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

fn keyword_name(argument: &str) -> Option<String> {
    let (name, value) = argument.trim_start().split_once('=')?;
    let name = name.trim_end();
    if value.starts_with('=')
        || name.is_empty()
        || name.starts_with(|c: char| c.is_ascii_digit())
        || !name.chars().all(is_identifier_continue)
    {
        return None;
    }
    Some(name.to_string())
}

fn callee_before(chars: &[char], open: usize) -> Option<String> {
    let mut end = open;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (is_identifier_continue(chars[start - 1]) || chars[start - 1] == '.') {
        start -= 1;
    }
    let mut callee = chars[start..end].iter().collect::<String>();
    while callee.starts_with('.') {
        callee.remove(0);
    }
    if callee.is_empty()
        || callee.starts_with(|c: char| c.is_ascii_digit())
        || !callee
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(is_identifier_continue))
    {
        return None;
    }
    const PYTHON_KEYWORDS: &[&str] = &[
        "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def",
        "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
        "import", "in", "is", "lambda", "match", "None", "nonlocal", "not", "or", "pass", "raise",
        "return", "True", "try", "type", "while", "with", "yield",
    ];
    (!PYTHON_KEYWORDS.contains(&callee.as_str())).then_some(callee)
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            output.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for item in chars.by_ref() {
                    if item.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(item) = chars.next() {
                    if item == '\x07' {
                        break;
                    }
                    if item == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    output
}

fn extract_signature(doc: &str) -> Option<String> {
    let mut in_signature = false;
    let mut lines = Vec::<String>::new();
    for line in doc.lines() {
        let trimmed = line.trim();
        if let Some((header, rest)) = inspect_header(trimmed) {
            if is_signature_header(header) {
                in_signature = true;
                if !rest.trim().is_empty() {
                    lines.push(rest.trim().to_string());
                }
            } else if in_signature {
                break;
            }
            continue;
        }
        if in_signature {
            lines.push(trimmed.to_string());
        }
    }

    if lines.is_empty() {
        lines = doc
            .lines()
            .map(str::trim)
            .filter(|line| looks_like_signature(line))
            .map(str::to_string)
            .collect();
    }
    if lines.is_empty() {
        return None;
    }

    let joined = lines
        .iter()
        .filter(|line| !line.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let open = joined.find('(')?;
    let close = matching_close_in_signature(&joined, open)?;
    let mut signature = joined[..=close].trim().to_string();
    if let Some(rest) = joined.get(close + 1..).map(str::trim)
        && rest.starts_with("->")
    {
        signature.push(' ');
        signature.push_str(rest);
    }
    Some(clean_signature_spacing(&signature))
}

fn inspect_header(line: &str) -> Option<(&str, &str)> {
    let (header, rest) = line.split_once(':')?;
    let header = header.trim();
    if header.is_empty()
        || !header.chars().all(|c| c.is_ascii_alphabetic() || c == ' ')
        || !header.starts_with(|c: char| c.is_ascii_uppercase())
    {
        return None;
    }
    Some((header, rest))
}

fn is_signature_header(header: &str) -> bool {
    matches!(header, "Signature" | "Init signature" | "Call signature")
}

fn looks_like_signature(line: &str) -> bool {
    let Some(open) = line.find('(') else {
        return false;
    };
    open > 0
        && line[..open]
            .trim()
            .chars()
            .all(|c| is_identifier_continue(c) || c == '.' || c.is_whitespace())
}

fn matching_close_in_signature(text: &str, open: usize) -> Option<usize> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut depth = 0usize;
    let mut quote = None;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        if let Some(delimiter) = quote {
            if c == '\\' {
                i = (i + 2).min(chars.len());
                continue;
            }
            if c == delimiter {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn clean_signature_spacing(signature: &str) -> String {
    signature
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ,", ",")
        .replace(",)", ")")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn signature_parameter_ranges(signature: &str) -> Vec<std::ops::Range<usize>> {
    let Some(open) = signature.find('(') else {
        return Vec::new();
    };
    let bytes = signature.as_bytes();
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut start = open + 1;
    let mut index = open + 1;
    while index < bytes.len() {
        let c = bytes[index] as char;
        if let Some(delimiter) = quote {
            if c == '\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if c == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => {
                push_parameter_range(signature, start, index, &mut ranges);
                break;
            }
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                push_parameter_range(signature, start, index, &mut ranges);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    ranges
}

fn push_parameter_range(
    signature: &str,
    start: usize,
    end: usize,
    output: &mut Vec<std::ops::Range<usize>>,
) {
    let text = &signature[start..end];
    let leading = text.len() - text.trim_start().len();
    let trailing = text.trim_end().len();
    if leading < trailing {
        output.push(start + leading..start + trailing);
    }
}

fn parameter_name(parameter: &str) -> Option<String> {
    let name = parameter
        .trim()
        .trim_start_matches('*')
        .split(['=', ':'])
        .next()?
        .trim();
    (!name.is_empty() && name.chars().all(is_identifier_continue)).then(|| name.to_string())
}

fn positional_parameter_range(
    signature: &str,
    ranges: &[std::ops::Range<usize>],
    active_arg: usize,
) -> Option<std::ops::Range<usize>> {
    let mut positional = 0usize;
    let mut keyword_only = false;
    for range in ranges {
        let parameter = signature[range.clone()].trim();
        if parameter == "/" {
            continue;
        }
        if parameter == "*" {
            keyword_only = true;
            continue;
        }
        if parameter.starts_with("**") {
            return None;
        }
        if parameter.starts_with('*') {
            return (!keyword_only).then(|| range.clone());
        }
        if keyword_only {
            return None;
        }
        if positional == active_arg {
            return Some(range.clone());
        }
        positional += 1;
    }
    None
}

#[derive(Clone, Debug)]
pub struct InspectPager {
    pub title: String,
    pub ansi_text: String,
    pub scroll_top: usize,
}

impl InspectPager {
    pub fn new(title: impl Into<String>, ansi_text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ansi_text: ansi_text.into(),
            scroll_top: 0,
        }
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll_top = self.scroll_top.saturating_add(rows).min(self.max_scroll());
    }

    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll_top = self.scroll_top.saturating_sub(rows);
    }

    pub fn scroll_top(&mut self) {
        self.scroll_top = 0;
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll_top = self.max_scroll();
    }

    fn max_scroll(&self) -> usize {
        self.ansi_text.split('\n').count().saturating_sub(1)
    }
}
