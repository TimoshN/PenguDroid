//! Minimal `org.json.JSONObject` and `org.json.JSONArray` implementation.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::value::{ArrayData, Heap, HeapObject, ObjId};
use crate::stdlib::{value_to_string, BuiltinResult};
use crate::Value;

#[derive(Clone, Debug)]
enum JsonVal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<JsonVal>),
    Obj(Vec<(String, JsonVal)>),
}

static JSON_STORE: OnceLock<Mutex<HashMap<ObjId, JsonVal>>> = OnceLock::new();

fn store() -> &'static Mutex<HashMap<ObjId, JsonVal>> {
    JSON_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self { bytes: s.as_bytes(), pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn parse_value(&mut self) -> Result<JsonVal, String> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonVal::Str),
            Some(b't') if self.bytes[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Ok(JsonVal::Bool(true))
            }
            Some(b'f') if self.bytes[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Ok(JsonVal::Bool(false))
            }
            Some(b'n') if self.bytes[self.pos..].starts_with(b"null") => {
                self.pos += 4;
                Ok(JsonVal::Null)
            }
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            other => Err(format!("JSONException: unexpected token {other:?} at {}", self.pos)),
        }
    }

    fn parse_object(&mut self) -> Result<JsonVal, String> {
        self.next();
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.next();
            return Ok(JsonVal::Obj(entries));
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(format!("JSONException: expected key at {}", self.pos));
            }
            let key = self.parse_string()?;
            if self.peek() != Some(b':') {
                return Err(format!("JSONException: expected ':' at {}", self.pos));
            }
            self.next();
            let val = self.parse_value()?;
            set_entry(&mut entries, key, val);
            match self.peek() {
                Some(b',') => {
                    self.next();
                }
                Some(b'}') => {
                    self.next();
                    break;
                }
                _ => return Err(format!("JSONException: expected ',' or '}}' at {}", self.pos)),
            }
        }
        Ok(JsonVal::Obj(entries))
    }

    fn parse_array(&mut self) -> Result<JsonVal, String> {
        self.next();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.next();
            return Ok(JsonVal::Arr(items));
        }
        loop {
            items.push(self.parse_value()?);
            match self.peek() {
                Some(b',') => {
                    self.next();
                }
                Some(b']') => {
                    self.next();
                    break;
                }
                _ => return Err(format!("JSONException: expected ',' or ']' at {}", self.pos)),
            }
        }
        Ok(JsonVal::Arr(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.next(); // '"'
        let mut out = String::new();
        while let Some(b) = self.next() {
            match b {
                b'"' => return Ok(out),
                b'\\' => match self.next() {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'b') => out.push('\u{0008}'),
                    Some(b'f') => out.push('\u{000C}'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        let hex = std::str::from_utf8(
                            self.bytes.get(self.pos..self.pos + 4).ok_or("JSONException: bad \\u")?,
                        )
                        .map_err(|_| "JSONException: bad \\u")?;
                        self.pos += 4;
                        let code = u16::from_str_radix(hex, 16).map_err(|_| "JSONException: bad \\u")?;
                        if let Some(ch) = char::from_u32(code as u32) {
                            out.push(ch);
                        }
                    }
                    _ => return Err("JSONException: invalid escape".into()),
                },
                _ => {
                    let start = self.pos - 1;
                    while self.pos < self.bytes.len()
                        && self.bytes[self.pos] != b'"'
                        && self.bytes[self.pos] != b'\\'
                    {
                        self.pos += 1;
                    }
                    let slice = std::str::from_utf8(&self.bytes[start..self.pos])
                        .map_err(|_| "JSONException: invalid UTF-8")?;
                    out.push_str(slice);
                }
            }
        }
        Err("JSONException: unterminated string".into())
    }

    fn parse_number(&mut self) -> Result<JsonVal, String> {
        let start = self.pos;
        let mut is_float = false;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while let Some(&b) = self.bytes.get(self.pos) {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "JSONException: invalid number")?;
        if !is_float {
            if let Ok(v) = s.parse::<i64>() {
                return Ok(JsonVal::Int(v));
            }
        }
        s.parse::<f64>()
            .map(JsonVal::Float)
            .map_err(|_| format!("JSONException: invalid number {s}"))
    }
}

fn set_entry(entries: &mut Vec<(String, JsonVal)>, key: String, val: JsonVal) {
    if let Some((_, slot)) = entries.iter_mut().find(|(k, _)| k == &key) {
        *slot = val;
    } else {
        entries.push((key, val));
    }
}

fn escape_json_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn format_json(val: &JsonVal, out: &mut String) {
    match val {
        JsonVal::Null => out.push_str("null"),
        JsonVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonVal::Int(i) => out.push_str(&i.to_string()),
        JsonVal::Float(f) => out.push_str(&f.to_string()),
        JsonVal::Str(s) => escape_json_str(s, out),
        JsonVal::Arr(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                format_json(item, out);
            }
            out.push(']');
        }
        JsonVal::Obj(entries) => {
            out.push('{');
            for (idx, (k, v)) in entries.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                escape_json_str(k, out);
                out.push(':');
                format_json(v, out);
            }
            out.push('}');
        }
    }
}

fn val_to_java(heap: &mut Heap, val: &JsonVal) -> Value {
    match val {
        JsonVal::Null => Value::Ref(None),
        JsonVal::Bool(b) => {
            let id = heap.new_instance("Ljava/lang/Boolean;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("value".into(), Value::Int(*b as i32));
            }
            Value::Ref(Some(id))
        }
        JsonVal::Int(i) => {
            if let Ok(i32v) = i32::try_from(*i) {
                let id = heap.new_instance("Ljava/lang/Integer;");
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("value".into(), Value::Int(i32v));
                }
                Value::Ref(Some(id))
            } else {
                let id = heap.new_instance("Ljava/lang/Long;");
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("value".into(), Value::Long(*i));
                }
                Value::Ref(Some(id))
            }
        }
        JsonVal::Float(f) => {
            let id = heap.new_instance("Ljava/lang/Double;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("value".into(), Value::Double(*f));
            }
            Value::Ref(Some(id))
        }
        JsonVal::Str(s) => Value::Ref(Some(heap.new_string(s.clone()))),
        JsonVal::Arr(_) => {
            let id = heap.new_instance("Lorg/json/JSONArray;");
            store().lock().unwrap().insert(id, val.clone());
            Value::Ref(Some(id))
        }
        JsonVal::Obj(_) => {
            let id = heap.new_instance("Lorg/json/JSONObject;");
            store().lock().unwrap().insert(id, val.clone());
            Value::Ref(Some(id))
        }
    }
}

fn java_to_json(heap: &Heap, val: &Value) -> JsonVal {
    match val {
        Value::Void => JsonVal::Null,
        Value::Int(i) => JsonVal::Int(*i as i64),
        Value::Long(l) => JsonVal::Int(*l),
        Value::Float(f) => JsonVal::Float(*f as f64),
        Value::Double(d) => JsonVal::Float(*d),
        Value::Ref(None) => JsonVal::Null,
        Value::Ref(Some(id)) => {
            if let Some(existing) = store().lock().unwrap().get(id).cloned() {
                return existing;
            }
            if let Some(s) = heap.as_str(*id) {
                return JsonVal::Str(s.to_string());
            }
            match heap.get(*id) {
                Some(HeapObject::Instance { class_name, fields }) => match class_name.as_str() {
                    "Ljava/lang/Boolean;" => {
                        JsonVal::Bool(fields.get("value").is_some_and(|v| v.as_int().unwrap_or(0) != 0))
                    }
                    "Ljava/lang/Integer;" | "Ljava/lang/Short;" | "Ljava/lang/Byte;" => {
                        JsonVal::Int(fields.get("value").and_then(|v| v.as_int().ok()).unwrap_or(0) as i64)
                    }
                    "Ljava/lang/Long;" => {
                        JsonVal::Int(fields.get("value").and_then(|v| v.as_long().ok()).unwrap_or(0))
                    }
                    "Ljava/lang/Float;" => {
                        let bits = fields.get("value").and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
                        JsonVal::Float(f32::from_bits(bits) as f64)
                    }
                    "Ljava/lang/Double;" => {
                        let bits = fields.get("value").and_then(|v| v.as_long().ok()).unwrap_or(0) as u64;
                        JsonVal::Float(f64::from_bits(bits))
                    }
                    "Lorg/json/JSONObject$1;" => JsonVal::Null,
                    _ => JsonVal::Str(value_to_string(heap, val)),
                },
                _ => JsonVal::Str(value_to_string(heap, val)),
            }
        }
    }
}

fn as_str(val: &JsonVal) -> Option<String> {
    match val {
        JsonVal::Null => None,
        JsonVal::Str(s) => Some(s.clone()),
        JsonVal::Bool(b) => Some(b.to_string()),
        JsonVal::Int(i) => Some(i.to_string()),
        JsonVal::Float(f) => Some(f.to_string()),
        _ => {
            let mut out = String::new();
            format_json(val, &mut out);
            Some(out)
        }
    }
}

fn as_int(val: &JsonVal) -> Option<i32> {
    match val {
        JsonVal::Int(i) => Some(*i as i32),
        JsonVal::Float(f) => Some(*f as i32),
        JsonVal::Str(s) => s.trim().parse::<i32>().ok().or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i32)),
        _ => None,
    }
}

fn as_long(val: &JsonVal) -> Option<i64> {
    match val {
        JsonVal::Int(i) => Some(*i),
        JsonVal::Float(f) => Some(*f as i64),
        JsonVal::Str(s) => s.trim().parse::<i64>().ok().or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64)),
        _ => None,
    }
}

fn as_double(val: &JsonVal) -> Option<f64> {
    match val {
        JsonVal::Int(i) => Some(*i as f64),
        JsonVal::Float(f) => Some(*f),
        JsonVal::Str(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn as_bool(val: &JsonVal) -> Option<bool> {
    match val {
        JsonVal::Bool(b) => Some(*b),
        JsonVal::Str(s) if s.eq_ignore_ascii_case("true") => Some(true),
        JsonVal::Str(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

pub(crate) fn try_invoke_json_builtin(
    heap: &mut Heap,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    if class_name == "Lorg/json/JSONObject;" {
        let this_id = args.first().and_then(|v| v.as_ref());
        match (method_name, descriptor) {
            ("<init>", "()V") => {
                if let Some(id) = this_id {
                    store().lock().unwrap().insert(id, JsonVal::Obj(Vec::new()));
                }
                return Ok(BuiltinResult::Handled(Value::Void));
            }
            ("<init>", "(Ljava/lang/String;)V") => {
                let s = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id))
                    .ok_or_else(|| "JSONException: null string".to_string())?;
                let parsed = Parser::new(s).parse_value()?;
                if !matches!(parsed, JsonVal::Obj(_)) {
                    return Err("JSONException: Value cannot be converted to JSONObject".into());
                }
                if let Some(id) = this_id {
                    store().lock().unwrap().insert(id, parsed);
                }
                return Ok(BuiltinResult::Handled(Value::Void));
            }
            ("put", _) | ("putOpt", _) => {
                let Some(id) = this_id else { return Ok(BuiltinResult::Handled(Value::Ref(None))); };
                let Some(key) = args.get(1).and_then(|v| v.as_ref()).and_then(|k| heap.as_str(k)).map(str::to_string) else {
                    return Err("JSONException: null key".into());
                };
                let jv = args.get(2).map(|v| java_to_json(heap, v)).unwrap_or(JsonVal::Null);
                let mut map = store().lock().unwrap();
                let entry = map.entry(id).or_insert_with(|| JsonVal::Obj(Vec::new()));
                if let JsonVal::Obj(entries) = entry {
                    if method_name == "putOpt" && matches!(jv, JsonVal::Null) {
                        return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
                    }
                    set_entry(entries, key, jv);
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            ("remove", "(Ljava/lang/String;)Ljava/lang/Object;") => {
                let Some(id) = this_id else { return Ok(BuiltinResult::Handled(Value::Ref(None))); };
                let Some(key) = args.get(1).and_then(|v| v.as_ref()).and_then(|k| heap.as_str(k)).map(str::to_string) else {
                    return Ok(BuiltinResult::Handled(Value::Ref(None)));
                };
                let removed = {
                    let mut map = store().lock().unwrap();
                    match map.get_mut(&id) {
                        Some(JsonVal::Obj(entries)) => entries
                            .iter()
                            .position(|(k, _)| k == &key)
                            .map(|idx| entries.remove(idx).1),
                        _ => None,
                    }
                };
                return Ok(BuiltinResult::Handled(
                    removed.map(|v| val_to_java(heap, &v)).unwrap_or(Value::Ref(None)),
                ));
            }
            ("has", "(Ljava/lang/String;)Z") => {
                let key = args.get(1).and_then(|v| v.as_ref()).and_then(|k| heap.as_str(k)).unwrap_or("");
                let has = this_id.and_then(|id| {
                    store().lock().unwrap().get(&id).and_then(|v| match v {
                        JsonVal::Obj(e) => Some(e.iter().any(|(k, _)| k == key)),
                        _ => None,
                    })
                }).unwrap_or(false);
                return Ok(BuiltinResult::Handled(Value::Int(has as i32)));
            }
            ("isNull", "(Ljava/lang/String;)Z") => {
                let key = args.get(1).and_then(|v| v.as_ref()).and_then(|k| heap.as_str(k)).unwrap_or("");
                let is_null = this_id.and_then(|id| {
                    store().lock().unwrap().get(&id).and_then(|v| match v {
                        JsonVal::Obj(e) => Some(
                            e.iter().find(|(k, _)| k == key).is_none_or(|(_, val)| matches!(val, JsonVal::Null))
                        ),
                        _ => None,
                    })
                }).unwrap_or(true);
                return Ok(BuiltinResult::Handled(Value::Int(is_null as i32)));
            }
            ("length", "()I") => {
                let len = this_id.and_then(|id| {
                    store().lock().unwrap().get(&id).and_then(|v| match v {
                        JsonVal::Obj(e) => Some(e.len() as i32),
                        _ => None,
                    })
                }).unwrap_or(0);
                return Ok(BuiltinResult::Handled(Value::Int(len)));
            }
            ("keys", "()Ljava/util/Iterator;") => {
                let keys: Vec<Value> = this_id
                    .and_then(|id| {
                        store().lock().unwrap().get(&id).and_then(|v| match v {
                            JsonVal::Obj(e) => Some(e.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()),
                            _ => None,
                        })
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .map(|k| Value::Ref(Some(heap.new_string(k))))
                    .collect();
                let iter_id = heap.new_instance("Lpengu/util/ListIterator;");
                let arr_id = heap.alloc(HeapObject::Array {
                    type_desc: "[Ljava/lang/Object;".into(),
                    data: ArrayData::Object(keys.into_iter().map(|v| v.as_ref()).collect()),
                });
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(iter_id) {
                    fields.insert("items".into(), Value::Ref(Some(arr_id)));
                    fields.insert("index".into(), Value::Int(0));
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(iter_id))));
            }
            ("toString", "()Ljava/lang/String;") => {
                let mut out = String::new();
                if let Some(id) = this_id {
                    if let Some(v) = store().lock().unwrap().get(&id) {
                        format_json(v, &mut out);
                    }
                }
                if out.is_empty() {
                    out.push_str("{}");
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(out)))));
            }
            _ => {}
        }

        let key = args.get(1).and_then(|v| v.as_ref()).and_then(|k| heap.as_str(k)).map(str::to_string);
        if let Some(key) = key {
            let found = this_id.and_then(|id| {
                store().lock().unwrap().get(&id).and_then(|v| match v {
                    JsonVal::Obj(e) => e.iter().find(|(k, _)| k == &key).map(|(_, val)| val.clone()),
                    _ => None,
                })
            });
            match method_name {
                "opt" => return Ok(BuiltinResult::Handled(found.as_ref().map(|v| val_to_java(heap, v)).unwrap_or(Value::Ref(None)))),
                "get" => {
                    let v = found.ok_or_else(|| format!("JSONException: No value for {key}"))?;
                    return Ok(BuiltinResult::Handled(val_to_java(heap, &v)));
                }
                "optString" => {
                    let fallback = args.get(2).and_then(|v| v.as_ref()).and_then(|s| heap.as_str(s)).unwrap_or("").to_string();
                    let s = found.as_ref().and_then(as_str).unwrap_or(fallback);
                    return Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))));
                }
                "getString" => {
                    let s = found.as_ref().and_then(as_str).ok_or_else(|| format!("JSONException: No value for {key}"))?;
                    return Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))));
                }
                "optInt" => {
                    let fallback = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0);
                    return Ok(BuiltinResult::Handled(Value::Int(found.as_ref().and_then(as_int).unwrap_or(fallback))));
                }
                "getInt" => {
                    let v = found.as_ref().and_then(as_int).ok_or_else(|| format!("JSONException: No value for {key}"))?;
                    return Ok(BuiltinResult::Handled(Value::Int(v)));
                }
                "optLong" => {
                    let fallback = args.get(2).and_then(|v| v.as_long().ok()).unwrap_or(0);
                    return Ok(BuiltinResult::Handled(Value::Long(found.as_ref().and_then(as_long).unwrap_or(fallback))));
                }
                "getLong" => {
                    let v = found.as_ref().and_then(as_long).ok_or_else(|| format!("JSONException: No value for {key}"))?;
                    return Ok(BuiltinResult::Handled(Value::Long(v)));
                }
                "optDouble" => {
                    let fallback = args.get(2).and_then(|v| v.as_double().ok()).unwrap_or(0.0);
                    let v = found.as_ref().and_then(as_double).unwrap_or(fallback);
                    return Ok(BuiltinResult::Handled(Value::Double(v)));
                }
                "getDouble" => {
                    let v = found.as_ref().and_then(as_double).ok_or_else(|| format!("JSONException: No value for {key}"))?;
                    return Ok(BuiltinResult::Handled(Value::Double(v)));
                }
                "optBoolean" => {
                    let fallback = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0) != 0;
                    return Ok(BuiltinResult::Handled(Value::Int(found.as_ref().and_then(as_bool).unwrap_or(fallback) as i32)));
                }
                "getBoolean" => {
                    let v = found.as_ref().and_then(as_bool).ok_or_else(|| format!("JSONException: No value for {key}"))?;
                    return Ok(BuiltinResult::Handled(Value::Int(v as i32)));
                }
                "optJSONObject" => {
                    let out = match found {
                        Some(v @ JsonVal::Obj(_)) => val_to_java(heap, &v),
                        _ => Value::Ref(None),
                    };
                    return Ok(BuiltinResult::Handled(out));
                }
                "getJSONObject" => match found {
                    Some(v @ JsonVal::Obj(_)) => return Ok(BuiltinResult::Handled(val_to_java(heap, &v))),
                    _ => return Err(format!("JSONException: No JSONObject for {key}")),
                },
                "optJSONArray" => {
                    let out = match found {
                        Some(v @ JsonVal::Arr(_)) => val_to_java(heap, &v),
                        _ => Value::Ref(None),
                    };
                    return Ok(BuiltinResult::Handled(out));
                }
                "getJSONArray" => match found {
                    Some(v @ JsonVal::Arr(_)) => return Ok(BuiltinResult::Handled(val_to_java(heap, &v))),
                    _ => return Err(format!("JSONException: No JSONArray for {key}")),
                },
                _ => {}
            }
        }
    }

    if class_name == "Lorg/json/JSONArray;" {
        let this_id = args.first().and_then(|v| v.as_ref());
        match (method_name, descriptor) {
            ("<init>", "()V") => {
                if let Some(id) = this_id {
                    store().lock().unwrap().insert(id, JsonVal::Arr(Vec::new()));
                }
                return Ok(BuiltinResult::Handled(Value::Void));
            }
            ("<init>", "(Ljava/lang/String;)V") => {
                let s = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id))
                    .ok_or_else(|| "JSONException: null string".to_string())?;
                let parsed = Parser::new(s).parse_value()?;
                if !matches!(parsed, JsonVal::Arr(_)) {
                    return Err("JSONException: Value cannot be converted to JSONArray".into());
                }
                if let Some(id) = this_id {
                    store().lock().unwrap().insert(id, parsed);
                }
                return Ok(BuiltinResult::Handled(Value::Void));
            }
            ("length", "()I") => {
                let len = this_id.and_then(|id| {
                    store().lock().unwrap().get(&id).and_then(|v| match v {
                        JsonVal::Arr(items) => Some(items.len() as i32),
                        _ => None,
                    })
                }).unwrap_or(0);
                return Ok(BuiltinResult::Handled(Value::Int(len)));
            }
            ("put", _) => {
                let Some(id) = this_id else { return Ok(BuiltinResult::Handled(Value::Ref(None))); };
                let jv = args.get(1).map(|v| java_to_json(heap, v)).unwrap_or(JsonVal::Null);
                let mut map = store().lock().unwrap();
                let entry = map.entry(id).or_insert_with(|| JsonVal::Arr(Vec::new()));
                if let JsonVal::Arr(items) = entry {
                    items.push(jv);
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            ("toString", "()Ljava/lang/String;") => {
                let mut out = String::new();
                if let Some(id) = this_id {
                    if let Some(v) = store().lock().unwrap().get(&id) {
                        format_json(v, &mut out);
                    }
                }
                if out.is_empty() {
                    out.push_str("[]");
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(out)))));
            }
            _ => {}
        }

        if let Some(idx) = args.get(1).and_then(|v| v.as_int().ok()) {
            let found = if idx >= 0 {
                this_id.and_then(|id| {
                    store().lock().unwrap().get(&id).and_then(|v| match v {
                        JsonVal::Arr(items) => items.get(idx as usize).cloned(),
                        _ => None,
                    })
                })
            } else {
                None
            };
            match method_name {
                "opt" => return Ok(BuiltinResult::Handled(found.as_ref().map(|v| val_to_java(heap, v)).unwrap_or(Value::Ref(None)))),
                "get" => {
                    let v = found.ok_or_else(|| format!("JSONException: Index {idx} out of range"))?;
                    return Ok(BuiltinResult::Handled(val_to_java(heap, &v)));
                }
                "optString" => {
                    let fallback = args.get(2).and_then(|v| v.as_ref()).and_then(|s| heap.as_str(s)).unwrap_or("").to_string();
                    let s = found.as_ref().and_then(as_str).unwrap_or(fallback);
                    return Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))));
                }
                "getString" => {
                    let s = found.as_ref().and_then(as_str).ok_or_else(|| format!("JSONException: Index {idx} out of range"))?;
                    return Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))));
                }
                "optInt" => {
                    let fallback = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0);
                    return Ok(BuiltinResult::Handled(Value::Int(found.as_ref().and_then(as_int).unwrap_or(fallback))));
                }
                "getInt" => {
                    let v = found.as_ref().and_then(as_int).ok_or_else(|| format!("JSONException: Index {idx} out of range"))?;
                    return Ok(BuiltinResult::Handled(Value::Int(v)));
                }
                "optLong" => {
                    let fallback = args.get(2).and_then(|v| v.as_long().ok()).unwrap_or(0);
                    return Ok(BuiltinResult::Handled(Value::Long(found.as_ref().and_then(as_long).unwrap_or(fallback))));
                }
                "getLong" => {
                    let v = found.as_ref().and_then(as_long).ok_or_else(|| format!("JSONException: Index {idx} out of range"))?;
                    return Ok(BuiltinResult::Handled(Value::Long(v)));
                }
                "optBoolean" => {
                    let fallback = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0) != 0;
                    return Ok(BuiltinResult::Handled(Value::Int(found.as_ref().and_then(as_bool).unwrap_or(fallback) as i32)));
                }
                "getBoolean" => {
                    let v = found.as_ref().and_then(as_bool).ok_or_else(|| format!("JSONException: Index {idx} out of range"))?;
                    return Ok(BuiltinResult::Handled(Value::Int(v as i32)));
                }
                "optJSONObject" => {
                    let out = match found {
                        Some(v @ JsonVal::Obj(_)) => val_to_java(heap, &v),
                        _ => Value::Ref(None),
                    };
                    return Ok(BuiltinResult::Handled(out));
                }
                "getJSONObject" => match found {
                    Some(v @ JsonVal::Obj(_)) => return Ok(BuiltinResult::Handled(val_to_java(heap, &v))),
                    _ => return Err(format!("JSONException: Index {idx} is not JSONObject")),
                },
                "optJSONArray" => {
                    let out = match found {
                        Some(v @ JsonVal::Arr(_)) => val_to_java(heap, &v),
                        _ => Value::Ref(None),
                    };
                    return Ok(BuiltinResult::Handled(out));
                }
                "getJSONArray" => match found {
                    Some(v @ JsonVal::Arr(_)) => return Ok(BuiltinResult::Handled(val_to_java(heap, &v))),
                    _ => return Err(format!("JSONException: Index {idx} is not JSONArray")),
                },
                _ => {}
            }
        }
    }

    Ok(BuiltinResult::NotBuiltin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_and_array_roundtrip_and_missing_key_throws_json_exception() {
        let mut heap = Heap::default();
        let obj = heap.new_instance("Lorg/json/JSONObject;");
        let raw = heap.new_string(r#"{"area":"os","nested":{"count":3,"items":["a","b"]}}"#.into());
        try_invoke_json_builtin(
            &mut heap,
            "Lorg/json/JSONObject;",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Ref(Some(obj)), Value::Ref(Some(raw))],
        )
        .unwrap();

        let area_key = heap.new_string("area".into());
        let BuiltinResult::Handled(Value::Ref(Some(area_id))) = try_invoke_json_builtin(
            &mut heap,
            "Lorg/json/JSONObject;",
            "optString",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Ref(Some(obj)), Value::Ref(Some(area_key))],
        )
        .unwrap()
        else {
            panic!("expected string");
        };
        assert_eq!(heap.as_str(area_id), Some("os"));

        let nested_key = heap.new_string("nested".into());
        let BuiltinResult::Handled(Value::Ref(Some(nested_id))) = try_invoke_json_builtin(
            &mut heap,
            "Lorg/json/JSONObject;",
            "optJSONObject",
            "(Ljava/lang/String;)Lorg/json/JSONObject;",
            &[Value::Ref(Some(obj)), Value::Ref(Some(nested_key))],
        )
        .unwrap()
        else {
            panic!("expected nested JSONObject");
        };

        let items_key = heap.new_string("items".into());
        let BuiltinResult::Handled(Value::Ref(Some(arr_id))) = try_invoke_json_builtin(
            &mut heap,
            "Lorg/json/JSONObject;",
            "optJSONArray",
            "(Ljava/lang/String;)Lorg/json/JSONArray;",
            &[Value::Ref(Some(nested_id)), Value::Ref(Some(items_key))],
        )
        .unwrap()
        else {
            panic!("expected JSONArray");
        };
        let BuiltinResult::Handled(Value::Int(len)) = try_invoke_json_builtin(
            &mut heap,
            "Lorg/json/JSONArray;",
            "length",
            "()I",
            &[Value::Ref(Some(arr_id))],
        )
        .unwrap()
        else {
            panic!("expected length");
        };
        assert_eq!(len, 2);

        let bad_obj = heap.new_instance("Lorg/json/JSONObject;");
        let empty = heap.new_string(String::new());
        let Err(err) = try_invoke_json_builtin(
            &mut heap,
            "Lorg/json/JSONObject;",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Ref(Some(bad_obj)), Value::Ref(Some(empty))],
        ) else {
            panic!("expected JSONException");
        };
        assert!(err.starts_with("JSONException:"));
    }
}
