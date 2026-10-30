//! Language server answers as the text the model reads, in the wording Claude
//! Code's `LSP` tool uses.

use std::path::Path;

use serde_json::Value;

use super::client::uri_to_path;

/// `uri` as a path relative to `cwd` when that is shorter and does not climb
/// out by more than one level, else the absolute path.
fn format_uri(uri: Option<&str>, cwd: &Path) -> String {
    let Some(uri) = uri.filter(|u| !u.is_empty()) else {
        return "<unknown location>".to_string();
    };
    let path = uri_to_path(uri);
    if let Ok(rel) = Path::new(&path).strip_prefix(cwd) {
        let rel = rel.to_string_lossy().into_owned();
        if rel.len() < path.len() {
            return rel;
        }
    }
    path
}

fn line_of(range: &Value) -> i64 {
    range
        .pointer("/start/line")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1
}

fn char_of(range: &Value) -> i64 {
    range
        .pointer("/start/character")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// A `Location` or `LocationLink` as `(uri, range)`.
fn location(value: &Value) -> Option<(&str, &Value)> {
    if let Some(uri) = value.get("targetUri").and_then(Value::as_str) {
        let range = value
            .get("targetSelectionRange")
            .or_else(|| value.get("targetRange"))?;
        return Some((uri, range));
    }
    Some((value.get("uri")?.as_str()?, value.get("range")?))
}

fn locations(result: &Value) -> Vec<(&str, &Value)> {
    match result {
        Value::Array(list) => list.iter().filter_map(location).collect(),
        Value::Null => Vec::new(),
        single => location(single).into_iter().collect(),
    }
}

fn place(uri: &str, range: &Value, cwd: &Path) -> String {
    format!(
        "{}:{}:{}",
        format_uri(Some(uri), cwd),
        line_of(range),
        char_of(range)
    )
}

pub fn definition(result: &Value, cwd: &Path) -> String {
    const NONE: &str = "No definition found. This may occur if the cursor is not on a symbol, or if the definition is in an external library not indexed by the LSP server.";
    let found = locations(result);
    match found.as_slice() {
        [] => NONE.to_string(),
        [(uri, range)] => format!("Defined in {}", place(uri, range, cwd)),
        many => {
            let list = many
                .iter()
                .map(|(uri, range)| format!("  {}", place(uri, range, cwd)))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Found {} definitions:\n{list}", many.len())
        }
    }
}

pub fn implementation(result: &Value, cwd: &Path) -> String {
    let found = locations(result);
    match found.as_slice() {
        [] => "No implementation found. This may occur if the cursor is not on an interface or abstract method, or if the LSP server has not fully indexed the workspace.".to_string(),
        [(uri, range)] => format!("Implemented in {}", place(uri, range, cwd)),
        many => {
            let list = many
                .iter()
                .map(|(uri, range)| format!("  {}", place(uri, range, cwd)))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Found {} implementations:\n{list}", many.len())
        }
    }
}

pub fn references(result: &Value, cwd: &Path) -> String {
    const NONE: &str = "No references found. This may occur if the symbol has no usages, or if the LSP server has not fully indexed the workspace.";
    let found = locations(result);
    if found.is_empty() {
        return NONE.to_string();
    }
    if let [(uri, range)] = found.as_slice() {
        return format!("Found 1 reference:\n  {}", place(uri, range, cwd));
    }
    let mut files: Vec<(String, Vec<&Value>)> = Vec::new();
    for (uri, range) in &found {
        let name = format_uri(Some(uri), cwd);
        match files.iter_mut().find(|(have, _)| *have == name) {
            Some((_, ranges)) => ranges.push(range),
            None => files.push((name, vec![range])),
        }
    }
    let mut lines = vec![format!(
        "Found {} references across {} files:",
        found.len(),
        files.len()
    )];
    for (name, ranges) in files {
        lines.push(format!("\n{name}:"));
        for range in ranges {
            lines.push(format!("  Line {}:{}", line_of(range), char_of(range)));
        }
    }
    lines.join("\n")
}

fn markup(contents: &Value) -> String {
    match contents {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::String(text) => text.clone(),
        other => other
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

pub fn hover(result: &Value) -> String {
    if result.is_null() {
        return "No hover information available. This may occur if the cursor is not on a symbol, or if the LSP server has not fully indexed the file.".to_string();
    }
    let content = markup(result.get("contents").unwrap_or(&Value::Null));
    match result.get("range") {
        Some(range) => format!(
            "Hover info at {}:{}:\n\n{content}",
            line_of(range),
            char_of(range)
        ),
        None => content,
    }
}

fn kind_name(kind: i64) -> &'static str {
    const KINDS: [&str; 26] = [
        "File",
        "Module",
        "Namespace",
        "Package",
        "Class",
        "Method",
        "Property",
        "Field",
        "Constructor",
        "Enum",
        "Interface",
        "Function",
        "Variable",
        "Constant",
        "String",
        "Number",
        "Boolean",
        "Array",
        "Object",
        "Key",
        "Null",
        "EnumMember",
        "Struct",
        "Event",
        "Operator",
        "TypeParameter",
    ];
    usize::try_from(kind - 1)
        .ok()
        .and_then(|i| KINDS.get(i))
        .copied()
        .unwrap_or("Unknown")
}

fn symbol_node(symbol: &Value, depth: usize, out: &mut Vec<String>) {
    let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
    let kind = kind_name(symbol.get("kind").and_then(Value::as_i64).unwrap_or(0));
    let mut line = format!("{}{name} ({kind})", "  ".repeat(depth));
    if let Some(detail) = symbol
        .get("detail")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        line.push(' ');
        line.push_str(detail);
    }
    line.push_str(&format!(
        " - Line {}",
        line_of(symbol.get("range").unwrap_or(&Value::Null))
    ));
    out.push(line);
    for child in symbol
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        symbol_node(child, depth + 1, out);
    }
}

pub fn document_symbols(result: &Value, cwd: &Path) -> String {
    let Some(list) = result.as_array().filter(|l| !l.is_empty()) else {
        return "No symbols found in document. This may occur if the file is empty, not supported by the LSP server, or if the server has not fully indexed the file.".to_string();
    };
    if list[0].get("location").is_some() {
        return workspace_symbols(result, cwd);
    }
    let mut lines = vec!["Document symbols:".to_string()];
    for symbol in list {
        symbol_node(symbol, 0, &mut lines);
    }
    lines.join("\n")
}

pub fn workspace_symbols(result: &Value, cwd: &Path) -> String {
    let valid: Vec<&Value> = result
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s.pointer("/location/uri").is_some())
        .collect();
    if valid.is_empty() {
        return "No symbols found in workspace. This may occur if the workspace is empty, or if the LSP server has not finished indexing the project.".to_string();
    }
    let mut lines = vec![format!(
        "Found {} {} in workspace:",
        valid.len(),
        plural(valid.len(), "symbol")
    )];
    let mut files: Vec<(String, Vec<&Value>)> = Vec::new();
    for symbol in valid {
        let name = format_uri(symbol.pointer("/location/uri").and_then(Value::as_str), cwd);
        match files.iter_mut().find(|(have, _)| *have == name) {
            Some((_, symbols)) => symbols.push(symbol),
            None => files.push((name, vec![symbol])),
        }
    }
    for (name, symbols) in files {
        lines.push(format!("\n{name}:"));
        for symbol in symbols {
            let mut line = format!(
                "  {} ({}) - Line {}",
                symbol.get("name").and_then(Value::as_str).unwrap_or("?"),
                kind_name(symbol.get("kind").and_then(Value::as_i64).unwrap_or(0)),
                line_of(symbol.pointer("/location/range").unwrap_or(&Value::Null))
            );
            if let Some(container) = symbol
                .get("containerName")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                line.push_str(&format!(" in {container}"));
            }
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn call_item(item: &Value, cwd: &Path) -> String {
    let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
    let kind = kind_name(item.get("kind").and_then(Value::as_i64).unwrap_or(0));
    let Some(uri) = item.get("uri").and_then(Value::as_str) else {
        return format!("{name} ({kind}) - <unknown location>");
    };
    let mut text = format!(
        "{name} ({kind}) - {}:{}",
        format_uri(Some(uri), cwd),
        line_of(item.get("range").unwrap_or(&Value::Null))
    );
    if let Some(detail) = item
        .get("detail")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        text.push_str(&format!(" [{detail}]"));
    }
    text
}

pub fn prepare_call_hierarchy(result: &Value, cwd: &Path) -> String {
    match result.as_array().map(Vec::as_slice) {
        None | Some([]) => "No call hierarchy item found at this position".to_string(),
        Some([item]) => format!("Call hierarchy item: {}", call_item(item, cwd)),
        Some(items) => {
            let mut lines = vec![format!("Found {} call hierarchy items:", items.len())];
            lines.extend(items.iter().map(|i| format!("  {}", call_item(i, cwd))));
            lines.join("\n")
        }
    }
}

fn calls(result: &Value, cwd: &Path, side: &str, label: &str, sites: &str) -> Option<String> {
    let list = result.as_array().filter(|l| !l.is_empty())?;
    let mut lines = vec![format!(
        "Found {} {label} {}:",
        list.len(),
        plural(list.len(), "call")
    )];
    let mut files: Vec<(String, Vec<&Value>)> = Vec::new();
    for call in list {
        let Some(item) = call.get(side) else { continue };
        let name = format_uri(item.get("uri").and_then(Value::as_str), cwd);
        match files.iter_mut().find(|(have, _)| *have == name) {
            Some((_, calls)) => calls.push(call),
            None => files.push((name, vec![call])),
        }
    }
    for (name, group) in files {
        lines.push(format!("\n{name}:"));
        for call in group {
            let item = &call[side];
            let mut line = format!(
                "  {} ({}) - Line {}",
                item.get("name").and_then(Value::as_str).unwrap_or("?"),
                kind_name(item.get("kind").and_then(Value::as_i64).unwrap_or(0)),
                line_of(item.get("range").unwrap_or(&Value::Null))
            );
            let ranges: Vec<String> = call
                .get("fromRanges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|r| format!("{}:{}", line_of(r), char_of(r)))
                .collect();
            if !ranges.is_empty() {
                line.push_str(&format!(" [{sites}: {}]", ranges.join(", ")));
            }
            lines.push(line);
        }
    }
    Some(lines.join("\n"))
}

pub fn incoming_calls(result: &Value, cwd: &Path) -> String {
    calls(result, cwd, "from", "incoming", "calls at")
        .unwrap_or_else(|| "No incoming calls found (nothing calls this function)".to_string())
}

pub fn outgoing_calls(result: &Value, cwd: &Path) -> String {
    calls(result, cwd, "to", "outgoing", "called from")
        .unwrap_or_else(|| "No outgoing calls found (this function calls nothing)".to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CWD: &str = "/proj";

    fn at(file: &str, line: u32, ch: u32) -> Value {
        json!({
            "uri": format!("file://{CWD}/{file}"),
            "range": {"start": {"line": line, "character": ch}, "end": {"line": line, "character": ch + 1}},
        })
    }

    #[test]
    fn one_definition_is_one_line_and_none_explains_itself() {
        let cwd = Path::new(CWD);
        assert_eq!(
            definition(&at("src/a.rs", 4, 2), cwd),
            "Defined in src/a.rs:5:3"
        );
        assert!(definition(&Value::Null, cwd).starts_with("No definition found."));
        let many = json!([at("a.rs", 0, 0), {
            "targetUri": "file:///proj/b.rs",
            "targetRange": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 1}},
            "targetSelectionRange": {"start": {"line": 9, "character": 4}, "end": {"line": 9, "character": 5}},
        }]);
        assert_eq!(
            definition(&many, cwd),
            "Found 2 definitions:\n  a.rs:1:1\n  b.rs:10:5"
        );
    }

    #[test]
    fn references_group_by_file() {
        let cwd = Path::new(CWD);
        let refs = json!([at("a.rs", 1, 0), at("b.rs", 2, 4), at("a.rs", 7, 1)]);
        assert_eq!(
            references(&refs, cwd),
            "Found 3 references across 2 files:\n\na.rs:\n  Line 2:1\n  Line 8:2\n\nb.rs:\n  Line 3:5"
        );
        assert_eq!(
            references(&json!([at("a.rs", 1, 0)]), cwd),
            "Found 1 reference:\n  a.rs:2:1"
        );
        assert!(references(&json!([]), cwd).starts_with("No references found."));
    }

    #[test]
    fn hover_keeps_the_range_and_the_markup() {
        let result = json!({
            "contents": {"kind": "markdown", "value": "```rust\nfn main()\n```"},
            "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7}},
        });
        assert_eq!(
            hover(&result),
            "Hover info at 1:4:\n\n```rust\nfn main()\n```"
        );
        assert_eq!(hover(&json!({"contents": ["a", {"value": "b"}]})), "a\n\nb");
    }

    #[test]
    fn document_symbols_indent_children() {
        let result = json!([{
            "name": "Shape", "kind": 23, "detail": "struct",
            "range": {"start": {"line": 2, "character": 0}, "end": {"line": 5, "character": 0}},
            "children": [{
                "name": "area", "kind": 6,
                "range": {"start": {"line": 3, "character": 0}, "end": {"line": 4, "character": 0}},
            }],
        }]);
        assert_eq!(
            document_symbols(&result, Path::new(CWD)),
            "Document symbols:\nShape (Struct) struct - Line 3\n  area (Method) - Line 4"
        );
        assert!(
            document_symbols(&json!([]), Path::new(CWD))
                .starts_with("No symbols found in document.")
        );
    }

    #[test]
    fn workspace_symbols_group_and_name_the_container() {
        let result = json!([{
            "name": "run", "kind": 12, "containerName": "app",
            "location": at("src/app.rs", 10, 0),
        }]);
        assert_eq!(
            workspace_symbols(&result, Path::new(CWD)),
            "Found 1 symbol in workspace:\n\nsrc/app.rs:\n  run (Function) - Line 11 in app"
        );
        assert!(
            workspace_symbols(&json!(null), Path::new(CWD))
                .starts_with("No symbols found in workspace.")
        );
    }

    #[test]
    fn call_hierarchy_wording() {
        let cwd = Path::new(CWD);
        let item = json!({
            "name": "go", "kind": 12, "uri": "file:///proj/a.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
        });
        assert_eq!(
            prepare_call_hierarchy(&json!([item.clone()]), cwd),
            "Call hierarchy item: go (Function) - a.rs:1"
        );
        let incoming = json!([{
            "from": item,
            "fromRanges": [{"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 4}}],
        }]);
        assert_eq!(
            incoming_calls(&incoming, cwd),
            "Found 1 incoming call:\n\na.rs:\n  go (Function) - Line 1 [calls at: 5:3]"
        );
        assert_eq!(
            outgoing_calls(&json!([]), cwd),
            "No outgoing calls found (this function calls nothing)"
        );
    }
}
