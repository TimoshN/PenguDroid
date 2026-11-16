//! Built-in Java standard library and minimal Android framework.
//!
//! Replaces `core-oj.jar`, `core-libart.jar`, and `framework.jar` for core
//! classes so DEX bytecode executes without any Android ART/framework modules.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::value::{ArrayData, Heap, HeapObject, Value};

pub enum BuiltinResult {
    Handled(Value),
    LoadLibrary(String),
    NotBuiltin,
}

fn value_to_string(heap: &Heap, v: &Value) -> String {
    match v {
        Value::Void => "void".into(),
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Ref(None) => "null".into(),
        Value::Ref(Some(id)) => match heap.get(*id) {
            Some(HeapObject::StringObj(s)) => s.clone(),
            Some(HeapObject::StringBuilder(s)) => s.clone(),
            Some(HeapObject::ClassObj(c)) => format!("class {c}"),
            Some(HeapObject::Instance { class_name, fields }) => {
                if let Some(msg) = fields.get("detailMessage") {
                    let m = value_to_string(heap, msg);
                    return format!("{class_name}: {m}");
                }
                format!("{class_name}@{id:x}")
            }
            Some(HeapObject::Array { type_desc, data }) => {
                format!("{type_desc}[{}]", data.len())
            }
            Some(HeapObject::Queue(v)) => format!("ConcurrentLinkedQueue(len={})",v.len()),
            Some(HeapObject::List(v)) => format!("ArrayList(len={})", v.len()),
            Some(HeapObject::Map(m)) => format!("HashMap(len={})", m.len()),
            None => "null".into(),
        },
    }
}

pub fn builtin_interfaces(class:&str)->&'static [&'static str] {
    match class {
        "Ljava/util/HashMap;" | "Ljava/util/IdentityHashMap;"=>&["Ljava/util/Map;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/ArrayList;"=>&["Ljava/util/List;","Ljava/util/RandomAccess;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/HashSet;"=>&["Ljava/util/Set;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/concurrent/ConcurrentLinkedQueue;"=>&["Ljava/util/Queue;","Ljava/io/Serializable;"],
        "Ljava/util/List;" | "Ljava/util/Set;" | "Ljava/util/Queue;"=>&["Ljava/util/Collection;"],
        "Ljava/util/Collection;"=>&["Ljava/lang/Iterable;"],
        "Ljava/util/Deque;"=>&["Ljava/util/Queue;"],
        "Ljava/lang/Thread;"=>&["Ljava/lang/Runnable;"],
        "Ljava/lang/String;"=>&["Ljava/lang/CharSequence;","Ljava/lang/Comparable;","Ljava/io/Serializable;"],
        _=>&[],
    }
}

pub fn builtin_superclass(class_name: &str) -> Option<&'static str> {
    match class_name {
        "Ljava/util/LinkedHashMap;"=>Some("Ljava/util/HashMap;"),
        "Ljava/util/LinkedHashSet;"=>Some("Ljava/util/HashSet;"),
        "Ljava/util/HashMap;" | "Ljava/util/IdentityHashMap;"=>Some("Ljava/util/AbstractMap;"),
        "Ljava/util/ArrayList;"=>Some("Ljava/util/AbstractList;"),
        "Ljava/util/HashSet;"=>Some("Ljava/util/AbstractSet;"),
        "Ljava/util/concurrent/ConcurrentLinkedQueue;"=>Some("Ljava/util/AbstractQueue;"),
        "Ljava/util/AbstractList;" | "Ljava/util/AbstractSet;" | "Ljava/util/AbstractQueue;"=>Some("Ljava/util/AbstractCollection;"),
        "Ljava/util/AbstractCollection;" | "Ljava/util/AbstractMap;" | "Ljava/lang/Class;" | "Ljava/lang/String;"=>Some("Ljava/lang/Object;"),

        "Landroid/app/Activity;" | "Landroid/app/NativeActivity;" => Some("Landroid/view/ContextThemeWrapper;"),
        "Landroid/view/ContextThemeWrapper;" => Some("Landroid/content/ContextWrapper;"),
        "Landroid/content/ContextWrapper;" | "Landroid/app/Application;" => Some("Landroid/content/Context;"),
        "Landroid/widget/FrameLayout;" => Some("Landroid/view/ViewGroup;"),
        "Landroid/view/ViewGroup;" | "Landroid/view/SurfaceView;" => Some("Landroid/view/View;"),
        "Landroid/content/Context;" | "Landroid/view/View;" | "Ljava/lang/Thread;" | "Ljava/lang/Throwable;" => {
            Some("Ljava/lang/Object;")
        }
        "Landroid/os/HandlerThread;" => Some("Ljava/lang/Thread;"),
        "Ljava/lang/InterruptedException;" | "Ljava/lang/NoSuchMethodException;" | "Ljava/lang/NoSuchFieldException;" => Some("Ljava/lang/Exception;"),
        "Ljava/lang/IllegalMonitorStateException;" | "Ljava/lang/IllegalThreadStateException;" => Some("Ljava/lang/RuntimeException;"),
        "Ljava/lang/Exception;" => Some("Ljava/lang/Throwable;"),
        "Ljava/lang/RuntimeException;" => Some("Ljava/lang/Exception;"),
        "Ljava/lang/ClassCastException;" | "Ljava/lang/NullPointerException;"
        | "Ljava/lang/IllegalStateException;"
        | "Ljava/lang/IllegalArgumentException;"
        | "Ljava/lang/IndexOutOfBoundsException;"
        | "Ljava/lang/UnsupportedOperationException;" => Some("Ljava/lang/RuntimeException;"),
        _ => None,
    }
}

pub fn try_invoke_builtin(
    heap: &mut Heap,
    static_fields: &mut HashMap<String, Value>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    if matches!(class_name, "Ljava/lang/Boolean;" | "Ljava/lang/Byte;" | "Ljava/lang/Character;" | "Ljava/lang/Short;" | "Ljava/lang/Integer;" | "Ljava/lang/Long;" | "Ljava/lang/Float;" | "Ljava/lang/Double;") {
        if method_name == "<init>" && !descriptor.contains("Ljava/lang/String;") {
            let id = args.first().and_then(Value::as_ref).ok_or("null boxed receiver")?;
            let value = args.get(1).cloned().ok_or("missing boxed value")?;
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) { fields.insert("value".into(), value); }
            return Ok(BuiltinResult::Handled(Value::Void));
        }
        if method_name == "valueOf" && !descriptor.contains("Ljava/lang/String;") {
            let value = args.first().cloned().ok_or("missing boxed value")?;
            let id = heap.new_instance(class_name);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) { fields.insert("value".into(), value); }
            return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
        }
        if matches!(method_name, "booleanValue" | "byteValue" | "charValue" | "shortValue" | "intValue" | "longValue" | "floatValue" | "doubleValue") {
            let id = args.first().and_then(Value::as_ref).ok_or("null boxed receiver")?;
            let value = match heap.get(id) { Some(HeapObject::Instance { fields, .. }) => fields.get("value").cloned(), _ => None }.ok_or("missing boxed payload")?;
            let number = match value { Value::Int(v) => v as f64, Value::Long(v) => v as f64, Value::Float(v) => v as f64, Value::Double(v) => v, _ => return Err("invalid boxed payload".into()) };
            let integer = match value { Value::Int(v) => v, Value::Long(v) => v as i32, _ => number as i32 };
            let result = match method_name { "longValue" => if let Value::Long(_) = value { value } else { Value::Long(number as i64) }, "floatValue" => Value::Float(number as f32), "doubleValue" => Value::Double(number), "byteValue" => Value::Int(integer as i8 as i32), "shortValue" => Value::Int(integer as i16 as i32), "charValue" => Value::Int(integer as u16 as i32), _ => Value::Int(integer) };
            return Ok(BuiltinResult::Handled(result));
        }
    }
    let atomic = crate::stdlib_sync::try_invoke(heap, class_name, method_name, args)?;
    if !matches!(atomic, BuiltinResult::NotBuiltin) { return Ok(atomic); }
    let filesystem = crate::stdlib_fs::try_invoke(heap, static_fields, class_name, method_name, descriptor, args)?;
    if !matches!(filesystem, BuiltinResult::NotBuiltin) { return Ok(filesystem); }
    if class_name.starts_with('[') && method_name == "clone" {
        if let Some(id) = args.first().and_then(|v| v.as_ref()) {
            if let Some(HeapObject::Array { type_desc, data }) = heap.get(id).cloned() {
                let copy_id = heap.alloc(HeapObject::Array { type_desc, data });
                return Ok(BuiltinResult::Handled(Value::Ref(Some(copy_id))));
            }
        }
        return Ok(BuiltinResult::Handled(Value::Ref(None)));
    }
    if class_name == "Landroid/os/Bundle;" && matches!(method_name,"get" | "getString" | "getInt" | "getLong" | "getBoolean" | "containsKey") {
        let key = args.get(1).and_then(Value::as_ref).and_then(|id|heap.as_str(id)).unwrap_or("");
        let value = args.first().and_then(Value::as_ref).and_then(|id|heap.get(id)).and_then(|object|match object{HeapObject::Instance{fields,..}=>fields.get(key).cloned(),_=>None});
        if method_name=="containsKey" { return Ok(BuiltinResult::Handled(Value::Int(value.is_some() as i32))); }
        let default = args.get(2).cloned().unwrap_or_else(||match method_name{"getInt"|"getBoolean"=>Value::Int(0),"getLong"=>Value::Long(0),_=>Value::Ref(None)});
        return Ok(BuiltinResult::Handled(value.unwrap_or(default)));
    }
    match (class_name, method_name) {
        ("Landroid/content/ContextWrapper;" | "Landroid/app/Application;", "attachBaseContext") => {
            if let Some(id)=args.first().and_then(Value::as_ref) { if let Some(HeapObject::Instance{fields,..})=heap.get_mut(id){fields.insert("mBase".into(),args.get(1).cloned().unwrap_or(Value::Ref(None)));} }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/app/Application;", "registerActivityLifecycleCallbacks" | "unregisterActivityLifecycleCallbacks") => {
            let id=args.first().and_then(Value::as_ref).ok_or("null Application receiver")?;
            let callback=args.get(1).and_then(Value::as_ref).ok_or("null activity lifecycle callback")?;
            if let Some(HeapObject::Instance{fields,..})=heap.get_mut(id){
                let list=fields.entry("activityCallbacks".into()).or_insert(Value::Ref(None)).as_ref();
                let list=if let Some(list)=list {list} else {let list=heap.alloc(HeapObject::List(Vec::new()));if let Some(HeapObject::Instance{fields,..})=heap.get_mut(id){fields.insert("activityCallbacks".into(),Value::Ref(Some(list)));}list};
                if let Some(HeapObject::List(items))=heap.get_mut(list){if method_name.starts_with("unregister"){items.retain(|v|v.as_ref()!=Some(callback));}else if !items.iter().any(|v|v.as_ref()==Some(callback)){items.push(Value::Ref(Some(callback)));}}
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/ref/WeakReference;", "<init>") => {
            let referent = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("referent".into(), referent);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/ref/WeakReference;", "get") => {
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("referent").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/lang/ref/WeakReference;", "clear") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("referent".into(), Value::Ref(None));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/Enum;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let name_val = args.get(1).cloned().unwrap_or(Value::Ref(None));
                let ord_val = args.get(2).cloned().unwrap_or(Value::Int(0));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("name".into(), name_val);
                    fields.insert("ordinal".into(), ord_val);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/Enum;", "ordinal") => {
            let ord = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        fields.get("ordinal").and_then(|v| v.as_int().ok())
                    }
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(ord)))
        }
        ("Ljava/lang/Enum;", "name" | "toString") => {
            let name = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("name").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(name))
        }
        // -------------------------------------------------------------------
        // java.lang.Object
        // -------------------------------------------------------------------
        ("Ljava/lang/Object;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/lang/Object;", "getClass") => {
            let cls = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.class_name(id))
                .map(str::to_owned)
                .unwrap_or_else(|| "Ljava/lang/Object;".to_string());
            let cid = heap.class_object(&cls);
            Ok(BuiltinResult::Handled(Value::Ref(Some(cid))))
        }
        ("Ljava/lang/Object;", "hashCode") => {
            let id = args.first().and_then(|v| v.as_ref()).unwrap_or(0) as i32;
            Ok(BuiltinResult::Handled(Value::Int(id)))
        }
        ("Ljava/lang/Object;", "equals") => {
            let a = args.first().and_then(|v| v.as_ref());
            let b = args.get(1).and_then(|v| v.as_ref());
            Ok(BuiltinResult::Handled(Value::Int(i32::from(a == b))))
        }
        ("Ljava/lang/Object;", "toString") => {
            let s = args
                .first()
                .map(|v| value_to_string(heap, v))
                .unwrap_or_else(|| "null".into());
            let sid = heap.new_string(s);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }

        // -------------------------------------------------------------------
        // java.lang.String
        // -------------------------------------------------------------------
        ("Ljava/lang/String;", "<init>") => {
            let this_id = args.first().and_then(|v| v.as_ref());
            let text = if let Some(src_id) = args.get(1).and_then(|v| v.as_ref()) {
                match heap.get(src_id) {
                    Some(HeapObject::StringObj(s)) => s.clone(),
                    Some(HeapObject::Array { data: ArrayData::Byte(b), .. }) => {
                        let bytes: Vec<u8> = b.iter().map(|&x| x as u8).collect();
                        String::from_utf8_lossy(&bytes).into_owned()
                    }
                    Some(HeapObject::Array { data: ArrayData::Char(c), .. }) => {
                        String::from_utf16_lossy(c)
                    }
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            if let Some(id) = this_id {
                if let Some(slot @ HeapObject::Instance { .. }) = heap.get_mut(id) {
                    *slot = HeapObject::StringObj(text);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/String;", "length") | ("Ljava/lang/CharSequence;", "length") => {
            let len = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.utf16(id))
                .map(|s| s.len() as i32)
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }
        ("Ljava/lang/String;", "isEmpty") => {
            let empty = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .map(|s| s.is_empty())
                .unwrap_or(true);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(empty))))
        }
        ("Ljava/lang/String;", "charAt") => {
            let idx = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as usize;
            let ch = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .and_then(|s| s.encode_utf16().nth(idx))
                .unwrap_or(0) as i32;
            Ok(BuiltinResult::Handled(Value::Int(ch)))
        }
        ("Ljava/lang/String;", "equals") => {
            let a = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id));
            let b = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id));
            Ok(BuiltinResult::Handled(Value::Int(i32::from(a.is_some() && a == b))))
        }
        ("Ljava/lang/String;", "hashCode") => {
            let mut h: i32 = 0;
            if let Some(s) = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)) {
                for u in s.encode_utf16() {
                    h = h.wrapping_mul(31).wrapping_add(u as i32);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(h)))
        }
        ("Ljava/lang/String;", "concat") => {
            let a = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("").to_string();
            let b = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let sid = heap.new_string(format!("{a}{b}"));
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/String;", "toLowerCase" | "toUpperCase") => {
            let s = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("");
            let out = if method_name == "toLowerCase" {
                s.to_ascii_lowercase()
            } else {
                s.to_ascii_uppercase()
            };
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(out)))))
        }
        ("Ljava/lang/String;", "replace") => {
            let s = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("")
                .to_string();
            let out = if descriptor == "(CC)Ljava/lang/String;" {
                let old_c = char::from_u32(args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as u32).unwrap_or('\0');
                let new_c = char::from_u32(args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0) as u32).unwrap_or('\0');
                s.replace(old_c, &new_c.to_string())
            } else {
                let old_s = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("").to_string();
                let new_s = args.get(2).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("").to_string();
                if old_s.is_empty() { s } else { s.replace(&old_s, &new_s) }
            };
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(out)))))
        }
        ("Ljava/lang/String;", "substring") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let start = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let units: Vec<u16> = s.encode_utf16().collect();
            let end = args
                .get(2)
                .and_then(|v| v.as_int().ok())
                .map(|e| (e.max(0) as usize).min(units.len()))
                .unwrap_or(units.len());
            let sub = if start <= end && end <= units.len() {
                String::from_utf16_lossy(&units[start..end])
            } else {
                String::new()
            };
            let sid = heap.new_string(sub);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/String;", "compareTo") => {
            let a = args.first().and_then(Value::as_ref).and_then(|id|heap.as_str(id)).ok_or("null String receiver")?;
            let b = args.get(1).and_then(Value::as_ref).and_then(|id|heap.as_str(id)).ok_or("null String argument")?;
            let a:Vec<_>=a.encode_utf16().collect();let b:Vec<_>=b.encode_utf16().collect();
            let difference=a.iter().zip(&b).find_map(|(a,b)|(a!=b).then_some(*a as i32-*b as i32)).unwrap_or(a.len() as i32-b.len() as i32);
            Ok(BuiltinResult::Handled(Value::Int(difference)))
        }
        ("Ljava/lang/String;", "getBytes") => {
            let text = args.first().and_then(Value::as_ref).and_then(|id|heap.as_str(id)).ok_or("null String receiver")?.to_owned();
            let encoding = args.get(1).and_then(Value::as_ref).and_then(|id|heap.as_str(id)).unwrap_or("UTF-8").to_ascii_uppercase().replace('_',"-");
            let bytes = match encoding.as_str() {
                "UTF-8" | "UTF8" => text.into_bytes(),
                "US-ASCII" | "ASCII" => text.chars().map(|c|if c.is_ascii(){c as u8}else{b'?'}).collect(),
                "ISO-8859-1" | "LATIN1" => text.chars().map(|c|if c as u32<=255{c as u8}else{b'?'}).collect(),
                "UTF-16LE" => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                "UTF-16BE" => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
                _ => return Err(format!("unsupported String encoding: {encoding}")),
            };
            let array=heap.alloc(HeapObject::Array{type_desc:"[B".into(),data:ArrayData::Byte(bytes.into_iter().map(|b|b as i8).collect())});
            Ok(BuiltinResult::Handled(Value::Ref(Some(array))))
        }
        ("Ljava/lang/String;", "startsWith") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let p = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            Ok(BuiltinResult::Handled(Value::Int(i32::from(s.starts_with(p)))))
        }
        ("Ljava/lang/String;", "endsWith") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let p = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            Ok(BuiltinResult::Handled(Value::Int(i32::from(s.ends_with(p)))))
        }
        ("Ljava/lang/String;", "indexOf") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let idx = if let Some(sub) = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)) {
                s.find(sub).map(|i| i as i32).unwrap_or(-1)
            } else if let Ok(ch) = args.get(1).map(|v| v.as_int()).transpose().map(|o| o.unwrap_or(-1)) {
                char::from_u32(ch as u32)
                    .and_then(|c| s.find(c))
                    .map(|i| i as i32)
                    .unwrap_or(-1)
            } else {
                -1
            };
            Ok(BuiltinResult::Handled(Value::Int(idx)))
        }
        ("Ljava/lang/String;", "valueOf") => {
            let s = if descriptor.starts_with("(Z)") {
                if args.first().and_then(|v| v.as_int().ok()).unwrap_or(0) != 0 {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            } else if descriptor.starts_with("(C)") {
                let c = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
                char::from_u32(c).map(|ch| ch.to_string()).unwrap_or_default()
            } else {
                args.first()
                    .map(|v| value_to_string(heap, v))
                    .unwrap_or_else(|| "null".into())
            };
            let sid = heap.new_string(s);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/String;", "toString") => {
            let id = args.first().and_then(|v| v.as_ref());
            Ok(BuiltinResult::Handled(Value::Ref(id)))
        }
        ("Ljava/lang/String;", "format") => {
            let fmt = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("")
                .to_string();
            let varargs = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => Some(v.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let mut out = String::new();
            let mut arg_i = 0usize;
            let mut chars = fmt.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '%' {
                    let mut pad_zero = false;
                    let mut width: usize = 0;
                    while let Some(&ch) = chars.peek() {
                        if ch == '0' && width == 0 && !pad_zero {
                            pad_zero = true;
                            chars.next();
                        } else if ch.is_ascii_digit() {
                            width = width * 10 + (ch as usize - '0' as usize);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Some(next) = chars.next() {
                        if next == '%' {
                            out.push('%');
                            continue;
                        }
                        let val = varargs
                            .get(arg_i)
                            .and_then(|o| *o)
                            .map(|id| match heap.get(id) {
                                Some(HeapObject::Instance { class_name, fields })
                                    if class_name == "Ljava/lang/Integer;" =>
                                {
                                    let n = fields.get("value").and_then(|v| v.as_int().ok()).unwrap_or(0);
                                    let raw = match next {
                                        'x' => format!("{n:x}"),
                                        'X' => format!("{n:X}"),
                                        _ => n.to_string(),
                                    };
                                    if width > raw.len() {
                                        let pad = if pad_zero { '0' } else { ' ' };
                                        format!("{}{}", pad.to_string().repeat(width - raw.len()), raw)
                                    } else {
                                        raw
                                    }
                                }
                                _ => value_to_string(heap, &Value::Ref(Some(id))),
                            })
                            .unwrap_or_else(|| "null".into());
                        out.push_str(&val);
                        arg_i += 1;
                        continue;
                    }
                }
                out.push(c);
            }
            let sid = heap.new_string(out);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }

        // -------------------------------------------------------------------
        // java.lang.StringBuilder
        // -------------------------------------------------------------------
        ("Ljava/lang/StringBuilder;", "<init>") => {
            let init = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("")
                .to_string();
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::StringBuilder(init);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/StringBuilder;", "append") => {
            let add = if descriptor.starts_with("(Z)") {
                if args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) != 0 {
                    "true".into()
                } else {
                    "false".into()
                }
            } else if descriptor.starts_with("(C)") {
                let c = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
                char::from_u32(c).map(|ch| ch.to_string()).unwrap_or_default()
            } else {
                args.get(1)
                    .map(|v| value_to_string(heap, v))
                    .unwrap_or_else(|| "null".into())
            };
            let this_id = args.first().and_then(|v| v.as_ref());
            if let Some(id) = this_id {
                if let Some(HeapObject::StringBuilder(buf)) = heap.get_mut(id) {
                    buf.push_str(&add);
                }
            }
            Ok(BuiltinResult::Handled(Value::Ref(this_id)))
        }
        ("Ljava/lang/StringBuilder;", "toString") => {
            let text = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::StringBuilder(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let sid = heap.new_string(text);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/StringBuilder;", "length") => {
            let len = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::StringBuilder(s)) => Some(s.encode_utf16().count() as i32),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }

        // -------------------------------------------------------------------
        // java.lang.Class
        // -------------------------------------------------------------------
        ("Ljava/lang/Class;", "getName" | "getCanonicalName") => {
            let raw = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::ClassObj(c)) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Ljava/lang/Object;".into());
            let mut java_name = raw
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(&raw)
                .replace('/', ".");
            if method_name == "getCanonicalName" {
                java_name = java_name.replace('$', ".");
            }
            let sid = heap.new_string(java_name);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/Class;", "getSimpleName") => {
            let raw = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::ClassObj(c)) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Ljava/lang/Object;".into());
            let simple = raw
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(&raw)
                .rsplit('/')
                .next()
                .unwrap_or("Object")
                .rsplit('$')
                .next()
                .unwrap_or("Object")
                .to_string();
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(simple)))))
        }
        ("Ljava/lang/Class;", "getPackage") => {
            let raw = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::ClassObj(c)) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Ljava/lang/Object;".into());
            let inner = raw
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(&raw);
            let pkg_name = inner
                .rsplit_once('/')
                .map(|(p, _)| p.replace('/', "."))
                .unwrap_or_default();
            let pkg_obj = heap.new_instance("Ljava/lang/Package;");
            let pkg_str = heap.new_string(pkg_name);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(pkg_obj) {
                fields.insert("name".into(), Value::Ref(Some(pkg_str)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(pkg_obj))))
        }
        ("Ljava/lang/Package;", "getName") => {
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("name").cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string(String::new()))));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/lang/Class;", "forName") => {
            let name = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("java.lang.Object")
                .replace('.', "/");
            let desc = format!("L{name};");
            let cid = heap.class_object(&desc);
            Ok(BuiltinResult::Handled(Value::Ref(Some(cid))))
        }
        ("Ljava/lang/Class;", "getClassLoader") | ("Landroid/content/Context;", "getClassLoader") => {
            let cl = heap.new_instance("Ljava/lang/ClassLoader;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(cl))))
        }
        ("Ljava/lang/Class;", "getDeclaredConstructor" | "getConstructor") => {
            let cls_val = args.first().cloned().unwrap_or(Value::Ref(None));
            let ctor = heap.new_instance("Ljava/lang/reflect/Constructor;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(ctor) {
                fields.insert("declaringClass".into(), cls_val);
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(ctor))))
        }
        ("Ljava/lang/ClassLoader;", "findLibrary") => {
            let lib_name = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("");
            let native_dir = static_fields
                .get("PENGUDROID_NATIVE_LIB_DIR")
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("/data/local/tmp");
            let full = format!("{native_dir}/lib{lib_name}.so");
            let sid = heap.new_string(full);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }

        // -------------------------------------------------------------------
        // java.lang.System & java.io.PrintStream & java.lang.Runtime
        // -------------------------------------------------------------------
        ("Ljava/lang/System;", "currentTimeMillis") => {
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Long(ms)))
        }
        ("Ljava/lang/System;", "nanoTime") => {
            let ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Long(ns)))
        }
        ("Ljava/lang/System;", "loadLibrary")
        | ("Ljava/lang/System;", "load")
        | ("Ljava/lang/Runtime;", "loadLibrary")
        | ("Ljava/lang/Runtime;", "load") => {
            let idx = if class_name == "Ljava/lang/System;" { 0 } else { 1 };
            let lib = args
                .get(idx)
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("")
                .to_string();
            Ok(BuiltinResult::LoadLibrary(lib))
        }
        ("Ljava/lang/System;", "arraycopy") => {
            if let (
                Some(src_id),
                Ok(src_pos),
                Some(dst_id),
                Ok(dst_pos),
                Ok(len),
            ) = (
                args.first().and_then(|v| v.as_ref()),
                args.get(1).map(|v| v.as_int()).transpose().map(|o| o.unwrap_or(0)),
                args.get(2).and_then(|v| v.as_ref()),
                args.get(3).map(|v| v.as_int()).transpose().map(|o| o.unwrap_or(0)),
                args.get(4).map(|v| v.as_int()).transpose().map(|o| o.unwrap_or(0)),
            ) {
                let sp = src_pos.max(0) as usize;
                let dp = dst_pos.max(0) as usize;
                let n = len.max(0) as usize;
                let src_clone = heap.get(src_id).cloned();
                if let (Some(HeapObject::Array { data: sdata, .. }), Some(HeapObject::Array { data: ddata, .. })) =
                    (src_clone, heap.get_mut(dst_id))
                {
                    match (sdata, ddata) {
                        (ArrayData::Int(s), ArrayData::Int(d)) if sp + n <= s.len() && dp + n <= d.len() => {
                            d[dp..dp + n].copy_from_slice(&s[sp..sp + n]);
                        }
                        (ArrayData::Byte(s), ArrayData::Byte(d)) if sp + n <= s.len() && dp + n <= d.len() => {
                            d[dp..dp + n].copy_from_slice(&s[sp..sp + n]);
                        }
                        (ArrayData::Object(s), ArrayData::Object(d)) if sp + n <= s.len() && dp + n <= d.len() => {
                            d[dp..dp + n].clone_from_slice(&s[sp..sp + n]);
                        }
                        _ => {}
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/System;", "getProperty") => {
            let key = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let val = match key {
                "os.name" => Some("Linux"),
                "os.arch" => Some("aarch64"),
                "java.vm.name" => Some("PenguDroid-DexVM"),
                "java.version" => Some("17"),
                _ => None,
            };
            let ret = val.map(|s| heap.new_string(s.to_string()));
            Ok(BuiltinResult::Handled(Value::Ref(ret)))
        }
        ("Ljava/lang/Runtime;", "getRuntime") => {
            let id = static_fields
                .get("Ljava/lang/Runtime;.INSTANCE")
                .and_then(|v| v.as_ref())
                .unwrap_or_else(|| {
                    let obj = heap.new_instance("Ljava/lang/Runtime;");
                    static_fields.insert("Ljava/lang/Runtime;.INSTANCE".into(), Value::Ref(Some(obj)));
                    obj
                });
            Ok(BuiltinResult::Handled(Value::Ref(Some(id))))
        }
        ("Ljava/lang/Runtime;", "availableProcessors") => {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get() as i32)
                .unwrap_or(4);
            Ok(BuiltinResult::Handled(Value::Int(cpus)))
        }
        ("Ljava/io/PrintStream;", "println") | ("Ljava/io/PrintStream;", "print") => {
            let msg = args
                .get(1)
                .map(|v| value_to_string(heap, v))
                .unwrap_or_default();
            if method_name == "println" {
                println!("{msg}");
            } else {
                print!("{msg}");
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }

        // -------------------------------------------------------------------
        // Primitive wrappers & Math
        // -------------------------------------------------------------------
        ("Ljava/lang/Integer;", "valueOf") => {
            let val = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0);
            let id = heap.new_instance("Ljava/lang/Integer;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("value".into(), Value::Int(val));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(id))))
        }
        ("Ljava/lang/Integer;", "intValue") => {
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("value").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(val)))
        }
        ("Ljava/lang/Integer;", "parseInt") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("0");
            let v = s.trim().parse::<i32>().unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(v)))
        }
        ("Ljava/lang/Integer;", "toString") => {
            let v = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0);
            let sid = heap.new_string(v.to_string());
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/Integer;", "highestOneBit") => {
            let i = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
            let res = if i == 0 { 0 } else { 1u32 << (31 - i.leading_zeros()) };
            Ok(BuiltinResult::Handled(Value::Int(res as i32)))
        }
        ("Ljava/lang/Boolean;", "valueOf") => {
            let b = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0) != 0;
            let id = heap.new_instance("Ljava/lang/Boolean;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("value".into(), Value::Int(i32::from(b)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(id))))
        }
        ("Ljava/lang/Boolean;", "booleanValue") => {
            let b = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("value").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(b)))
        }
        ("Ljava/util/Date;", "<init>") => {
            let ms = args
                .get(1)
                .and_then(|v| v.as_long().ok())
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0)
                });
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("time".into(), Value::Long(ms));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/Date;", "getTime") => {
            let ms = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("time").and_then(|v| v.as_long().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Long(ms)))
        }
        ("Ljava/lang/System;", "getenv") => {
            let key = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("");
            let val = std::env::var(key).ok().map(|s| Value::Ref(Some(heap.new_string(s)))).unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/lang/Math;", "min") if descriptor == "(II)I" => {
            let a = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0);
            let b = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(a.min(b))))
        }
        ("Ljava/lang/Math;", "max") if descriptor == "(II)I" => {
            let a = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0);
            let b = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(a.max(b))))
        }
        ("Ljava/lang/Math;", "abs") if descriptor == "(I)I" => {
            let a = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(a.wrapping_abs())))
        }
        ("Ljava/lang/Math;", "sqrt") => {
            let a = args.first().and_then(|v| v.as_double().ok()).unwrap_or(0.0);
            Ok(BuiltinResult::Handled(Value::Double(a.sqrt())))
        }

        // -------------------------------------------------------------------
        // Exceptions
        // -------------------------------------------------------------------
        (
            "Ljava/lang/Throwable;"
            | "Ljava/lang/Exception;"
            | "Ljava/lang/RuntimeException;"
            | "Ljava/lang/IllegalArgumentException;"
            | "Ljava/lang/IllegalStateException;"
            | "Ljava/lang/NullPointerException;",
            "<init>",
        ) => {
            if let (Some(id), Some(msg_v)) = (args.first().and_then(|v| v.as_ref()), args.get(1)) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("detailMessage".into(), msg_v.clone());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        (
            "Ljava/lang/Throwable;"
            | "Ljava/lang/Exception;"
            | "Ljava/lang/RuntimeException;",
            "getMessage",
        ) => {
            let msg = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("detailMessage").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(msg))
        }

        // -------------------------------------------------------------------
        // Collections
        // -------------------------------------------------------------------
        ("Ljava/util/ArrayList;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::List(Vec::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/ArrayList;", "add") => {
            let item = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::List(list)) = heap.get_mut(id) {
                    list.push(item);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/ArrayList;", "get") => {
            let idx = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as usize;
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::List(list)) => list.get(idx).cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/util/ArrayList;", "size") => {
            let len = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::List(list)) => Some(list.len() as i32),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }
        ("Ljava/lang/Throwable;", "getStackTrace") | ("Ljava/lang/Thread;", "getStackTrace") => {
            let arr = heap.new_array("[Ljava/lang/StackTraceElement;", 0)?;
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Ljava/lang/StackTraceElement;", "getClassName" | "getMethodName" | "getFileName") => {
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(String::new())))))
        }
        ("Lkotlin/jvm/internal/Intrinsics;", "sanitizeStackTrace") => {
            Ok(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None))))
        }
        ("Lkotlin/jvm/internal/Intrinsics;", "createParameterIsNullExceptionMessage") => {
            let p = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("param");
            let msg = format!("Parameter specified as non-null is null: {p}");
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(msg)))))
        }
        ("Ljava/util/Arrays;", "copyOfRange") => {
            Ok(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None))))
        }
        ("Ljava/util/Arrays;", "asList") => {
            let items = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Array {
                        data: ArrayData::Object(v),
                        ..
                    }) => Some(v.iter().map(|o| Value::Ref(*o)).collect::<Vec<_>>()),
                    _ => None,
                })
                .unwrap_or_default();
            let list_id = heap.alloc(HeapObject::List(items));
            Ok(BuiltinResult::Handled(Value::Ref(Some(list_id))))
        }
        ("Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::List(Vec::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "add") => {
            let item = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::List(vec)) = heap.get_mut(id) {
                    vec.push(item);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "size") => {
            let len = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::List(vec)) => Some(vec.len() as i32),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "isEmpty") => {
            let empty = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::List(vec)) => Some(vec.is_empty()),
                    _ => None,
                })
                .unwrap_or(true);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(empty))))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "iterator") => {
            let iter = heap.new_instance("Ljava/util/Iterator;");
            let list_val = args.first().cloned().unwrap_or(Value::Ref(None));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(iter) {
                fields.insert("list".into(), list_val);
                fields.insert("idx".into(), Value::Int(0));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(iter))))
        }
        ("Ljava/util/Iterator;", "hasNext") => {
            let has = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        let idx = fields.get("idx").and_then(|v| v.as_int().ok()).unwrap_or(0) as usize;
                        let lid = fields.get("list").and_then(|v| v.as_ref())?;
                        match heap.get(lid) {
                            Some(HeapObject::List(vec)) => Some(idx < vec.len()),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .unwrap_or(false);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(has))))
        }
        ("Ljava/util/Iterator;", "next") => {
            let Some(id) = args.first().and_then(|v| v.as_ref()) else {
                return Ok(BuiltinResult::Handled(Value::Ref(None)));
            };
            let (lid_opt, idx) = match heap.get(id) {
                Some(HeapObject::Instance { fields, .. }) => (
                    fields.get("list").and_then(|v| v.as_ref()),
                    fields.get("idx").and_then(|v| v.as_int().ok()).unwrap_or(0) as usize,
                ),
                _ => (None, 0),
            };
            let item = lid_opt
                .and_then(|lid| match heap.get(lid) {
                    Some(HeapObject::List(vec)) => vec.get(idx).cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("idx".into(), Value::Int((idx + 1) as i32));
            }
            Ok(BuiltinResult::Handled(item))
        }
        (
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;",
            "<init>",
        ) => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::Map(HashMap::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        (
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;",
            "put",
        ) => {
            let key = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let val = args.get(2).cloned().unwrap_or(Value::Ref(None));
            let prev = if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Map(map)) = heap.get_mut(id) {
                    map.insert(key, val).unwrap_or(Value::Ref(None))
                } else {
                    Value::Ref(None)
                }
            } else {
                Value::Ref(None)
            };
            Ok(BuiltinResult::Handled(prev))
        }
        (
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;",
            "get",
        ) => {
            let key = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Map(map)) => map.get(&key).cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        (
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;",
            "size",
        ) => {
            let len = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Map(map)) => Some(map.len() as i32),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }
        (
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;",
            "isEmpty",
        ) => {
            let empty = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Map(map)) => Some(map.is_empty()),
                    _ => None,
                })
                .unwrap_or(true);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(empty))))
        }
        (
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;",
            "entrySet" | "keySet" | "values",
        ) => {
            let set_id = heap.alloc(HeapObject::List(Vec::new()));
            Ok(BuiltinResult::Handled(Value::Ref(Some(set_id))))
        }
        ("Ljava/util/Collections;", "newSetFromMap" | "emptyList" | "emptySet") => {
            let set_id = heap.alloc(HeapObject::List(Vec::new()));
            Ok(BuiltinResult::Handled(Value::Ref(Some(set_id))))
        }
        ("Ljava/util/Collections;", "emptyMap") => {
            let map_id = heap.alloc(HeapObject::Map(HashMap::new()));
            Ok(BuiltinResult::Handled(Value::Ref(Some(map_id))))
        }
        ("Ljava/util/Collections;", "singletonList") => {
            let items = args.first().cloned().into_iter().collect();
            let list_id = heap.alloc(HeapObject::List(items));
            Ok(BuiltinResult::Handled(Value::Ref(Some(list_id))))
        }

        // -------------------------------------------------------------------
        // Android core framework stubs
        // -------------------------------------------------------------------
        ("Landroid/util/Log;", "v" | "d" | "i" | "w" | "e") => {
            let tag = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("App");
            let msg = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            eprintln!("[pengu-vm:log/{method_name}] {tag}: {msg}");
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/os/SystemClock;", "uptimeMillis" | "elapsedRealtime") => {
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Long(ms)))
        }
        (
            "Landroid/app/Activity;"
            | "Landroid/app/NativeActivity;"
            | "Landroid/view/ContextThemeWrapper;"
            | "Landroid/content/ContextWrapper;"
            | "Landroid/content/Context;"
            | "Landroid/app/Application;"
            | "Landroid/os/Bundle;"
            | "Landroid/widget/FrameLayout;"
            | "Landroid/view/ViewGroup;"
            | "Landroid/view/View;"
            | "Landroid/view/SurfaceView;",
            "<init>" | "onCreate" | "onStart" | "onResume" | "onPause" | "onStop" | "onDestroy"
            | "setContentView" | "requestWindowFeature" | "start",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/lang/Thread;", "setDefaultUncaughtExceptionHandler") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/Thread;", "getDefaultUncaughtExceptionHandler") => {
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Landroid/view/View;", "requestFocus") => Ok(BuiltinResult::Handled(Value::Int(1))),
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::Queue(std::collections::VecDeque::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;", "add" | "offer") => {
            let item = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Queue(q)) = heap.get_mut(id) {
                    q.push_back(item);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;", "poll") => {
            let item = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get_mut(id) {
                    Some(HeapObject::Queue(q)) if !q.is_empty() => q.pop_front(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(item))
        }
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;", "isEmpty") => {
            let empty = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Queue(q)) => Some(q.is_empty()),
                    _ => None,
                })
                .unwrap_or(true);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(empty))))
        }
        ("Landroid/content/Context;", "getApplicationContext") => {
            let this_ref = args.first().cloned().unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(this_ref))
        }
        ("Landroid/content/Context;", "getPackageName") => {
            let pkg = static_fields
                .get("PENGUDROID_PACKAGE_NAME")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("com.pengudroid.app".into()))));
            Ok(BuiltinResult::Handled(pkg))
        }
        ("Landroid/content/Context;" | "Landroid/content/pm/PackageManager;", "getApplicationInfo") => {
            let app_info = heap.new_instance("Landroid/content/pm/ApplicationInfo;");
            let lib_dir = static_fields
                .get("PENGUDROID_NATIVE_LIB_DIR")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/tmp".into()))));
            let apk_path = static_fields
                .get("PENGUDROID_APK_PATH")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/tmp/base.apk".into()))));
            let data_dir = static_fields
                .get("PENGUDROID_DATA_DIR")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/tmp".into()))));
            let pkg = static_fields
                .get("PENGUDROID_PACKAGE_NAME")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("com.pengudroid.app".into()))));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(app_info) {
                fields.insert("metaData".into(), static_fields.get("PENGUDROID_MANIFEST_META").cloned().unwrap_or(Value::Ref(None)));
                fields.insert("nativeLibraryDir".into(), lib_dir);
                fields.insert("sourceDir".into(), apk_path.clone());
                fields.insert("publicSourceDir".into(), apk_path);
                let splits = static_fields.get("PENGUDROID_SPLIT_APKS").cloned().unwrap_or(Value::Ref(None));
                fields.insert("splitSourceDirs".into(), splits.clone());
                fields.insert("splitPublicSourceDirs".into(), splits);
                fields.insert("dataDir".into(), data_dir);
                fields.insert("packageName".into(), pkg);
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(app_info))))
        }
        ("Landroid/content/Context;", "getResources") | ("Landroid/view/View;", "getResources") => {
            let res = heap.new_instance("Landroid/content/res/Resources;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(res))))
        }
        ("Landroid/content/res/Resources;", "getConfiguration") => {
            let cfg = heap.new_instance("Landroid/content/res/Configuration;");
            let width = std::env::var("PENGUDROID_SURFACE_W").ok().and_then(|s|s.parse::<i32>().ok()).unwrap_or(1280);
            let height = std::env::var("PENGUDROID_SURFACE_H").ok().and_then(|s|s.parse::<i32>().ok()).unwrap_or(720);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(cfg) {
                fields.insert("fontScale".into(), Value::Float(1.0));
                fields.insert("orientation".into(), Value::Int(if width > height {2} else {1}));
                fields.insert("densityDpi".into(), Value::Int(320));
                fields.insert("screenWidthDp".into(), Value::Int(width / 2));
                fields.insert("screenHeightDp".into(), Value::Int(height / 2));
                fields.insert("smallestScreenWidthDp".into(), Value::Int(width.min(height) / 2));
                fields.insert("uiMode".into(), Value::Int(0x11));
                fields.insert("keyboard".into(), Value::Int(1));
                fields.insert("touchscreen".into(), Value::Int(3));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(cfg))))
        }
        ("Landroid/content/res/Resources;", "getDisplayMetrics") => {
            let dm = heap.new_instance("Landroid/util/DisplayMetrics;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(dm) {
                fields.insert("widthPixels".into(), Value::Int(1280));
                fields.insert("heightPixels".into(), Value::Int(720));
                fields.insert("density".into(), Value::Float(2.0));
                fields.insert("densityDpi".into(), Value::Int(320));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(dm))))
        }
        ("Landroid/content/Context;", "getPackageManager") => {
            let pm = heap.new_instance("Landroid/content/pm/PackageManager;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(pm))))
        }
        ("Landroid/content/pm/PackageManager;", "hasSystemFeature") => {
            let feat = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let supported = feat.contains("vulkan") || feat.contains("opengles") || feat.contains("touchscreen");
            Ok(BuiltinResult::Handled(Value::Int(i32::from(supported))))
        }
        ("Landroid/content/pm/PackageManager;", "getSystemAvailableFeatures") => {
            let arr = heap.new_array("[Landroid/content/pm/FeatureInfo;", 0)?;
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Landroid/content/pm/PackageManager;", "getPackageInfo") => {
            let pkg_val = args.get(1).cloned().unwrap_or(Value::Ref(None));
            let pi = heap.new_instance("Landroid/content/pm/PackageInfo;");
            let ver = heap.new_string("1.0".into());
            let app_info = heap.new_instance("Landroid/content/pm/ApplicationInfo;");
            let native_dir = static_fields
                .get("PENGUDROID_NATIVE_LIB_DIR")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/data/local/tmp".into()))));
            let apk_path = static_fields
                .get("PENGUDROID_APK_PATH")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/data/app/base.apk".into()))));
            let data_dir = static_fields
                .get("PENGUDROID_DATA_DIR")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/tmp".into()))));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(app_info) {
                fields.insert("nativeLibraryDir".into(), native_dir);
                fields.insert("sourceDir".into(), apk_path.clone());
                fields.insert("publicSourceDir".into(), apk_path);
                fields.insert("dataDir".into(), data_dir);
            }
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(pi) {
                fields.insert("packageName".into(), pkg_val);
                fields.insert("versionName".into(), Value::Ref(Some(ver)));
                fields.insert("versionCode".into(), Value::Int(1));
                fields.insert("applicationInfo".into(), Value::Ref(Some(app_info)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(pi))))
        }
        ("Landroid/app/AlertDialog$Builder;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        (
            "Landroid/app/AlertDialog$Builder;",
            "setTitle" | "setMessage" | "setPositiveButton" | "setNegativeButton" | "setCancelable",
        ) => {
            if std::env::var_os("PENGUDROID_TRACE_VM").is_some()
                && (method_name == "setTitle" || method_name == "setMessage")
            {
                let text = args
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|id| heap.as_str(id))
                    .unwrap_or("");
                eprintln!("[pengu-vm] AlertDialog.Builder.{method_name}({text:?})");
            }
            Ok(BuiltinResult::Handled(
                args.first().cloned().unwrap_or(Value::Ref(None)),
            ))
        }
        ("Landroid/app/AlertDialog$Builder;", "create" | "show") => {
            let dlg = heap.new_instance("Landroid/app/AlertDialog;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(dlg))))
        }
        ("Landroid/content/Context;", "getSystemService") => {
            let name = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("");
            let cls = match name {
                "window" => "Landroid/view/WindowManager;",
                "display" => "Landroid/hardware/display/DisplayManager;",
                "audio" => "Landroid/media/AudioManager;",
                "activity" => "Landroid/app/ActivityManager;",
                "input_method" => "Landroid/view/inputmethod/InputMethodManager;",
                _ => return Ok(BuiltinResult::Handled(Value::Ref(None))),
            };
            let id = heap.new_instance(cls);
            Ok(BuiltinResult::Handled(Value::Ref(Some(id))))
        }
        ("Landroid/view/WindowManager;", "getDefaultDisplay")
        | ("Landroid/hardware/display/DisplayManager;", "getDisplay") => {
            let disp = heap.new_instance("Landroid/view/Display;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(disp))))
        }
        ("Landroid/util/DisplayMetrics;", "<init>") => {
            let w = std::env::var("PENGUDROID_SURFACE_W")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(1280);
            let h = std::env::var("PENGUDROID_SURFACE_H")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(720);
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("widthPixels".into(), Value::Int(w));
                    fields.insert("heightPixels".into(), Value::Int(h));
                    fields.insert("densityDpi".into(), Value::Int(320));
                    fields.insert("density".into(), Value::Float(2.0));
                    fields.insert("scaledDensity".into(), Value::Float(2.0));
                    fields.insert("xdpi".into(), Value::Float(320.0));
                    fields.insert("ydpi".into(), Value::Float(320.0));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/view/Display;", "getRealMetrics" | "getMetrics") => {
            let w = std::env::var("PENGUDROID_SURFACE_W")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(1280);
            let h = std::env::var("PENGUDROID_SURFACE_H")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(720);
            if let Some(dm_id) = args.get(1).and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(dm_id) {
                    fields.insert("widthPixels".into(), Value::Int(w));
                    fields.insert("heightPixels".into(), Value::Int(h));
                    fields.insert("densityDpi".into(), Value::Int(320));
                    fields.insert("density".into(), Value::Float(2.0));
                    fields.insert("scaledDensity".into(), Value::Float(2.0));
                    fields.insert("xdpi".into(), Value::Float(320.0));
                    fields.insert("ydpi".into(), Value::Float(320.0));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/view/Display;", "getDisplayId" | "getRotation") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/view/Display;", "getRefreshRate") => {
            Ok(BuiltinResult::Handled(Value::Float(60.0)))
        }
        ("Landroid/view/Display;", "getPresentationDeadlineNanos") => {
            Ok(BuiltinResult::Handled(Value::Long(16_666_666)))
        }
        ("Landroid/view/Display;", "getAppVsyncOffsetNanos") => {
            Ok(BuiltinResult::Handled(Value::Long(0)))
        }
        ("Landroid/view/Display;", "isWideColorGamut" | "isHdr") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/view/Display;", "getWidth") => {
            let w = std::env::var("PENGUDROID_SURFACE_W")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(1280);
            Ok(BuiltinResult::Handled(Value::Int(w)))
        }
        ("Landroid/view/Display;", "getHeight") => {
            let h = std::env::var("PENGUDROID_SURFACE_H")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(720);
            Ok(BuiltinResult::Handled(Value::Int(h)))
        }
        ("Landroid/view/SurfaceView;", "getHolder") => {
            let view_id = args.first().and_then(|v| v.as_ref());
            let existing = view_id.and_then(|id| match heap.get(id) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("holder").and_then(|v| v.as_ref()),
                _ => None,
            });
            let holder_id = if let Some(h) = existing {
                h
            } else {
                let h = heap.new_instance("Landroid/view/SurfaceHolder;");
                let surf = heap.new_instance("Landroid/view/Surface;");
                let cb_list = heap.alloc(HeapObject::List(Vec::new()));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(h) {
                    fields.insert("surface".into(), Value::Ref(Some(surf)));
                    fields.insert("callbacks".into(), Value::Ref(Some(cb_list)));
                }
                if let Some(vid) = view_id {
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(vid) {
                        fields.insert("holder".into(), Value::Ref(Some(h)));
                    }
                }
                static_fields.insert("PENGUDROID_ACTIVE_HOLDER".into(), Value::Ref(Some(h)));
                h
            };
            Ok(BuiltinResult::Handled(Value::Ref(Some(holder_id))))
        }
        ("Landroid/view/SurfaceHolder;", "addCallback") => {
            let holder_id = args.first().and_then(|v| v.as_ref());
            let cb_val = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(hid) = holder_id {
                let cb_list_id = match heap.get(hid) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("callbacks").and_then(|v| v.as_ref()),
                    _ => None,
                };
                if let Some(lid) = cb_list_id {
                    if let Some(HeapObject::List(vec)) = heap.get_mut(lid) {
                        vec.push(cb_val);
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/view/SurfaceHolder;", "getSurface") => {
            let holder_id = args.first().and_then(|v| v.as_ref());
            let surf = holder_id
                .and_then(|hid| match heap.get(hid) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("surface").cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| Value::Ref(Some(heap.new_instance("Landroid/view/Surface;"))));
            Ok(BuiltinResult::Handled(surf))
        }
        (
            "Landroid/view/SurfaceHolder;",
            "setFormat" | "setType" | "setFixedSize" | "removeCallback" | "setSizeFromLayout" | "setKeepScreenOn",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/view/Surface;", "isValid") => Ok(BuiltinResult::Handled(Value::Int(1))),
        ("Landroid/media/AudioManager;", "getStreamVolume") => Ok(BuiltinResult::Handled(Value::Int(10))),
        ("Landroid/media/AudioManager;", "getStreamMaxVolume") => Ok(BuiltinResult::Handled(Value::Int(15))),
        ("Landroidx/arch/core/executor/ArchTaskExecutor;", "isMainThread") => {
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        (
            "Ljava/util/concurrent/Executors;",
            "newSingleThreadExecutor" | "newCachedThreadPool" | "newFixedThreadPool" | "newScheduledThreadPool",
        ) => {
            let exec = heap.new_instance("Ljava/util/concurrent/ExecutorService;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(exec))))
        }
        ("Ljava/util/concurrent/ExecutorService;", "execute") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/ExecutorService;", "submit") => {
            let fut = heap.new_instance("Ljava/util/concurrent/Future;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(fut))))
        }
        ("Ljava/util/concurrent/ExecutorService;", "isShutdown" | "isTerminated") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Ljava/util/concurrent/ExecutorService;", "shutdown" | "shutdownNow") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/atomic/AtomicBoolean;", "<init>") => {
            let val = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("value".into(), Value::Int(val));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/atomic/AtomicBoolean;", "get") => {
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        fields.get("value").and_then(|v| v.as_int().ok())
                    }
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(val)))
        }
        ("Ljava/util/concurrent/atomic/AtomicBoolean;", "set") => {
            let val = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("value".into(), Value::Int(val));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/atomic/AtomicBoolean;", "compareAndSet") => {
            let expect = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            let update = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0);
            let mut ok = false;
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    let cur = fields.get("value").and_then(|v| v.as_int().ok()).unwrap_or(0);
                    if cur == expect {
                        fields.insert("value".into(), Value::Int(update));
                        ok = true;
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(i32::from(ok))))
        }
        ("Landroid/content/Context;" | "Landroid/content/ContextWrapper;" | "Landroid/app/Activity;", "getSharedPreferences") => {
            let sp = heap.new_instance("Landroid/content/SharedPreferences;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(sp))))
        }
        ("Landroid/content/SharedPreferences;", "edit") => {
            let ed = heap.new_instance("Landroid/content/SharedPreferences$Editor;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(ed))))
        }
        (
            "Landroid/content/SharedPreferences$Editor;",
            "putString" | "putInt" | "putLong" | "putFloat" | "putBoolean" | "remove" | "clear",
        ) => {
            Ok(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None))))
        }
        ("Landroid/content/SharedPreferences$Editor;", "apply" | "commit") => {
            if let Some(Value::Ref(_)) = args.first() {
                // commit returns boolean, apply returns void; returning Int(1) is ignored by Void callers and true for boolean callers
                Ok(BuiltinResult::Handled(Value::Int(1)))
            } else {
                Ok(BuiltinResult::Handled(Value::Int(1)))
            }
        }
        ("Landroid/content/SharedPreferences;", "getString") => {
            Ok(BuiltinResult::Handled(args.get(2).cloned().unwrap_or(Value::Ref(None))))
        }
        ("Landroid/content/SharedPreferences;", "getInt" | "getBoolean") => {
            Ok(BuiltinResult::Handled(args.get(2).cloned().unwrap_or(Value::Int(0))))
        }
        ("Landroid/content/SharedPreferences;", "getLong") => {
            Ok(BuiltinResult::Handled(args.get(2).cloned().unwrap_or(Value::Long(0))))
        }
        ("Landroid/os/Handler;", "obtainMessage") => {
            let descriptor = format!("(Landroid/os/Handler;{}", descriptor.strip_prefix('(').ok_or("invalid Handler.obtainMessage descriptor")?);
            return try_invoke_builtin(heap, static_fields, "Landroid/os/Message;", "obtain", &descriptor, args);
        }
        ("Landroid/os/Message;", "obtain") => {
            let msg = heap.new_instance("Landroid/os/Message;");
            let target = args.first().cloned().unwrap_or(Value::Ref(None));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(msg) {
                fields.insert("target".into(), target);
                match descriptor {
                    "(Landroid/os/Handler;Ljava/lang/Runnable;)Landroid/os/Message;" => {
                        fields.insert("callback".into(), args.get(1).cloned().unwrap_or(Value::Ref(None)));
                    }
                    "(Landroid/os/Handler;I)Landroid/os/Message;" => {
                        fields.insert("what".into(), args.get(1).cloned().unwrap_or(Value::Int(0)));
                    }
                    "(Landroid/os/Handler;ILjava/lang/Object;)Landroid/os/Message;" => {
                        fields.insert("what".into(), args.get(1).cloned().unwrap_or(Value::Int(0)));
                        fields.insert("obj".into(), args.get(2).cloned().unwrap_or(Value::Ref(None)));
                    }
                    "(Landroid/os/Handler;III)Landroid/os/Message;" => {
                        fields.insert("what".into(), args.get(1).cloned().unwrap_or(Value::Int(0)));
                        fields.insert("arg1".into(), args.get(2).cloned().unwrap_or(Value::Int(0)));
                        fields.insert("arg2".into(), args.get(3).cloned().unwrap_or(Value::Int(0)));
                    }
                    "(Landroid/os/Handler;IIILjava/lang/Object;)Landroid/os/Message;" => {
                        fields.insert("what".into(), args.get(1).cloned().unwrap_or(Value::Int(0)));
                        fields.insert("arg1".into(), args.get(2).cloned().unwrap_or(Value::Int(0)));
                        fields.insert("arg2".into(), args.get(3).cloned().unwrap_or(Value::Int(0)));
                        fields.insert("obj".into(), args.get(4).cloned().unwrap_or(Value::Ref(None)));
                    }
                    _ => {}
                }
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(msg))))
        }
        ("Landroid/app/Activity;", "getIntent") => {
            let intent = heap.new_instance("Landroid/content/Intent;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(intent))))
        }
        ("Landroid/content/Intent;", "getStringExtra") => Ok(BuiltinResult::Handled(Value::Ref(None))),
        ("Landroid/content/Intent;", "putExtra") => {
            let this_ref = args.first().cloned().unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(this_ref))
        }
        ("Landroid/content/Context;", "getPackageCodePath" | "getPackageResourcePath") => {
            let path = static_fields
                .get("PENGUDROID_APK_PATH")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/data/app/base.apk".into()))));
            Ok(BuiltinResult::Handled(path))
        }
        (
            "Landroid/content/Context;",
            "getFilesDir" | "getCacheDir" | "getExternalFilesDir" | "getExternalCacheDir",
        ) => {
            let path = static_fields
                .get("PENGUDROID_DATA_DIR")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/tmp".into()))));
            let file = heap.new_instance("Ljava/io/File;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(file) {
                fields.insert("path".into(), path);
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(file))))
        }
        ("Landroid/content/Context;" | "Landroid/content/res/Resources;", "getAssets") => {
            let am = heap.new_instance("Landroid/content/res/AssetManager;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(am))))
        }
        ("Ljava/io/File;", "getAbsolutePath" | "getPath" | "getCanonicalPath") => {
            let path = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("path").cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/tmp".into()))));
            Ok(BuiltinResult::Handled(path))
        }
        ("Landroid/os/Bundle;", "getString") => Ok(BuiltinResult::Handled(Value::Ref(None))),
        ("Landroid/os/Bundle;", "getInt") => {
            let def = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(def)))
        }
        ("Landroid/net/Uri;", "encode") => {
            let s = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("")
                .replace(' ', "%20");
            if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
                eprintln!("[pengu-vm] Uri.encode -> {s:?}");
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))))
        }

        _ => Ok(BuiltinResult::NotBuiltin),
    }
}

/// Populate well-known static fields like `System.out`, `System.err`, `Build.VERSION.SDK_INT`.
pub fn init_builtin_statics(heap: &mut Heap, static_fields: &mut HashMap<String, Value>) {
    for (wrapper, primitive) in [("Boolean","Z"),("Byte","B"),("Character","C"),("Short","S"),("Integer","I"),("Long","J"),("Float","F"),("Double","D"),("Void","V")] {
        let id = heap.class_object(primitive);
        static_fields.insert(format!("Ljava/lang/{wrapper};.TYPE"), Value::Ref(Some(id)));
    }

    for (name,nanos) in [("NANOSECONDS",1i64),("MICROSECONDS",1000),("MILLISECONDS",1_000_000),("SECONDS",1_000_000_000),("MINUTES",60_000_000_000),("HOURS",3_600_000_000_000),("DAYS",86_400_000_000_000)] {
        let id=heap.new_instance("Ljava/util/concurrent/TimeUnit;");
        if let Some(HeapObject::Instance{fields,..})=heap.get_mut(id){fields.insert("nanos".into(),Value::Long(nanos));}
        static_fields.insert(format!("Ljava/util/concurrent/TimeUnit;.{name}"),Value::Ref(Some(id)));
    }
    let out_stream = heap.new_instance("Ljava/io/PrintStream;");
    let err_stream = heap.new_instance("Ljava/io/PrintStream;");
    static_fields.insert("Ljava/lang/System;.out".into(), Value::Ref(Some(out_stream)));
    static_fields.insert("Ljava/lang/System;.err".into(), Value::Ref(Some(err_stream)));
    static_fields.insert("Landroid/os/Build$VERSION;.SDK_INT".into(), Value::Int(37));
    let model = heap.new_string("PenguDroid".into());
    let abi = heap.new_string("arm64-v8a".into());
    static_fields.insert("Landroid/os/Build;.MODEL".into(), Value::Ref(Some(model)));
    static_fields.insert("Landroid/os/Build;.SUPPORTED_ABIS".into(), Value::Ref(Some(abi)));
}
