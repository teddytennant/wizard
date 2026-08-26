//! JSON in and out of a QuickJS value.
//!
//! Everything that crosses the plugin boundary crosses as JSON, exactly as it
//! does for Lua: a tool's arguments, a tool's answer, an event payload, a
//! service, a config slice. What is different here — and it is the reason the
//! example plugin is written in JavaScript rather than in Lua — is that the
//! round trip is *exact*. Lua has one table type, so `{}` is ambiguous between
//! the empty object and the empty array and `src/kernel/lua/host.rs` carries an
//! `object_schema` repair for the half of that it can fix and a note saying the
//! other half is unfixable. JavaScript has both, distinguishes them, and every
//! JSON document survives `serde_json -> JS -> serde_json` unchanged.
//!
//! Two deliberate asymmetries, both because JavaScript has values JSON does
//! not:
//!
//! - `undefined` becomes `null` on the way out, which is what
//!   `JSON.stringify` does to it inside an array and what a plugin returning
//!   nothing means.
//! - A function, a symbol or a `BigInt` becomes `null` rather than an error.
//!   These arrive only when a plugin puts one somewhere structured data was
//!   expected, and failing the whole call over a stray callback in a payload
//!   would turn a cosmetic mistake into a broken tool.
//!
//! Non-finite numbers are the one case that is neither: `NaN` and `Infinity`
//! have no JSON spelling, so they also become `null`, which is `JSON.stringify`
//! again.

use rquickjs::{Array, Ctx, IntoAtom, Object, Value as JsValue};
use serde_json::{Map, Number, Value};

/// How deep a value may nest before the conversion refuses.
///
/// A cycle is the reason. `const a = {}; a.self = a;` is two lines of
/// JavaScript and would otherwise walk until the stack ends, which on a
/// bounded plugin is a segfault rather than a caught error — the interrupt
/// handler cannot fire inside a Rust recursion. The limit is far past any
/// honest document (JSON.stringify's own limit in most engines is around
/// this) and the refusal names the reason.
const MAX_DEPTH: usize = 128;

/// A JS value as JSON.
pub(crate) fn js_to_json(value: &JsValue<'_>) -> rquickjs::Result<Value> {
    to_json(value, 0)
}

fn to_json(value: &JsValue<'_>, depth: usize) -> rquickjs::Result<Value> {
    if depth > MAX_DEPTH {
        return Err(rquickjs::Error::new_from_js_message(
            "value",
            "json",
            "nested deeper than 128 levels; a cycle?",
        ));
    }
    if value.is_null() || value.is_undefined() {
        return Ok(Value::Null);
    }
    if let Some(flag) = value.as_bool() {
        return Ok(Value::Bool(flag));
    }
    if let Some(int) = value.as_int() {
        return Ok(Value::Number(int.into()));
    }
    if let Some(float) = value.as_float() {
        // `Number::from_f64` is `None` for NaN and the infinities, which is
        // the same set `JSON.stringify` writes as `null`.
        return Ok(Number::from_f64(float).map_or(Value::Null, Value::Number));
    }
    if let Some(text) = value.as_string() {
        return Ok(Value::String(text.to_string()?));
    }
    if let Some(array) = value.as_array() {
        let mut items = Vec::with_capacity(array.len());
        for item in array.iter::<JsValue>() {
            items.push(to_json(&item?, depth + 1)?);
        }
        return Ok(Value::Array(items));
    }
    // Checked after `as_array`, because an array is an object.
    if let Some(object) = value.as_object() {
        // A function is an object too, and a plugin that put one in a payload
        // meant a value rather than a call.
        if object.as_function().is_some() {
            return Ok(Value::Null);
        }
        let mut map = Map::new();
        for entry in object.props::<String, JsValue>() {
            let (key, item) = entry?;
            map.insert(key, to_json(&item, depth + 1)?);
        }
        return Ok(Value::Object(map));
    }
    // Symbols and BigInts land here.
    Ok(Value::Null)
}

/// JSON as a JS value.
pub(crate) fn json_to_js<'js>(ctx: &Ctx<'js>, value: &Value) -> rquickjs::Result<JsValue<'js>> {
    match value {
        Value::Null => Ok(JsValue::new_null(ctx.clone())),
        Value::Bool(flag) => Ok(JsValue::new_bool(ctx.clone(), *flag)),
        Value::Number(number) => Ok(match number.as_i64() {
            // Integers that fit go across as integers, so `1` reaches a plugin
            // as `1` rather than as `1.0` — which matters because
            // `String(1) !== String(1.0)` is false in JavaScript but
            // `JSON.stringify` of the round trip is not what a reader expects.
            Some(int) if i32::try_from(int).is_ok() => {
                JsValue::new_int(ctx.clone(), int as i32)
            }
            _ => JsValue::new_float(ctx.clone(), number.as_f64().unwrap_or(f64::NAN)),
        }),
        Value::String(text) => Ok(rquickjs::String::from_str(ctx.clone(), text)?.into_value()),
        Value::Array(items) => {
            let array = Array::new(ctx.clone())?;
            for (index, item) in items.iter().enumerate() {
                array.set(index, json_to_js(ctx, item)?)?;
            }
            Ok(array.into_value())
        }
        Value::Object(map) => {
            let object = Object::new(ctx.clone())?;
            for (key, item) in map {
                object.set(key.as_str().into_atom(ctx)?, json_to_js(ctx, item)?)?;
            }
            Ok(object.into_value())
        }
    }
}
