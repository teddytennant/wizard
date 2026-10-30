//! A JSON Schema checker for `--json-schema`.
//!
//! Covers the keywords a structured-output schema uses in practice: `type`,
//! `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`,
//! the length and range bounds, `pattern`, `anyOf`, `oneOf`, `allOf`, `not`
//! and local `$ref`. Keywords it does not know are ignored, so a schema that
//! uses one is checked less strictly, never rejected.

use serde_json::Value;

/// Check `value` against `schema`. The error lists every violation, each
/// prefixed with the path to the value (`$.items[2].name`).
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check(schema, schema, value, "$", &mut errors, 0);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The JSON value in a model's final message: the whole text, the first
/// fenced block, or the outermost `{...}` / `[...]` span.
pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Some(value);
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```")
            && let Ok(value) = serde_json::from_str(after[..end].trim())
        {
            return Some(value);
        }
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(a), Some(b)) = (trimmed.find(open), trimmed.rfind(close))
            && a < b
            && let Ok(value) = serde_json::from_str(&trimmed[a..=b])
        {
            return Some(value);
        }
    }
    None
}

const MAX_DEPTH: usize = 64;

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(want: &str, value: &Value) -> bool {
    match want {
        "number" => matches!(value, Value::Number(_)),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        other => type_name(value) == other,
    }
}

fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        Some(root)
    } else {
        root.pointer(pointer)
    }
}

fn check(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(object) = schema.as_object() else {
        if schema == &Value::Bool(false) {
            errors.push(format!("{path}: not allowed"));
        }
        return;
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve(root, reference)
    {
        check(root, target, value, path, errors, depth + 1);
    }
    if let Some(want) = object.get("type") {
        let wants: Vec<&str> = match want {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !wants.is_empty() && !wants.iter().any(|w| type_matches(w, value)) {
            errors.push(format!(
                "{path}: expected {}, got {}",
                wants.join(" or "),
                type_name(value)
            ));
            return;
        }
    }
    if let Some(options) = object.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        errors.push(format!("{path}: not one of the allowed values"));
    }
    if let Some(expected) = object.get("const")
        && expected != value
    {
        errors.push(format!("{path}: expected {expected}"));
    }
    match value {
        Value::Object(fields) => {
            for name in object
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !fields.contains_key(name) {
                    errors.push(format!("{path}: missing required property '{name}'"));
                }
            }
            let props = object.get("properties").and_then(Value::as_object);
            for (name, field) in fields {
                let child = format!("{path}.{name}");
                match props.and_then(|p| p.get(name)) {
                    Some(sub) => check(root, sub, field, &child, errors, depth + 1),
                    None => match object.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            errors.push(format!("{path}: unexpected property '{name}'"));
                        }
                        Some(sub @ Value::Object(_)) => {
                            check(root, sub, field, &child, errors, depth + 1);
                        }
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = object.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < min
            {
                errors.push(format!("{path}: needs at least {min} items"));
            }
            if let Some(max) = object.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > max
            {
                errors.push(format!("{path}: allows at most {max} items"));
            }
            if let Some(sub) = object.get("items") {
                for (at, item) in items.iter().enumerate() {
                    check(root, sub, item, &format!("{path}[{at}]"), errors, depth + 1);
                }
            }
            if object.get("uniqueItems") == Some(&Value::Bool(true)) {
                for (at, item) in items.iter().enumerate() {
                    if items[..at].contains(item) {
                        errors.push(format!("{path}[{at}]: duplicate item"));
                    }
                }
            }
        }
        Value::String(text) => {
            let length = text.chars().count() as u64;
            if let Some(min) = object.get("minLength").and_then(Value::as_u64)
                && length < min
            {
                errors.push(format!("{path}: shorter than {min} characters"));
            }
            if let Some(max) = object.get("maxLength").and_then(Value::as_u64)
                && length > max
            {
                errors.push(format!("{path}: longer than {max} characters"));
            }
            if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
                && let Ok(re) = regex::Regex::new(pattern)
                && !re.is_match(text)
            {
                errors.push(format!("{path}: does not match pattern {pattern}"));
            }
        }
        Value::Number(n) => {
            if let Some(x) = n.as_f64() {
                let bound = |key: &str| object.get(key).and_then(Value::as_f64);
                if let Some(min) = bound("minimum")
                    && x < min
                {
                    errors.push(format!("{path}: below the minimum {min}"));
                }
                if let Some(max) = bound("maximum")
                    && x > max
                {
                    errors.push(format!("{path}: above the maximum {max}"));
                }
                if let Some(min) = bound("exclusiveMinimum")
                    && x <= min
                {
                    errors.push(format!("{path}: must be above {min}"));
                }
                if let Some(max) = bound("exclusiveMaximum")
                    && x >= max
                {
                    errors.push(format!("{path}: must be below {max}"));
                }
            }
        }
        _ => {}
    }
    let branch = |key: &str| {
        object.get(key).and_then(Value::as_array).map(|subs| {
            subs.iter()
                .map(|sub| {
                    let mut inner = Vec::new();
                    check(root, sub, value, path, &mut inner, depth + 1);
                    inner
                })
                .collect::<Vec<_>>()
        })
    };
    if let Some(results) = branch("allOf") {
        for inner in results {
            errors.extend(inner);
        }
    }
    if let Some(results) = branch("anyOf")
        && !results.iter().any(Vec::is_empty)
    {
        errors.push(format!("{path}: matches none of the anyOf alternatives"));
    }
    if let Some(results) = branch("oneOf") {
        let matching = results.iter().filter(|inner| inner.is_empty()).count();
        if matching != 1 {
            errors.push(format!(
                "{path}: matches {matching} of the oneOf alternatives, expected exactly 1"
            ));
        }
    }
    if let Some(sub) = object.get("not") {
        let mut inner = Vec::new();
        check(root, sub, value, path, &mut inner, depth + 1);
        if inner.is_empty() {
            errors.push(format!("{path}: matches a schema it must not match"));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_matching_value_passes() {
        let schema = json!({"type":"object","properties":{"n":{"type":"integer"},"tags":{"type":"array","items":{"type":"string"}}},"required":["n"]});
        assert!(validate(&schema, &json!({"n": 3, "tags": ["a"]})).is_ok());
    }

    #[test]
    fn every_violation_is_reported_with_its_path() {
        let schema = json!({"type":"object","properties":{"n":{"type":"integer"},"tags":{"type":"array","items":{"type":"string"}}},"required":["n","m"]});
        let errors = validate(&schema, &json!({"n": "x", "tags": ["a", 2]})).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("$.n: expected integer")));
        assert!(errors.iter().any(|e| e.contains("$.tags[1]")));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("missing required property 'm'"))
        );
    }

    #[test]
    fn additional_properties_false_rejects_extras() {
        let schema = json!({"type":"object","properties":{"a":{}},"additionalProperties":false});
        assert!(validate(&schema, &json!({"a":1})).is_ok());
        assert!(validate(&schema, &json!({"a":1,"b":2})).is_err());
    }

    #[test]
    fn enum_const_bounds_and_pattern() {
        assert!(validate(&json!({"enum":["a","b"]}), &json!("c")).is_err());
        assert!(validate(&json!({"const": 1}), &json!(2)).is_err());
        assert!(validate(&json!({"type":"number","minimum":1,"maximum":3}), &json!(4)).is_err());
        assert!(validate(&json!({"type":"string","pattern":"^a+$"}), &json!("ab")).is_err());
        assert!(validate(&json!({"type":"string","minLength":2}), &json!("a")).is_err());
        assert!(validate(&json!({"type":"array","minItems":2}), &json!([1])).is_err());
    }

    #[test]
    fn combinators_and_refs() {
        let schema = json!({"$defs":{"s":{"type":"string"}},"anyOf":[{"$ref":"#/$defs/s"},{"type":"integer"}]});
        assert!(validate(&schema, &json!("x")).is_ok());
        assert!(validate(&schema, &json!(1)).is_ok());
        assert!(validate(&schema, &json!(true)).is_err());
        assert!(
            validate(
                &json!({"oneOf":[{"type":"integer"},{"type":"number"}]}),
                &json!(1)
            )
            .is_err()
        );
    }

    #[test]
    fn json_is_found_in_prose_and_fences() {
        assert_eq!(extract_json(" {\"a\":1} "), Some(json!({"a":1})));
        assert_eq!(
            extract_json("Here:\n```json\n{\"a\":2}\n```\n"),
            Some(json!({"a":2}))
        );
        assert_eq!(
            extract_json("The answer is {\"a\":3}."),
            Some(json!({"a":3}))
        );
        assert_eq!(extract_json("no json here"), None);
    }
}
