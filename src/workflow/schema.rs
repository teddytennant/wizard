//! The JSON Schema subset `agent({schema})` needs.
//!
//! Two jobs. [`check`] refuses a schema before an agent starts: Claude Code
//! requires an object at the root with `required` inside `properties`, and
//! refuses one that nothing can satisfy, naming the contradiction. [`validate`]
//! checks what the agent handed back. Both cover the keywords structured output
//! schemas use in practice (types, properties, required, items, enum, const,
//! numeric and length bounds, pattern, anyOf, oneOf, allOf, local `$ref`) and
//! ignore the rest, which errs toward accepting.

use serde_json::Value;

/// Refuse a schema that cannot be used as an agent's output shape.
pub fn check(schema: &Value) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Err("agent() schema must be a JSON Schema object".into());
    };
    if object.get("type").and_then(Value::as_str) != Some("object")
        || !object.get("properties").is_some_and(Value::is_object)
    {
        return Err(
            "agent() schema must have {type: 'object', properties: {...}} at its root".into(),
        );
    }
    let properties = object["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(required) = object.get("required") {
        let Some(names) = required.as_array() else {
            return Err("agent() schema `required` must be an array".into());
        };
        for name in names {
            let name = name.as_str().unwrap_or_default();
            if !properties.contains_key(name) {
                return Err(format!(
                    "agent() schema requires `{name}`, which is not in its properties"
                ));
            }
        }
    }
    if let Some(why) = unsatisfiable(schema, 0) {
        return Err(format!("agent() schema is unsatisfiable: {why}"));
    }
    for name in object
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(why) = unsatisfiable(&properties[name], 0) {
            return Err(format!(
                "agent() schema is unsatisfiable: required property `{name}`: {why}"
            ));
        }
    }
    Ok(())
}

fn unsatisfiable(schema: &Value, depth: usize) -> Option<String> {
    if depth > 32 {
        return None;
    }
    if schema == &Value::Bool(false) {
        return Some("the schema is `false`".into());
    }
    let object = schema.as_object()?;
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && values.is_empty()
    {
        return Some("`enum` is empty".into());
    }
    let number = |key: &str| object.get(key).and_then(Value::as_f64);
    if let (Some(min), Some(max)) = (number("minimum"), number("maximum"))
        && min > max
    {
        return Some(format!("`minimum` {min} is above `maximum` {max}"));
    }
    for (lo, hi) in [
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
        ("minProperties", "maxProperties"),
    ] {
        if let (Some(a), Some(b)) = (number(lo), number(hi))
            && a > b
        {
            return Some(format!("`{lo}` {a} is above `{hi}` {b}"));
        }
    }
    if let Some(Value::Array(types)) = object.get("type")
        && types.is_empty()
    {
        return Some("`type` is an empty list".into());
    }
    if let Some(items) = object.get("items")
        && number("minItems").is_some_and(|n| n > 0.0)
        && let Some(why) = unsatisfiable(items, depth + 1)
    {
        return Some(format!("`items` can never match: {why}"));
    }
    if let Some(Value::Array(branches)) = object.get("anyOf")
        && !branches.is_empty()
        && branches
            .iter()
            .all(|b| unsatisfiable(b, depth + 1).is_some())
    {
        return Some("every `anyOf` branch is unsatisfiable".into());
    }
    None
}

/// Check `value` against `schema`; the error says where and why.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    walk(schema, schema, value, "$", 0)
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => true,
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn walk(root: &Value, schema: &Value, value: &Value, at: &str, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Ok(());
    }
    let object = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{at}: no value is allowed here")),
        Value::Object(object) => object,
        _ => return Ok(()),
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve(root, reference)
    {
        return walk(root, target, value, at, depth + 1);
    }
    if let Some(types) = object.get("type") {
        let ok = match types {
            Value::String(name) => type_matches(name, value),
            Value::Array(names) => names
                .iter()
                .filter_map(Value::as_str)
                .any(|name| type_matches(name, value)),
            _ => true,
        };
        if !ok {
            return Err(format!(
                "{at}: expected {}, got {}",
                match types {
                    Value::String(name) => name.clone(),
                    other => other.to_string(),
                },
                kind(value)
            ));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(format!(
            "{at}: {value} is not one of {}",
            Value::Array(values.clone())
        ));
    }
    if let Some(expected) = object.get("const")
        && expected != value
    {
        return Err(format!("{at}: expected the constant {expected}"));
    }
    if let Some(number) = value.as_f64() {
        let bound = |key: &str| object.get(key).and_then(Value::as_f64);
        if let Some(min) = bound("minimum")
            && number < min
        {
            return Err(format!("{at}: {number} is below the minimum {min}"));
        }
        if let Some(max) = bound("maximum")
            && number > max
        {
            return Err(format!("{at}: {number} is above the maximum {max}"));
        }
        if let Some(min) = bound("exclusiveMinimum")
            && number <= min
        {
            return Err(format!("{at}: {number} is not above {min}"));
        }
        if let Some(max) = bound("exclusiveMaximum")
            && number >= max
        {
            return Err(format!("{at}: {number} is not below {max}"));
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(min) = object.get("minLength").and_then(Value::as_u64)
            && length < min
        {
            return Err(format!("{at}: shorter than minLength {min}"));
        }
        if let Some(max) = object.get("maxLength").and_then(Value::as_u64)
            && length > max
        {
            return Err(format!("{at}: longer than maxLength {max}"));
        }
        if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
            && let Ok(re) = regex::Regex::new(pattern)
            && !re.is_match(text)
        {
            return Err(format!("{at}: does not match the pattern {pattern}"));
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(min) = object.get("minItems").and_then(Value::as_u64)
            && (items.len() as u64) < min
        {
            return Err(format!("{at}: fewer than minItems {min}"));
        }
        if let Some(max) = object.get("maxItems").and_then(Value::as_u64)
            && items.len() as u64 > max
        {
            return Err(format!("{at}: more than maxItems {max}"));
        }
        if let Some(item_schema) = object.get("items") {
            for (index, item) in items.iter().enumerate() {
                walk(
                    root,
                    item_schema,
                    item,
                    &format!("{at}[{index}]"),
                    depth + 1,
                )?;
            }
        }
    }
    if let Some(map) = value.as_object() {
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    return Err(format!("{at}: missing required property `{name}`"));
                }
            }
        }
        let properties = object.get("properties").and_then(Value::as_object);
        for (key, item) in map {
            if let Some(sub) = properties.and_then(|p| p.get(key)) {
                walk(root, sub, item, &format!("{at}.{key}"), depth + 1)?;
            } else {
                match object.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        return Err(format!("{at}: unexpected property `{key}`"));
                    }
                    Some(extra @ Value::Object(_)) => {
                        walk(root, extra, item, &format!("{at}.{key}"), depth + 1)?;
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(branches) = object.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            walk(root, branch, value, at, depth + 1)?;
        }
    }
    if let Some(branches) = object.get("anyOf").and_then(Value::as_array)
        && !branches
            .iter()
            .any(|b| walk(root, b, value, at, depth + 1).is_ok())
    {
        return Err(format!("{at}: matches none of the anyOf branches"));
    }
    if let Some(branches) = object.get("oneOf").and_then(Value::as_array) {
        let hits = branches
            .iter()
            .filter(|b| walk(root, b, value, at, depth + 1).is_ok())
            .count();
        if hits != 1 {
            return Err(format!(
                "{at}: matches {hits} of the oneOf branches, not exactly one"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn findings() -> Value {
        json!({
            "type": "object",
            "properties": {
                "findings": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"file": {"type": "string"}, "line": {"type": "integer", "minimum": 1}},
                    "required": ["file"]
                }},
                "verdict": {"enum": ["real", "fake"]}
            },
            "required": ["findings"]
        })
    }

    #[test]
    fn a_good_schema_passes_the_check() {
        check(&findings()).unwrap();
    }

    #[test]
    fn the_root_must_be_an_object_with_properties() {
        assert!(
            check(&json!({"type": "array"}))
                .unwrap_err()
                .contains("root")
        );
        assert!(
            check(&json!({"type": "object"}))
                .unwrap_err()
                .contains("root")
        );
        assert!(check(&json!("nope")).is_err());
    }

    #[test]
    fn required_must_be_inside_properties() {
        let err =
            check(&json!({"type":"object","properties":{"a":{}},"required":["b"]})).unwrap_err();
        assert!(err.contains("requires `b`"), "{err}");
    }

    #[test]
    fn a_contradiction_is_named() {
        let schema = json!({"type":"object","properties":{"n":{"type":"number","minimum":5,"maximum":1}},"required":["n"]});
        let err = check(&schema).unwrap_err();
        assert!(
            err.contains("unsatisfiable") && err.contains("`minimum` 5 is above `maximum` 1"),
            "{err}"
        );
        let schema = json!({"type":"object","properties":{"s":{"enum":[]}},"required":["s"]});
        assert!(check(&schema).unwrap_err().contains("`enum` is empty"));
        // Optional and unsatisfiable is allowed: the agent just leaves it out.
        let schema = json!({"type":"object","properties":{"s":{"enum":[]}}});
        check(&schema).unwrap();
    }

    #[test]
    fn validation_walks_into_arrays_and_objects() {
        let schema = findings();
        validate(
            &schema,
            &json!({"findings": [{"file": "a.rs", "line": 3}], "verdict": "real"}),
        )
        .unwrap();
        let err = validate(&schema, &json!({"findings": [{"line": 3}]})).unwrap_err();
        assert!(
            err.contains("$.findings[0]") && err.contains("`file`"),
            "{err}"
        );
        let err = validate(&schema, &json!({"findings": [{"file": "a", "line": 0}]})).unwrap_err();
        assert!(err.contains("below the minimum"), "{err}");
        let err = validate(&schema, &json!({"findings": [], "verdict": "maybe"})).unwrap_err();
        assert!(err.contains("not one of"), "{err}");
        let err = validate(&schema, &json!({"findings": "x"})).unwrap_err();
        assert!(err.contains("expected array, got string"), "{err}");
    }

    #[test]
    fn refs_and_branches_work() {
        let schema = json!({
            "type": "object",
            "definitions": {"pos": {"type": "integer", "minimum": 0}},
            "properties": {
                "a": {"$ref": "#/definitions/pos"},
                "b": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "c": {"oneOf": [{"type": "integer"}, {"type": "number"}]}
            },
            "additionalProperties": false
        });
        validate(&schema, &json!({"a": 2, "b": null})).unwrap();
        assert!(validate(&schema, &json!({"a": -1})).is_err());
        assert!(validate(&schema, &json!({"b": 3})).is_err());
        assert!(
            validate(&schema, &json!({"z": 1}))
                .unwrap_err()
                .contains("unexpected property")
        );
        assert!(
            validate(&schema, &json!({"c": 1}))
                .unwrap_err()
                .contains("oneOf")
        );
    }
}
