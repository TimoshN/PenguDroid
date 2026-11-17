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

pub(crate) fn value_to_string(heap: &Heap, v: &Value) -> String {
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
        "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/IdentityHashMap;" | "Ljava/util/Properties;"=>&["Ljava/util/Map;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/concurrent/ConcurrentHashMap;"=>&["Ljava/util/concurrent/ConcurrentMap;","Ljava/util/Map;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/concurrent/ConcurrentMap;"=>&["Ljava/util/Map;"],
        "Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;" | "Ljava/util/LinkedList;"=>&["Ljava/util/List;","Ljava/util/RandomAccess;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/HashSet;" | "Ljava/util/LinkedHashSet;" | "Ljava/util/concurrent/CopyOnWriteArraySet;"=>&["Ljava/util/Set;","Ljava/lang/Cloneable;","Ljava/io/Serializable;"],
        "Ljava/util/concurrent/ConcurrentLinkedQueue;"=>&["Ljava/util/Queue;","Ljava/io/Serializable;"],
        "Ljava/util/concurrent/LinkedBlockingQueue;" | "Ljava/util/concurrent/SynchronousQueue;" | "Ljava/util/concurrent/ArrayBlockingQueue;" | "Ljava/util/concurrent/PriorityBlockingQueue;"=>&["Ljava/util/concurrent/BlockingQueue;","Ljava/util/Queue;","Ljava/util/Collection;","Ljava/io/Serializable;"],
        "Ljava/util/concurrent/BlockingQueue;"=>&["Ljava/util/Queue;"],
        "Ljava/util/concurrent/ThreadPoolExecutor$DiscardPolicy;"
        | "Ljava/util/concurrent/ThreadPoolExecutor$AbortPolicy;"
        | "Ljava/util/concurrent/ThreadPoolExecutor$CallerRunsPolicy;"
        | "Ljava/util/concurrent/ThreadPoolExecutor$DiscardOldestPolicy;" => &["Ljava/util/concurrent/RejectedExecutionHandler;"],
        "Ljava/util/concurrent/ThreadPoolExecutor;"
        | "Ljava/util/concurrent/ScheduledThreadPoolExecutor;"
        | "Ljava/util/concurrent/AbstractExecutorService;" => &["Ljava/util/concurrent/ExecutorService;", "Ljava/util/concurrent/Executor;"],
        "Ljava/util/concurrent/ExecutorService;" | "Ljava/util/concurrent/ScheduledExecutorService;" => &["Ljava/util/concurrent/Executor;"],
        "Ljava/util/List;" | "Ljava/util/Set;" | "Ljava/util/Queue;"=>&["Ljava/util/Collection;"],
        "Ljava/util/Collection;"=>&["Ljava/lang/Iterable;"],
        "Ljava/util/Deque;"=>&["Ljava/util/Queue;"],
        "Ljava/lang/Thread;"=>&["Ljava/lang/Runnable;"],
        "Ljava/lang/String;"=>&["Ljava/lang/CharSequence;","Ljava/lang/Comparable;","Ljava/io/Serializable;"],
        "Ljava/lang/Class;"=>&["Ljava/lang/reflect/Type;","Ljava/lang/reflect/GenericDeclaration;","Ljava/lang/reflect/AnnotatedElement;","Ljava/io/Serializable;"],
        "Ljava/io/InputStream;"=>&["Ljava/io/Closeable;"],
        "Ljava/io/Reader;"=>&["Ljava/io/Closeable;","Ljava/lang/Readable;"],
        "Ljava/io/Writer;"=>&["Ljava/io/Closeable;","Ljava/io/Flushable;","Ljava/lang/Appendable;"],
        "Ljava/io/Closeable;"=>&["Ljava/lang/AutoCloseable;"],
        "Lpengu/reflect/ParameterizedTypeImpl;"=>&["Ljava/lang/reflect/ParameterizedType;","Ljava/lang/reflect/Type;"],
        "Lpengu/reflect/TypeVariableImpl;"=>&["Ljava/lang/reflect/TypeVariable;","Ljava/lang/reflect/Type;"],
        "Ljava/lang/reflect/ParameterizedType;" | "Ljava/lang/reflect/WildcardType;" | "Ljava/lang/reflect/GenericArrayType;"=>&["Ljava/lang/reflect/Type;"],
        "Ljava/lang/reflect/TypeVariable;"=>&["Ljava/lang/reflect/Type;","Ljava/lang/reflect/AnnotatedElement;"],
        "Ljava/util/AbstractMap$SimpleEntry;"=>&["Ljava/util/Map$Entry;","Ljava/io/Serializable;"],
        "Landroid/app/AlertDialog;"=>&["Landroid/content/DialogInterface;"],
        _=>&[],
    }
}

pub fn is_builtin_interface(class: &str) -> bool {
    matches!(
        class,
        "Ljava/util/List;"
            | "Ljava/util/Set;"
            | "Ljava/util/Queue;"
            | "Ljava/util/concurrent/BlockingQueue;"
            | "Ljava/util/Deque;"
            | "Ljava/util/Collection;"
            | "Ljava/util/Map;"
            | "Ljava/util/Map$Entry;"
            | "Ljava/util/concurrent/ConcurrentMap;"
            | "Ljava/util/RandomAccess;"
            | "Ljava/util/Iterator;"
            | "Ljava/lang/Iterable;"
            | "Ljava/lang/Runnable;"
            | "Ljava/lang/Cloneable;"
            | "Ljava/io/Serializable;"
            | "Ljava/io/Closeable;"
            | "Ljava/io/Flushable;"
            | "Ljava/lang/AutoCloseable;"
            | "Ljava/lang/Readable;"
            | "Ljava/lang/Appendable;"
            | "Ljava/lang/CharSequence;"
            | "Ljava/lang/Comparable;"
            | "Ljava/lang/reflect/Type;"
            | "Ljava/lang/reflect/ParameterizedType;"
            | "Ljava/lang/reflect/WildcardType;"
            | "Ljava/lang/reflect/GenericArrayType;"
            | "Ljava/lang/reflect/TypeVariable;"
            | "Ljava/lang/reflect/GenericDeclaration;"
            | "Ljava/lang/reflect/AnnotatedElement;"
    )
}

pub fn builtin_superclass(class_name: &str) -> Option<&'static str> {
    match class_name {
        "Ljava/util/LinkedHashMap;" | "Ljava/util/Properties;"=>Some("Ljava/util/HashMap;"),
        "Ljava/util/LinkedHashSet;"=>Some("Ljava/util/HashSet;"),
        "Ljava/util/HashMap;" | "Ljava/util/IdentityHashMap;" | "Ljava/util/concurrent/ConcurrentHashMap;"=>Some("Ljava/util/AbstractMap;"),
        "Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;" | "Ljava/util/LinkedList;"=>Some("Ljava/util/AbstractList;"),
        "Ljava/io/ByteArrayInputStream;"=>Some("Ljava/io/InputStream;"),
        "Ljava/io/InputStreamReader;" | "Ljava/io/BufferedReader;" | "Ljava/io/StringReader;"=>Some("Ljava/io/Reader;"),
        "Ljava/io/StringWriter;"=>Some("Ljava/io/Writer;"),
        "Ljavax/net/ssl/HttpsURLConnection;"=>Some("Ljava/net/HttpURLConnection;"),
        "Ljavax/net/ssl/SSLSocketFactory;"=>Some("Ljavax/net/SocketFactory;"),
        "Ljavax/net/SocketFactory;"=>Some("Ljava/lang/Object;"),
        "Ljava/net/HttpURLConnection;"=>Some("Ljava/net/URLConnection;"),
        "Ljava/net/CookieManager;"=>Some("Ljava/net/CookieHandler;"),
        "Lpengu/internal/ConnOutputStream;"=>Some("Ljava/io/OutputStream;"),
        "Lpengu/internal/StreamChannel;"=>Some("Ljava/nio/channels/ReadableByteChannel;"),
        "Ljava/util/HashSet;" | "Ljava/util/concurrent/CopyOnWriteArraySet;"=>Some("Ljava/util/AbstractSet;"),
        "Ljava/util/concurrent/ConcurrentLinkedQueue;" | "Ljava/util/concurrent/LinkedBlockingQueue;" | "Ljava/util/concurrent/SynchronousQueue;" | "Ljava/util/concurrent/ArrayBlockingQueue;" | "Ljava/util/concurrent/PriorityBlockingQueue;"=>Some("Ljava/util/AbstractQueue;"),
        "Ljava/util/AbstractList;" | "Ljava/util/AbstractSet;" | "Ljava/util/AbstractQueue;"=>Some("Ljava/util/AbstractCollection;"),
        "Ljava/util/AbstractCollection;" | "Ljava/util/AbstractMap;" | "Ljava/lang/Class;" | "Ljava/lang/String;" | "Ljava/io/InputStream;" | "Ljava/io/Reader;" | "Ljava/io/Writer;" | "Ljava/util/Observable;" | "Ljava/util/AbstractMap$SimpleEntry;" => Some("Ljava/lang/Object;"),

        "Landroid/app/Activity;" | "Landroid/app/NativeActivity;" => Some("Landroid/view/ContextThemeWrapper;"),
        "Landroid/view/ContextThemeWrapper;" => Some("Landroid/content/ContextWrapper;"),
        "Landroid/content/ContextWrapper;" | "Landroid/app/Application;" => Some("Landroid/content/Context;"),
        "Landroid/widget/FrameLayout;" => Some("Landroid/view/ViewGroup;"),
        "Landroid/view/ViewGroup;" | "Landroid/view/SurfaceView;" => Some("Landroid/view/View;"),
        "Ljava/lang/Byte;" | "Ljava/lang/Short;" | "Ljava/lang/Integer;" | "Ljava/lang/Long;" | "Ljava/lang/Float;" | "Ljava/lang/Double;" => Some("Ljava/lang/Number;"),
        "Ljava/lang/Number;" | "Ljava/lang/Boolean;" | "Ljava/lang/Character;" => Some("Ljava/lang/Object;"),
        "Landroid/content/Context;" | "Landroid/view/View;" | "Ljava/lang/Thread;" | "Ljava/lang/Throwable;" => {
            Some("Ljava/lang/Object;")
        }
        "Landroid/os/HandlerThread;" => Some("Ljava/lang/Thread;"),
        "Ljava/io/FileNotFoundException;" => Some("Ljava/io/IOException;"),
        "Ljava/io/IOException;" | "Lorg/json/JSONException;" => Some("Ljava/lang/Exception;"),
        "Ljava/lang/InterruptedException;" | "Ljava/lang/NoSuchMethodException;" | "Ljava/lang/NoSuchFieldException;" => Some("Ljava/lang/Exception;"),
        "Ljava/lang/IllegalMonitorStateException;" | "Ljava/lang/IllegalThreadStateException;" | "Ljava/util/NoSuchElementException;" => Some("Ljava/lang/RuntimeException;"),
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
            let id = args.first().and_then(Value::as_ref).ok_or("NullPointerException: null boxed receiver")?;
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
            let id = args.first().and_then(Value::as_ref).ok_or("NullPointerException: null boxed receiver")?;
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
    let json = crate::stdlib_json::try_invoke_json_builtin(heap, class_name, method_name, descriptor, args)?;
    if !matches!(json, BuiltinResult::NotBuiltin) { return Ok(json); }
    let res = crate::resources::try_invoke(heap, static_fields, class_name, method_name, args)?;
    if !matches!(res, BuiltinResult::NotBuiltin) { return Ok(res); }
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
    if let Some(res) = crate::stdlib_regex::try_invoke(heap, class_name, method_name, args)? {
        return Ok(res);
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
            let has_slice = descriptor.starts_with("([CII)") || descriptor.starts_with("([BII)");
            let off = if has_slice {
                args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize
            } else {
                0
            };
            let count = if has_slice {
                args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize
            } else {
                usize::MAX
            };
            let text = if let Some(src_id) = args.get(1).and_then(|v| v.as_ref()) {
                match heap.get(src_id) {
                    Some(HeapObject::StringObj(s)) => s.clone(),
                    Some(HeapObject::Array { data: ArrayData::Byte(b), .. }) => {
                        let start = off.min(b.len());
                        let end = start.saturating_add(count).min(b.len());
                        let bytes: Vec<u8> = b[start..end].iter().map(|&x| x as u8).collect();
                        String::from_utf8_lossy(&bytes).into_owned()
                    }
                    Some(HeapObject::Array { data: ArrayData::Char(c), .. }) => {
                        let start = off.min(c.len());
                        let end = start.saturating_add(count).min(c.len());
                        String::from_utf16_lossy(&c[start..end])
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
        ("Ljava/lang/String;", "equalsIgnoreCase") => {
            let a = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id));
            let b = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id));
            let eq = matches!((a, b), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
            Ok(BuiltinResult::Handled(Value::Int(i32::from(eq))))
        }
        ("Ljava/lang/String;", "contains") => {
            let a = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id));
            let b = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id));
            let ok = matches!((a, b), (Some(a), Some(b)) if a.contains(b));
            Ok(BuiltinResult::Handled(Value::Int(i32::from(ok))))
        }
        ("Ljava/lang/String;", "trim") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("").trim().to_string();
            let sid = heap.new_string(s);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/lang/String;", "split") => {
            let s = args.first().and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("").to_string();
            let sep = args.get(1).and_then(|v| v.as_ref()).and_then(|id| heap.as_str(id)).unwrap_or("").to_string();
            let parts: Vec<String> = if sep.is_empty() {
                vec![s]
            } else {
                let clean = sep.trim_start_matches('\\');
                s.split(clean).map(str::to_string).collect()
            };
            let elems: Vec<Option<usize>> = parts.into_iter().map(|p| Some(heap.new_string(p))).collect();
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/lang/String;".into(),
                data: ArrayData::Object(elems),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
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
            let from = args
                .get(2)
                .and_then(|v| v.as_int().ok())
                .unwrap_or(0)
                .max(0) as usize;
            let units = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.utf16(id))
                .unwrap_or_default();
            let idx = if from >= units.len() {
                -1
            } else if let Some(sub_id) = args.get(1).and_then(|v| v.as_ref()) {
                let sub = heap.utf16(sub_id).unwrap_or_default();
                if sub.is_empty() {
                    from as i32
                } else {
                    units[from..]
                        .windows(sub.len())
                        .position(|w| w == sub.as_slice())
                        .map(|i| (from + i) as i32)
                        .unwrap_or(-1)
                }
            } else if let Ok(ch) = args.get(1).map(|v| v.as_int()).transpose().map(|o| o.unwrap_or(-1)) {
                u16::try_from(ch)
                    .ok()
                    .and_then(|target| units[from..].iter().position(|&u| u == target))
                    .map(|i| (from + i) as i32)
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
            } else if descriptor.starts_with("([CII)") {
                let off = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                let len = args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                match args.get(1).and_then(Value::as_ref).and_then(|id| heap.get(id)) {
                    Some(HeapObject::Array { data: ArrayData::Char(c), .. }) => {
                        let start = off.min(c.len());
                        let end = start.saturating_add(len).min(c.len());
                        String::from_utf16_lossy(&c[start..end])
                    }
                    _ => "null".into(),
                }
            } else if descriptor.starts_with("([C)") {
                match args.get(1).and_then(Value::as_ref).and_then(|id| heap.get(id)) {
                    Some(HeapObject::Array { data: ArrayData::Char(c), .. }) => {
                        String::from_utf16_lossy(c)
                    }
                    _ => "null".into(),
                }
            } else if descriptor.starts_with("(Ljava/lang/CharSequence;II)") {
                let off = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                let end = args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                let s = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_else(|| "null".into());
                let units: Vec<u16> = s.encode_utf16().collect();
                let start = off.min(units.len());
                let stop = end.max(start).min(units.len());
                String::from_utf16_lossy(&units[start..stop])
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
                "java.vm.name" => Some("Dalvik"),
                "java.version" => Some("0"),
                "java.specification.version" => Some("0.9"),
                "java.vm.specification.version" => Some("0.9"),
                "java.runtime.version" => Some("0.9"),
                "http.agent" => Some("Dalvik/2.1.0 (Linux; U; Android 16; PenguDroid)"),
                "line.separator" => Some("\n"),
                "file.separator" => Some("/"),
                "path.separator" => Some(":"),
                _ => None,
            };
            if let Some(s) = val {
                Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s.to_string())))))
            } else {
                Ok(BuiltinResult::Handled(args.get(1).cloned().unwrap_or(Value::Ref(None))))
            }
        }
        ("Ljava/lang/System;", "getProperties") => {
            let mut map = HashMap::new();
            for (k, v) in [
                ("os.name", "Linux"),
                ("os.arch", "aarch64"),
                ("java.vm.name", "PenguDroid-DexVM"),
                ("java.version", "17"),
            ] {
                let s = heap.new_string(v.to_string());
                map.insert(k.to_string(), Value::Ref(Some(s)));
            }
            let mid = heap.alloc(HeapObject::Map(map));
            let pid = heap.new_instance("Ljava/util/Properties;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(pid) {
                fields.insert("entries".into(), Value::Ref(Some(mid)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(pid))))
        }
        ("Ljava/util/Properties;", "<init>") => {
            let mid = heap.alloc(HeapObject::Map(HashMap::new()));
            if let Some(id) = args.first().and_then(Value::as_ref) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("entries".into(), Value::Ref(Some(mid)));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/Properties;", "clone") => {
            let cloned = args
                .first()
                .and_then(Value::as_ref)
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("entries").and_then(Value::as_ref),
                    _ => None,
                })
                .and_then(|mid| match heap.get(mid) {
                    Some(HeapObject::Map(m)) => Some(m.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let mid = heap.alloc(HeapObject::Map(cloned));
            let pid = heap.new_instance("Ljava/util/Properties;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(pid) {
                fields.insert("entries".into(), Value::Ref(Some(mid)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(pid))))
        }
        ("Ljava/util/Properties;", "getProperty") => {
            let key = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let val = args
                .first()
                .and_then(Value::as_ref)
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("entries").and_then(Value::as_ref),
                    _ => None,
                })
                .and_then(|mid| match heap.get(mid) {
                    Some(HeapObject::Map(m)) => m.get(&key).cloned(),
                    _ => None,
                })
                .or_else(|| args.get(2).cloned())
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/util/Properties;", "entrySet") => {
            let entries: Vec<Value> = args
                .first()
                .and_then(Value::as_ref)
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("entries").and_then(Value::as_ref),
                    _ => None,
                })
                .and_then(|mid| match heap.get(mid) {
                    Some(HeapObject::Map(m)) => Some(
                        m.iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| {
                    let entry = heap.new_instance("Ljava/util/AbstractMap$SimpleEntry;");
                    let ks = heap.new_string(k);
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(entry) {
                        fields.insert("key".into(), Value::Ref(Some(ks)));
                        fields.insert("value".into(), v);
                    }
                    Value::Ref(Some(entry))
                })
                .collect();
            let set = heap.alloc(HeapObject::List(entries));
            Ok(BuiltinResult::Handled(Value::Ref(Some(set))))
        }
        ("Ljava/util/Set;", "iterator") => {
            let iter = heap.new_instance("Ljava/util/Iterator;");
            if let Some(list_id) = args.first().and_then(Value::as_ref) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(iter) {
                    fields.insert("target".into(), Value::Ref(Some(list_id)));
                    fields.insert("index".into(), Value::Int(0));
                }
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(iter))))
        }
        ("Ljava/util/concurrent/CopyOnWriteArraySet;", "<init>") => {
            if let Some(id) = args.first().and_then(Value::as_ref) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::List(Vec::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/Integer;", "getInteger") => {
            let def = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            let boxed = heap.new_instance("Ljava/lang/Integer;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(boxed) {
                fields.insert("value".into(), Value::Int(def));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(boxed))))
        }
        ("Ljava/lang/StackTraceElement;", "<init>")
        | ("Ljava/lang/Error;", "<init>" | "setStackTrace") => {
            Ok(BuiltinResult::Handled(Value::Void))
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
        ("Ljava/lang/Integer;", "numberOfLeadingZeros") => {
            let i = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
            Ok(BuiltinResult::Handled(Value::Int(i.leading_zeros() as i32)))
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
        ("Ljava/lang/Math;", "round") if descriptor == "(F)I" => {
            let a = match args.first() {
                Some(Value::Float(f)) => *f,
                Some(Value::Int(bits)) => f32::from_bits(*bits as u32),
                _ => 0.0,
            };
            let r = if a.is_nan() { 0 } else { (a + 0.5).floor() as i32 };
            Ok(BuiltinResult::Handled(Value::Int(r)))
        }
        ("Ljava/lang/Math;", "round") if descriptor == "(D)J" => {
            let a = args.first().and_then(|v| v.as_double().ok()).unwrap_or(0.0);
            let r = if a.is_nan() { 0 } else { (a + 0.5).floor() as i64 };
            Ok(BuiltinResult::Handled(Value::Long(r)))
        }
        ("Landroid/media/AudioTrack;", "getMinBufferSize") => {
            Ok(BuiltinResult::Handled(Value::Int(4096)))
        }
        ("Landroid/media/AudioTrack;", "getNativeOutputSampleRate") => {
            Ok(BuiltinResult::Handled(Value::Int(48000)))
        }
        ("Landroid/media/AudioTrack;", "<init>" | "play" | "stop" | "release") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/media/AudioTrack;", "getState") => {
            Ok(BuiltinResult::Handled(Value::Int(1))) // STATE_INITIALIZED
        }
        ("Landroid/media/AudioTrack;", "write") => {
            let size = args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(size)))
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
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;", "<init>") => {
            let init_items = if descriptor == "(Ljava/util/Collection;)V" {
                args.get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|cid| match heap.get(cid) {
                        Some(HeapObject::List(src)) => Some(src.clone()),
                        Some(HeapObject::Instance { fields, .. }) => fields
                            .get("values")
                            .or_else(|| {
                                fields.values().find(|v| {
                                    v.as_ref().is_some_and(|aid| {
                                        matches!(
                                            heap.get(aid),
                                            Some(HeapObject::Array { data: ArrayData::Object(_), .. })
                                        )
                                    })
                                })
                            })
                            .and_then(|v| v.as_ref())
                            .and_then(|aid| match heap.get(aid) {
                                Some(HeapObject::Array { data: ArrayData::Object(elems), .. }) => {
                                    Some(elems.iter().map(|&o| Value::Ref(o)).collect())
                                }
                                _ => None,
                            }),
                        _ => None,
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::List(init_items);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;", "add") => {
            let item = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::List(list)) = heap.get_mut(id) {
                    list.push(item);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;", "addAll") => {
            let items: Vec<Value> = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|cid| match heap.get(cid) {
                    Some(HeapObject::List(src)) => Some(src.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let changed = !items.is_empty();
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::List(list)) = heap.get_mut(id) {
                    list.extend(items);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(i32::from(changed))))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;", "get") => {
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
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;" | "Ljava/util/LinkedList;", "set") => {
            let idx = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(-1);
            let val = args.get(2).cloned().unwrap_or(Value::Ref(None));
            let prev = if idx >= 0 {
                args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get_mut(id) {
                    Some(HeapObject::List(list)) => {
                        let u = idx as usize;
                        if u < list.len() {
                            Some(std::mem::replace(&mut list[u], val))
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
            } else {
                None
            }
            .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(prev))
        }
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;", "size") => {
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
            let from = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let to = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let sliced = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get(id) {
                Some(HeapObject::Array { type_desc, data }) => {
                    let td = type_desc.clone();
                    let new_len = to.saturating_sub(from);
                    let new_data = match data {
                        ArrayData::Byte(v) => {
                            let mut out = vec![0i8; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Byte(out)
                        }
                        ArrayData::Short(v) => {
                            let mut out = vec![0i16; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Short(out)
                        }
                        ArrayData::Char(v) => {
                            let mut out = vec![0u16; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Char(out)
                        }
                        ArrayData::Int(v) => {
                            let mut out = vec![0i32; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Int(out)
                        }
                        ArrayData::Long(v) => {
                            let mut out = vec![0i64; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Long(out)
                        }
                        ArrayData::Float(v) => {
                            let mut out = vec![0.0f32; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Float(out)
                        }
                        ArrayData::Double(v) => {
                            let mut out = vec![0.0f64; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Double(out)
                        }
                        ArrayData::Boolean(v) => {
                            let mut out = vec![false; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Boolean(out)
                        }
                        ArrayData::Object(v) => {
                            let mut out = vec![None; new_len];
                            if from < v.len() {
                                let n = new_len.min(v.len() - from);
                                out[..n].copy_from_slice(&v[from..from + n]);
                            }
                            ArrayData::Object(out)
                        }
                    };
                    Some((td, new_data))
                }
                _ => None,
            });
            if let Some((type_desc, data)) = sliced {
                let arr_id = heap.alloc(HeapObject::Array { type_desc, data });
                Ok(BuiltinResult::Handled(Value::Ref(Some(arr_id))))
            } else {
                Ok(BuiltinResult::Handled(Value::Ref(None)))
            }
        }
        ("Ljava/util/Arrays;", "hashCode") => {
            let hash = args
                .first()
                .and_then(|v| v.as_ref())
                .map_or(0, |id| id as i32);
            Ok(BuiltinResult::Handled(Value::Int(hash)))
        }
        ("Ljava/lang/ThreadLocal;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/lang/ThreadLocal;", "get") => {
            let tid_key = format!("tl:{:?}", std::thread::current().id());
            let val = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get(&tid_key).cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/lang/ThreadLocal;", "set") => {
            let tid_key = format!("tl:{:?}", std::thread::current().id());
            let val = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert(tid_key, val);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/lang/ThreadLocal;", "remove") => {
            let tid_key = format!("tl:{:?}", std::thread::current().id());
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.remove(&tid_key);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
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
        ("Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "addAll") => {
            let mut added = false;
            if let (Some(id), Some(cid)) = (args.first().and_then(|v| v.as_ref()), args.get(1).and_then(|v| v.as_ref())) {
                let src_items: Vec<(Value, String)> = match heap.get(cid) {
                    Some(HeapObject::List(v)) => v.iter().map(|i| (i.clone(), value_to_string(heap, i))).collect(),
                    _ => Vec::new(),
                };
                let existing: Vec<String> = match heap.get(id) {
                    Some(HeapObject::List(dst)) => dst.iter().map(|i| value_to_string(heap, i)).collect(),
                    _ => Vec::new(),
                };
                if let Some(HeapObject::List(dst)) = heap.get_mut(id) {
                    for (item, key) in src_items {
                        if !existing.iter().any(|k| k == &key) {
                            dst.push(item);
                            added = true;
                        }
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(i32::from(added))))
        }
        ("Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "clear") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::List(vec)) = heap.get_mut(id) {
                    vec.clear();
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        (
            "Ljava/util/LinkedHashSet;"
            | "Ljava/util/HashSet;"
            | "Ljava/util/ArrayList;"
            | "Ljava/util/concurrent/CopyOnWriteArrayList;"
            | "Ljava/util/LinkedList;"
            | "Ljava/util/List;",
            "toArray",
        ) => {
            let items: Vec<Option<usize>> = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::List(vec)) => Some(vec.iter().map(|v| v.as_ref()).collect()),
                    Some(HeapObject::Array { data: ArrayData::Object(vec), .. }) => Some(vec.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            if let Some(dst_id) = args.get(1).and_then(|v| v.as_ref()) {
                let (dst_len, dst_type) = match heap.get(dst_id) {
                    Some(HeapObject::Array { data: ArrayData::Object(v), type_desc }) => (v.len(), type_desc.clone()),
                    _ => (0, "[Ljava/lang/Object;".to_string()),
                };
                if dst_len >= items.len() {
                    if let Some(HeapObject::Array { data: ArrayData::Object(dst), .. }) = heap.get_mut(dst_id) {
                        for (i, item) in items.into_iter().enumerate() {
                            dst[i] = item;
                        }
                        if dst_len > dst.len() {}
                        return Ok(BuiltinResult::Handled(Value::Ref(Some(dst_id))));
                    }
                }
                let arr_id = heap.alloc(HeapObject::Array {
                    type_desc: dst_type,
                    data: ArrayData::Object(items),
                });
                return Ok(BuiltinResult::Handled(Value::Ref(Some(arr_id))));
            }
            let arr_id = heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/lang/Object;".into(),
                data: ArrayData::Object(items),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr_id))))
        }
        ("Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;" | "Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;", "contains") => {
            let needle = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let found = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::List(vec)) => {
                        Some(vec.iter().any(|item| value_to_string(heap, item) == needle))
                    }
                    _ => None,
                })
                .unwrap_or(false);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(found))))
        }
        ("Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "size") => {
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
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;" | "Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "isEmpty") => {
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
        ("Ljava/util/ArrayList;" | "Ljava/util/concurrent/CopyOnWriteArrayList;" | "Ljava/util/LinkedHashSet;" | "Ljava/util/HashSet;", "iterator") => {
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
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
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
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
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
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
            "putIfAbsent",
        ) => {
            let key = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let val = args.get(2).cloned().unwrap_or(Value::Ref(None));
            let prev = if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Map(map)) = heap.get_mut(id) {
                    if let Some(existing) = map.get(&key) {
                        existing.clone()
                    } else {
                        map.insert(key, val);
                        Value::Ref(None)
                    }
                } else {
                    Value::Ref(None)
                }
            } else {
                Value::Ref(None)
            };
            Ok(BuiltinResult::Handled(prev))
        }
        (
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
            "putAll",
        ) => {
            let entries: Vec<(String, Value)> = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|src_id| match heap.get(src_id) {
                    Some(HeapObject::Map(src)) => Some(src.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                    _ => None,
                })
                .unwrap_or_default();
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Map(dst)) = heap.get_mut(id) {
                    for (k, v) in entries {
                        dst.insert(k, v);
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        (
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
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
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
            "containsKey",
        ) => {
            let key = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let has = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Map(map)) => Some(map.contains_key(&key)),
                    _ => None,
                })
                .unwrap_or(false);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(has))))
        }
        (
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
            "remove",
        ) => {
            let key = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            let prev = if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Map(map)) = heap.get_mut(id) {
                    map.remove(&key).unwrap_or(Value::Ref(None))
                } else {
                    Value::Ref(None)
                }
            } else {
                Value::Ref(None)
            };
            Ok(BuiltinResult::Handled(prev))
        }
        (
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
            "clear",
        ) => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Map(map)) = heap.get_mut(id) {
                    map.clear();
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        (
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
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
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
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
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;",
            "entrySet" | "keySet" | "values",
        ) => {
            let pairs: Vec<(String, Value)> = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Map(map)) => {
                        Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let items = match method_name {
                "values" => pairs.into_iter().map(|(_, v)| v).collect(),
                "keySet" => pairs
                    .into_iter()
                    .map(|(k, _)| Value::Ref(Some(heap.new_string(k))))
                    .collect(),
                _ => pairs
                    .into_iter()
                    .map(|(k, v)| {
                        let k_id = heap.new_string(k);
                        let e_id = heap.new_instance("Ljava/util/AbstractMap$SimpleEntry;");
                        if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(e_id) {
                            fields.insert("key".into(), Value::Ref(Some(k_id)));
                            fields.insert("value".into(), v);
                        }
                        Value::Ref(Some(e_id))
                    })
                    .collect(),
            };
            let set_id = heap.alloc(HeapObject::List(items));
            Ok(BuiltinResult::Handled(Value::Ref(Some(set_id))))
        }
        ("Ljava/util/Map$Entry;" | "Ljava/util/AbstractMap$SimpleEntry;", "getKey") => {
            let k = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("key").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(k))
        }
        ("Ljava/util/Map$Entry;" | "Ljava/util/AbstractMap$SimpleEntry;", "getValue") => {
            let v = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("value").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(v))
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
        (
            "Ljava/util/Collections;",
            "unmodifiableList"
            | "unmodifiableSet"
            | "unmodifiableMap"
            | "unmodifiableCollection"
            | "synchronizedList"
            | "synchronizedSet"
            | "synchronizedMap",
        ) => Ok(BuiltinResult::Handled(
            args.first().cloned().unwrap_or(Value::Ref(None)),
        )),

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
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;" | "Ljava/util/concurrent/LinkedBlockingQueue;" | "Ljava/util/concurrent/SynchronousQueue;" | "Ljava/util/concurrent/ArrayBlockingQueue;" | "Ljava/util/concurrent/PriorityBlockingQueue;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::Queue(std::collections::VecDeque::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;" | "Ljava/util/concurrent/LinkedBlockingQueue;" | "Ljava/util/concurrent/SynchronousQueue;" | "Ljava/util/concurrent/ArrayBlockingQueue;" | "Ljava/util/concurrent/PriorityBlockingQueue;", "add" | "offer") => {
            let item = args.get(1).cloned().unwrap_or(Value::Ref(None));
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Queue(q)) = heap.get_mut(id) {
                    q.push_back(item);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;" | "Ljava/util/concurrent/LinkedBlockingQueue;" | "Ljava/util/concurrent/SynchronousQueue;" | "Ljava/util/concurrent/ArrayBlockingQueue;" | "Ljava/util/concurrent/PriorityBlockingQueue;", "poll") => {
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
        ("Ljava/util/concurrent/ConcurrentLinkedQueue;" | "Ljava/util/concurrent/LinkedBlockingQueue;" | "Ljava/util/concurrent/SynchronousQueue;" | "Ljava/util/concurrent/ArrayBlockingQueue;" | "Ljava/util/concurrent/PriorityBlockingQueue;", "isEmpty") => {
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
            "setTitle"
            | "setMessage"
            | "setPositiveButton"
            | "setNegativeButton"
            | "setNeutralButton"
            | "setCancelable"
            | "setView"
            | "setOnCancelListener"
            | "setOnDismissListener"
            | "setIcon",
        ) => {
            if matches!(method_name, "setTitle" | "setMessage" | "setPositiveButton" | "setNegativeButton") {
                let text = args
                    .get(1)
                    .map(|v| value_to_string(heap, v))
                    .unwrap_or_default();
                eprintln!("[pengu-vm] AlertDialog.Builder.{method_name}({text:?})");
            }
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    match method_name {
                        "setTitle" => {
                            if let Some(v) = args.get(1) {
                                fields.insert("title".into(), v.clone());
                            }
                        }
                        "setMessage" => {
                            if let Some(v) = args.get(1) {
                                fields.insert("message".into(), v.clone());
                            }
                        }
                        "setPositiveButton" => {
                            if let Some(v) = args.get(2) {
                                fields.insert("positiveListener".into(), v.clone());
                            }
                        }
                        "setNegativeButton" => {
                            if let Some(v) = args.get(2) {
                                fields.insert("negativeListener".into(), v.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(BuiltinResult::Handled(
                args.first().cloned().unwrap_or(Value::Ref(None)),
            ))
        }
        ("Landroid/app/AlertDialog$Builder;", "create") => {
            let copied = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => Some(fields.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let dlg = heap.new_instance("Landroid/app/AlertDialog;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(dlg) {
                for (k, v) in copied {
                    fields.insert(k, v);
                }
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(dlg))))
        }
        ("Landroid/app/AlertDialog;", "dismiss" | "cancel") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/widget/CheckBox;" | "Landroid/widget/CompoundButton;", "<init>" | "setOnCheckedChangeListener" | "setText") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/widget/CheckBox;" | "Landroid/widget/CompoundButton;", "setChecked") => {
            let checked = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("checked".into(), Value::Int(checked));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/widget/CheckBox;" | "Landroid/widget/CompoundButton;", "isChecked") => {
            let checked = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("checked").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(checked)))
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
        ("Ljava/util/concurrent/Executors;", "defaultThreadFactory") => {
            let tf = heap.new_instance("Ljava/util/concurrent/ThreadFactory;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(tf))))
        }
        ("Ljava/util/concurrent/Executors;", "unconfigurableExecutorService") => {
            let exec = args
                .first()
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_instance("Ljava/util/concurrent/ExecutorService;"))));
            Ok(BuiltinResult::Handled(exec))
        }
        (
            "Ljava/util/concurrent/ThreadPoolExecutor;" | "Ljava/util/concurrent/ScheduledThreadPoolExecutor;",
            "<init>" | "allowCoreThreadTimeOut" | "setKeepAliveTime",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        (
            "Ljava/util/concurrent/ThreadPoolExecutor$DiscardPolicy;"
            | "Ljava/util/concurrent/ThreadPoolExecutor$AbortPolicy;"
            | "Ljava/util/concurrent/ThreadPoolExecutor$CallerRunsPolicy;"
            | "Ljava/util/concurrent/ThreadPoolExecutor$DiscardOldestPolicy;",
            "<init>" | "rejectedExecution",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        (
            "Ljava/util/concurrent/ExecutorService;"
            | "Ljava/util/concurrent/ThreadPoolExecutor;"
            | "Ljava/util/concurrent/ScheduledThreadPoolExecutor;",
            "execute",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        (
            "Ljava/util/concurrent/ExecutorService;"
            | "Ljava/util/concurrent/ThreadPoolExecutor;"
            | "Ljava/util/concurrent/ScheduledThreadPoolExecutor;",
            "submit",
        ) => {
            let fut = heap.new_instance("Ljava/util/concurrent/Future;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(fut))))
        }
        (
            "Ljava/util/concurrent/ExecutorService;"
            | "Ljava/util/concurrent/ThreadPoolExecutor;"
            | "Ljava/util/concurrent/ScheduledThreadPoolExecutor;",
            "isShutdown" | "isTerminated",
        ) => Ok(BuiltinResult::Handled(Value::Int(0))),
        (
            "Ljava/util/concurrent/ExecutorService;"
            | "Ljava/util/concurrent/ThreadPoolExecutor;"
            | "Ljava/util/concurrent/ScheduledThreadPoolExecutor;",
            "shutdown" | "shutdownNow",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/content/Intent;", "hasExtra") => Ok(BuiltinResult::Handled(Value::Int(0))),
        ("Landroid/content/Intent;", "getBooleanExtra") => {
            let default = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(default)))
        }
        ("Landroid/content/res/Resources$Theme;", "obtainStyledAttributes") => {
            let ta = heap.new_instance("Landroid/content/res/TypedArray;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(ta))))
        }
        ("Landroid/content/res/TypedArray;", "getBoolean") => {
            let default = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(default)))
        }
        ("Landroid/content/res/TypedArray;", "recycle") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/app/Activity;", "getOnBackInvokedDispatcher") => {
            let d = heap.new_instance("Landroid/window/OnBackInvokedDispatcher;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(d))))
        }
        ("Landroid/window/OnBackInvokedDispatcher;", "registerOnBackInvokedCallback" | "unregisterOnBackInvokedCallback") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/app/Activity;", "getComponentName") => {
            let cn = heap.new_instance("Landroid/content/ComponentName;");
            let pkg_str = std::env::var("PENGUDROID_PACKAGE_NAME").unwrap_or_default();
            let pkg = heap.new_string(pkg_str);
            let cls = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { class_name, .. }) => Some(
                        class_name
                            .strip_prefix('L')
                            .unwrap_or(class_name)
                            .strip_suffix(';')
                            .unwrap_or(class_name)
                            .replace('/', "."),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            let cls_sid = heap.new_string(cls);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(cn) {
                fields.insert("mPackage".into(), Value::Ref(Some(pkg)));
                fields.insert("mClass".into(), Value::Ref(Some(cls_sid)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(cn))))
        }
        ("Landroid/content/pm/PackageManager;", "getActivityInfo") => {
            let info = heap.new_instance("Landroid/content/pm/ActivityInfo;");
            let pkg_str = std::env::var("PENGUDROID_PACKAGE_NAME").unwrap_or_default();
            let pkg = heap.new_string(pkg_str);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(info) {
                fields.insert("packageName".into(), Value::Ref(Some(pkg)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(info))))
        }
        ("Landroid/app/NotificationManager;", "cancelAll") => Ok(BuiltinResult::Handled(Value::Void)),
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
        ("Landroid/content/Context;" | "Landroid/content/ContextWrapper;" | "Landroid/app/Activity;", "getSharedPreferences" | "getPreferences") => {
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
        ("Landroid/content/SharedPreferences;", "contains") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/net/ConnectivityManager;", "getActiveNetworkInfo") => {
            let ni = heap.new_instance("Landroid/net/NetworkInfo;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(ni))))
        }
        ("Landroid/net/ConnectivityManager;", "getActiveNetwork") => {
            let net = heap.new_instance("Landroid/net/Network;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(net))))
        }
        ("Landroid/net/ConnectivityManager;", "getNetworkCapabilities") => {
            let nc = heap.new_instance("Landroid/net/NetworkCapabilities;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(nc))))
        }
        ("Landroid/net/ConnectivityManager;", "unregisterNetworkCallback") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/net/ConnectivityManager$NetworkCallback;", "<init>" | "onAvailable" | "onCapabilitiesChanged" | "onLost") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/media/AudioManager;", "getDevices") => {
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Landroid/media/AudioDeviceInfo;".into(),
                data: ArrayData::Object(Vec::new()),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Landroid/media/AudioManager;", "getProperty") => Ok(BuiltinResult::Handled(Value::Ref(None))),
        ("Landroid/media/AudioManager;", "isBluetoothA2dpOn" | "isBluetoothScoOn" | "isMusicActive") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/view/InputDevice;", "getDeviceIds") => {
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[I".into(),
                data: ArrayData::Int(Vec::new()),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Landroid/view/Window;", "getAttributes") => {
            let lp = heap.new_instance("Landroid/view/WindowManager$LayoutParams;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(lp))))
        }
        ("Landroid/view/View;", "setOnSystemUiVisibilityChangeListener") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/hardware/display/DisplayManager;", "registerDisplayListener" | "unregisterDisplayListener") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/content/ContentResolver;", "query") => {
            let c = heap.new_instance("Landroid/database/MatrixCursor;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(c))))
        }
        ("Landroid/database/MatrixCursor;", "moveToFirst" | "moveToNext") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/database/MatrixCursor;", "getCount") => Ok(BuiltinResult::Handled(Value::Int(0))),
        ("Landroid/database/MatrixCursor;", "close") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/content/ContentResolver;", "registerContentObserver" | "unregisterContentObserver") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/database/ContentObserver;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/content/pm/PackageManager;", "queryIntentActivities") => {
            let list = heap.new_instance("Ljava/util/ArrayList;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(list))))
        }
        ("Landroid/content/Intent;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let action = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("action".into(), action);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/content/Intent;", "addCategory" | "addFlags" | "setPackage" | "setData") => {
            Ok(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None))))
        }
        ("Landroid/content/Intent;", "getData" | "getExtras") => {
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Landroid/net/Uri;", "parse") => {
            let uri = heap.new_instance("Landroid/net/Uri;");
            let s = args.first().cloned().unwrap_or(Value::Ref(None));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(uri) {
                fields.insert("uri".into(), s);
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(uri))))
        }
        ("Landroid/content/res/Resources$Theme;", "resolveAttribute") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/util/TypedValue;", "<init>") | ("Landroid/graphics/Rect;", "<init>") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/telephony/PhoneStateListener;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/webkit/WebView;", "setDataDirectorySuffix") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/util/Log;", "getStackTraceString") => {
            let s = args.first().map(|v| value_to_string(heap, v)).unwrap_or_default();
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))))
        }
        ("Landroid/os/Environment;", "getExternalStorageDirectory") => {
            let dir = static_fields
                .get("PENGUDROID_DATA_DIR")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("/sdcard".into()))));
            let file = heap.new_instance("Ljava/io/File;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(file) {
                fields.insert("path".into(), dir);
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(file))))
        }
        ("Ljava/io/EOFException;" | "Ljava/lang/NoSuchFieldError;" | "Ljava/lang/NoSuchMethodError;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let msg = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("detailMessage".into(), msg);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/util/Base64;", "encodeToString") => {
            const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let bytes: Vec<u8> = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Array { data: ArrayData::Byte(b), .. }) => {
                        Some(b.iter().map(|&x| x as u8).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
            for chunk in bytes.chunks(3) {
                let b0 = chunk[0] as usize;
                let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
                let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
                out.push(TBL[b0 >> 2] as char);
                out.push(TBL[((b0 & 3) << 4) | (b1 >> 4)] as char);
                out.push(if chunk.len() > 1 { TBL[((b1 & 0xf) << 2) | (b2 >> 6)] as char } else { '=' });
                out.push(if chunk.len() > 2 { TBL[b2 & 0x3f] as char } else { '=' });
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(out)))))
        }
        ("Ljavax/crypto/spec/SecretKeySpec;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljavax/crypto/Cipher;", "getInstance") => {
            let c = heap.new_instance("Ljavax/crypto/Cipher;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(c))))
        }
        ("Ljavax/crypto/Cipher;", "init") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljavax/crypto/Cipher;", "doFinal") => {
            let data = args.get(1).cloned().unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(data))
        }
        ("Ljava/lang/reflect/Array;", "newInstance") => {
            let elem = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::ClassObj(c)) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Ljava/lang/Object;".into());
            let len = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let type_desc = format!("[{elem}");
            let data = match elem.as_str() {
                "Z" | "B" => ArrayData::Byte(vec![0; len]),
                "C" | "S" | "I" => ArrayData::Int(vec![0; len]),
                "J" => ArrayData::Long(vec![0; len]),
                "F" => ArrayData::Float(vec![0.0; len]),
                "D" => ArrayData::Double(vec![0.0; len]),
                _ => ArrayData::Object(vec![None; len]),
            };
            let arr = heap.alloc(HeapObject::Array { type_desc, data });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Ljava/lang/reflect/Array;", "getLength") => {
            let len = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Array { data, .. }) => Some(data.len() as i32),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }
        ("Ljava/lang/reflect/Array;", "get") => {
            let idx = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(-1);
            let out = if idx >= 0 {
                args.first()
                    .and_then(|v| v.as_ref())
                    .and_then(|id| match heap.get(id) {
                        Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => {
                            v.get(idx as usize).copied().flatten().map(|o| Value::Ref(Some(o)))
                        }
                        _ => None,
                    })
                    .unwrap_or(Value::Ref(None))
            } else {
                Value::Ref(None)
            };
            Ok(BuiltinResult::Handled(out))
        }
        ("Ljava/lang/NoSuchMethodException;", "printStackTrace") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/net/NetworkCapabilities;", "hasTransport" | "hasCapability") => {
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Landroid/net/NetworkInfo;", "isConnected" | "isConnectedOrConnecting" | "isAvailable") => {
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Landroid/net/NetworkInfo;", "getType") => {
            Ok(BuiltinResult::Handled(Value::Int(1))) // TYPE_WIFI
        }
        ("Landroid/net/NetworkInfo;", "getTypeName") => {
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string("WIFI".into())))))
        }
        ("Landroid/net/NetworkInfo;", "getSubtype") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/net/NetworkInfo;", "getSubtypeName") => {
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(String::new())))))
        }
        ("Ljava/util/Locale;", "getDefault") => {
            let loc = heap.new_instance("Ljava/util/Locale;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(loc))))
        }
        ("Ljava/util/Locale;", "getLanguage") => {
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string("en".into())))))
        }
        ("Ljava/util/Locale;", "getCountry") => {
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string("US".into())))))
        }
        ("Ljava/util/Locale;", "toString" | "toLanguageTag") => {
            let tag = if method_name == "toLanguageTag" { "en-US" } else { "en_US" };
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(tag.into())))))
        }
        ("Ljava/util/UUID;", "randomUUID") => {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
                ^ (n.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let s = format!(
                "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
                (t >> 32) as u32,
                (t >> 16) as u16,
                (t >> 4) as u16 & 0x0fff,
                n as u16 & 0x0fff,
                (t ^ n) & 0xffff_ffff_ffff
            );
            let sid = heap.new_string(s);
            let u = heap.new_instance("Ljava/util/UUID;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(u) {
                fields.insert("str".into(), Value::Ref(Some(sid)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(u))))
        }
        ("Ljava/util/UUID;", "toString") => {
            let s = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("str").cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("00000000-0000-4000-8000-000000000000".into()))));
            Ok(BuiltinResult::Handled(s))
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
        ("Landroid/app/Activity;", "getWindow") => {
            let win = heap.new_instance("Landroid/view/Window;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(win))))
        }
        ("Landroid/app/Activity;", "isFinishing" | "isDestroyed" | "isInMultiWindowMode") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/app/Activity;", "getRequestedOrientation") => {
            let orient = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("requestedOrientation").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(orient)))
        }
        ("Landroid/app/Activity;", "setRequestedOrientation") => {
            if let (Some(id), Ok(orient)) = (
                args.first().and_then(|v| v.as_ref()),
                args.get(1).map(|v| v.as_int()).transpose().map(|o| o.unwrap_or(0)),
            ) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("requestedOrientation".into(), Value::Int(orient));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/app/Activity;", "findViewById" | "getCallingPackage") => {
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Landroid/content/Context;", "getContentResolver") => {
            let cr = heap.new_instance("Landroid/content/ContentResolver;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(cr))))
        }
        ("Landroid/content/Context;", "checkCallingOrSelfPermission" | "checkSelfPermission") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/content/Context;", "getTheme") => {
            let theme = heap.new_instance("Landroid/content/res/Resources$Theme;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(theme))))
        }
        ("Landroid/app/Application;", "getProcessName") => {
            let pkg = static_fields
                .get("PENGUDROID_PACKAGE")
                .cloned()
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("com.pengudroid.app".into()))));
            Ok(BuiltinResult::Handled(pkg))
        }
        ("Landroid/os/Process;", "setThreadPriority") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ldalvik/system/VMRuntime;", "getRuntime") => {
            let rt = heap.new_instance("Ldalvik/system/VMRuntime;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(rt))))
        }
        ("Ldalvik/system/VMRuntime;", "setHiddenApiExemptions") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/app/Activity;", "onWindowFocusChanged") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/view/Window;", "getDecorView") => {
            let v = heap.new_instance("Landroid/view/View;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(v))))
        }
        ("Landroid/view/Window;", "setFlags") => Ok(BuiltinResult::Handled(Value::Void)),
        (
            "Landroid/view/View;",
            "setSystemUiVisibility" | "setContentDescription" | "setFocusable" | "setFocusableInTouchMode" | "setId",
        ) => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/view/View;", "getSystemUiVisibility") => Ok(BuiltinResult::Handled(Value::Int(0))),
        ("Landroid/text/TextUtils;", "equals") => {
            let a = args.first().map(|v| value_to_string(heap, v));
            let b = args.get(1).map(|v| value_to_string(heap, v));
            let same = match (args.first(), args.get(1)) {
                (Some(Value::Ref(None)), Some(Value::Ref(None))) => true,
                (Some(Value::Ref(None)), _) | (_, Some(Value::Ref(None))) => false,
                _ => a == b,
            };
            Ok(BuiltinResult::Handled(Value::Int(same as i32)))
        }
        ("Landroid/text/TextUtils;", "isEmpty") => {
            let empty = match args.first() {
                Some(Value::Ref(Some(id))) => value_to_string(heap, &Value::Ref(Some(*id))).is_empty(),
                _ => true,
            };
            Ok(BuiltinResult::Handled(Value::Int(empty as i32)))
        }
        ("Ljava/net/URLEncoder;", "encode") => {
            let input = args.first().map(|v| value_to_string(heap, v)).unwrap_or_default();
            let mut out = String::with_capacity(input.len());
            for b in input.as_bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(*b as char),
                    b' ' => out.push('+'),
                    _ => out.push_str(&format!("%{:02X}", b)),
                }
            }
            let sid = heap.new_string(out);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Lcom/google/android/play/core/assetpacks/AssetPackStates;", "packStates") => {
            let map = heap.new_instance("Ljava/util/HashMap;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(map))))
        }
        ("Lcom/google/android/play/core/assetpacks/AssetPackStates;", "totalBytes") => {
            Ok(BuiltinResult::Handled(Value::Long(0)))
        }
        ("Landroid/content/SharedPreferences;", "getAll") => {
            let map = heap.new_instance("Ljava/util/HashMap;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(map))))
        }
        ("Lcom/unity3d/player/UnityPlayer;", "addView" | "removeView" | "bringChildToFront") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Lcom/unity3d/player/o;", "forceLayout") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Landroid/view/View;" | "Lcom/unity3d/player/o;", "getContext") => {
            let ctx = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("context").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(ctx))
        }
        ("Ljava/util/TimeZone;", "getTimeZone") => {
            let tz = heap.new_instance("Ljava/util/TimeZone;");
            let id_val = args.first().cloned().unwrap_or(Value::Ref(None));
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(tz) {
                fields.insert("id".into(), id_val);
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(tz))))
        }
        ("Ljava/text/SimpleDateFormat;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let pat = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("pattern".into(), pat);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/text/SimpleDateFormat;", "setTimeZone") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let tz = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("timeZone".into(), tz);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/text/SimpleDateFormat;", "format") => {
            let s = heap.new_string("2026-11-17T00:00:00Z".to_string());
            Ok(BuiltinResult::Handled(Value::Ref(Some(s))))
        }
        ("Ljava/io/StringWriter;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.entry("buf".into()).or_insert_with(|| Value::Ref(None));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/io/StringWriter;", "append") => {
            let chunk = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let cur = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("buf").map(|v| value_to_string(heap, v)).unwrap_or_default(),
                    _ => String::new(),
                };
                let next = heap.new_string(format!("{cur}{chunk}"));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("buf".into(), Value::Ref(Some(next)));
                }
            }
            Ok(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None))))
        }
        ("Ljava/io/StringWriter;", "write") => {
            let chunk = args.get(1).map(|v| value_to_string(heap, v)).unwrap_or_default();
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let cur = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("buf").map(|v| value_to_string(heap, v)).unwrap_or_default(),
                    _ => String::new(),
                };
                let next = heap.new_string(format!("{cur}{chunk}"));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("buf".into(), Value::Ref(Some(next)));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/io/StringWriter;", "toString") => {
            let out = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("buf").map(|v| value_to_string(heap, v)),
                    _ => None,
                })
                .unwrap_or_default();
            let sid = heap.new_string(out);
            Ok(BuiltinResult::Handled(Value::Ref(Some(sid))))
        }
        ("Ljava/util/Scanner;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let text = if let Some(src_id) = args.get(1).and_then(|v| v.as_ref()) {
                    if let Some(s) = heap.as_str(src_id) {
                        s.to_string()
                    } else if let Some(HeapObject::Instance { fields, .. }) = heap.get(src_id) {
                        if let Some(buf_id) = fields.get("buf").and_then(|v| v.as_ref()) {
                            let pos = fields.get("pos").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                            match heap.get(buf_id) {
                                Some(HeapObject::Array { data: ArrayData::Byte(b), .. }) => {
                                    let bytes: Vec<u8> = b.get(pos..).unwrap_or(&[]).iter().map(|&x| x as u8).collect();
                                    String::from_utf8_lossy(&bytes).into_owned()
                                }
                                _ => String::new(),
                            }
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let s_id = heap.new_string(text);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("text".into(), Value::Ref(Some(s_id)));
                    fields.insert("pos".into(), Value::Int(0));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/Scanner;", "useDelimiter") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let delim = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("delim".into(), delim);
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Ljava/util/Scanner;", "hasNext" | "hasNextLine") => {
            let has = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        let pos = fields.get("pos").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                        let s = fields.get("text").and_then(|v| v.as_ref()).and_then(|sid| heap.as_str(sid)).unwrap_or("");
                        Some(pos < s.len())
                    }
                    _ => None,
                })
                .unwrap_or(false);
            Ok(BuiltinResult::Handled(Value::Int(has as i32)))
        }
        ("Ljava/util/Scanner;", "next" | "nextLine") => {
            let (text, pos, delim) = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => Some((
                        fields.get("text").and_then(|v| v.as_ref()).and_then(|sid| heap.as_str(sid)).unwrap_or("").to_string(),
                        fields.get("pos").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                        fields.get("delim").and_then(|v| v.as_ref()).and_then(|sid| heap.as_str(sid)).map(str::to_string),
                    )),
                    _ => None,
                })
                .unwrap_or_default();
            if pos >= text.len() {
                return Err("NoSuchElementException: Scanner exhausted".into());
            }
            let rest = &text[pos..];
            let token = if delim.as_deref() == Some("\\A") {
                if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("pos".into(), Value::Int(text.len() as i32));
                    }
                }
                rest.to_string()
            } else {
                let end = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
                if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("pos".into(), Value::Int((pos + end) as i32));
                    }
                }
                rest[..end].trim_end_matches(&['\r', '\n'][..]).to_string()
            };
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(token)))))
        }
        ("Ljava/util/Scanner;", "close") => Ok(BuiltinResult::Handled(Value::Void)),
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

        ("Ljava/nio/ByteOrder;", "nativeOrder") => {
            let bo = heap.new_instance("Ljava/nio/ByteOrder;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(bo))))
        }
        ("Ljava/nio/ByteBuffer;", "allocateDirect") => {
            let cap = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let id = heap.new_instance("Ljava/nio/ByteBuffer;");
            crate::jni_host::alloc_direct_buffer(id, cap);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("capacity".into(), Value::Int(cap as i32));
                fields.insert("limit".into(), Value::Int(cap as i32));
                fields.insert("position".into(), Value::Int(0));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(id))))
        }
        ("Ljava/nio/ByteBuffer;", "allocate") => {
            let cap = args.first().and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[B".into(),
                data: ArrayData::Byte(vec![0; cap]),
            });
            let id = heap.new_instance("Ljava/nio/ByteBuffer;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("capacity".into(), Value::Int(cap as i32));
                fields.insert("limit".into(), Value::Int(cap as i32));
                fields.insert("position".into(), Value::Int(0));
                fields.insert("backing".into(), Value::Ref(Some(arr)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(id))))
        }
        ("Ljava/nio/ByteBuffer;", "capacity") => {
            let cap = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("capacity").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(cap)))
        }
        ("Ljava/nio/ByteBuffer;", "limit") => {
            if let Some(new_lim) = args.get(1).and_then(|v| v.as_int().ok()) {
                if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("limit".into(), Value::Int(new_lim));
                    }
                    return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
                }
            }
            let lim = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("limit").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(lim)))
        }
        ("Ljava/nio/ByteBuffer;", "position") => {
            if let Some(new_pos) = args.get(1).and_then(|v| v.as_int().ok()) {
                if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("position".into(), Value::Int(new_pos));
                    }
                    return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
                }
            }
            let pos = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("position").and_then(|v| v.as_int().ok()),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(pos)))
        }
        ("Ljava/nio/ByteBuffer;", "remaining") => {
            let rem = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        let pos = fields.get("position").and_then(|v| v.as_int().ok()).unwrap_or(0);
                        let lim = fields.get("limit").and_then(|v| v.as_int().ok()).unwrap_or(0);
                        Some((lim - pos).max(0))
                    }
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(rem)))
        }
        ("Ljava/nio/ByteBuffer;", "clear" | "rewind") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("position".into(), Value::Int(0));
                    if method_name == "clear" {
                        let cap = fields.get("capacity").cloned().unwrap_or(Value::Int(0));
                        fields.insert("limit".into(), cap);
                    }
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Ljava/nio/ByteBuffer;", "flip") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    let pos = fields.get("position").cloned().unwrap_or(Value::Int(0));
                    fields.insert("limit".into(), pos);
                    fields.insert("position".into(), Value::Int(0));
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Ljava/nio/ByteBuffer;", "order") => {
            Ok(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None))))
        }
        ("Ljava/nio/ByteBuffer;", "isDirect") => {
            let direct = args
                .first()
                .and_then(|v| v.as_ref())
                .is_some_and(|id| crate::jni_host::direct_buffer_slice_mut(id, |_| ()).is_some());
            Ok(BuiltinResult::Handled(Value::Int(direct as i32)))
        }
        ("Ljava/nio/ByteBuffer;", "array") => {
            let id = args.first().and_then(|v| v.as_ref());
            let backing = id.and_then(|bid| match heap.get(bid) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("backing").cloned(),
                _ => None,
            });
            if let Some(val @ Value::Ref(Some(_))) = backing {
                return Ok(BuiltinResult::Handled(val));
            }
            if let Some(bid) = id {
                let cap = match heap.get(bid) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("capacity").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    _ => 0,
                };
                let arr = heap.alloc(HeapObject::Array {
                    type_desc: "[B".into(),
                    data: ArrayData::Byte(vec![0; cap]),
                });
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(bid) {
                    fields.insert("backing".into(), Value::Ref(Some(arr)));
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(arr))));
            }
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Ljava/nio/ByteBuffer;", "arrayOffset") => Ok(BuiltinResult::Handled(Value::Int(0))),
        ("Ljava/net/CookieHandler;", "getDefault") => {
            let cur = static_fields.get("java.net.CookieHandler.default").cloned().unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(cur))
        }
        ("Ljava/net/CookieHandler;", "setDefault") => {
            let v = args.first().cloned().unwrap_or(Value::Ref(None));
            static_fields.insert("java.net.CookieHandler.default".into(), v);
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/CookieManager;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/net/URL;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let url_str = args.get(1).and_then(|v| v.as_ref()).and_then(|s| heap.as_str(s)).unwrap_or("").to_string();
                let proto_str = url_str.split(':').next().unwrap_or("http").to_string();
                let u_sid = heap.new_string(url_str);
                let p_sid = heap.new_string(proto_str);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("url".into(), Value::Ref(Some(u_sid)));
                    fields.insert("protocol".into(), Value::Ref(Some(p_sid)));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/URL;", "getProtocol") => {
            let p = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("protocol").cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| Value::Ref(Some(heap.new_string("https".into()))));
            Ok(BuiltinResult::Handled(p))
        }
        ("Ljava/net/URL;", "toString") => {
            let u = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("url").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(u))
        }
        ("Ljava/net/URL;", "openConnection") => {
            let (url_val, proto) = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        let u = fields.get("url").cloned().unwrap_or(Value::Ref(None));
                        let p = fields
                            .get("protocol")
                            .and_then(|v| v.as_ref())
                            .and_then(|pid| heap.as_str(pid))
                            .unwrap_or("https")
                            .to_string();
                        Some((u, p))
                    }
                    _ => None,
                })
                .unwrap_or((Value::Ref(None), "https".to_string()));
            let cls = if proto.eq_ignore_ascii_case("https") {
                "Ljavax/net/ssl/HttpsURLConnection;"
            } else {
                "Ljava/net/HttpURLConnection;"
            };
            let conn = heap.new_instance(cls);
            let req_headers = heap.alloc(HeapObject::Map(HashMap::new()));
            let method_sid = heap.new_string("GET".into());
            let body_arr = heap.alloc(HeapObject::Array {
                type_desc: "[B".into(),
                data: ArrayData::Byte(Vec::new()),
            });
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(conn) {
                fields.insert("url".into(), url_val);
                fields.insert("method".into(), Value::Ref(Some(method_sid)));
                fields.insert("reqHeaders".into(), Value::Ref(Some(req_headers)));
                fields.insert("reqBody".into(), Value::Ref(Some(body_arr)));
                fields.insert("connectTimeout".into(), Value::Int(15000));
                fields.insert("readTimeout".into(), Value::Int(15000));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(conn))))
        }
        ("Ljava/net/URLConnection;" | "Ljava/net/HttpURLConnection;", "setConnectTimeout") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let ms = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(15000);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("connectTimeout".into(), Value::Int(ms));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/URLConnection;" | "Ljava/net/HttpURLConnection;", "setReadTimeout") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let ms = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(15000);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("readTimeout".into(), Value::Int(ms));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/URLConnection;" | "Ljava/net/HttpURLConnection;", "setDoOutput" | "setDoInput" | "setUseCaches" | "setInstanceFollowRedirects") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/HttpURLConnection;", "setRequestMethod") => {
            if let (Some(id), Some(m)) = (
                args.first().and_then(|v| v.as_ref()),
                args.get(1).cloned(),
            ) {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("method".into(), m);
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/URLConnection;" | "Ljava/net/HttpURLConnection;", "addRequestProperty" | "setRequestProperty") => {
            if let (Some(id), Some(k), Some(v)) = (
                args.first().and_then(|v| v.as_ref()),
                args.get(1).and_then(|v| v.as_ref()).and_then(|kid| heap.as_str(kid)).map(str::to_string),
                args.get(2).and_then(|v| v.as_ref()).and_then(|vid| heap.as_str(vid)).map(str::to_string),
            ) {
                let map_id = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("reqHeaders").and_then(|v| v.as_ref()),
                    _ => None,
                };
                if let Some(mid) = map_id {
                    let vs = heap.new_string(v);
                    if let Some(HeapObject::Map(m)) = heap.get_mut(mid) {
                        m.insert(k, Value::Ref(Some(vs)));
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/net/URLConnection;" | "Ljava/net/HttpURLConnection;", "getOutputStream") => {
            let conn_id = args.first().and_then(|v| v.as_ref());
            let os = heap.new_instance("Lpengu/internal/ConnOutputStream;");
            if let Some(cid) = conn_id {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(os) {
                    fields.insert("conn".into(), Value::Ref(Some(cid)));
                }
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(os))))
        }
        ("Ljava/io/OutputStream;", "write") => {
            let os_id = args.first().and_then(|v| v.as_ref());
            let conn_id = os_id.and_then(|oid| match heap.get(oid) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("conn").and_then(|v| v.as_ref()),
                _ => None,
            });
            let mut chunk: Vec<i8> = Vec::new();
            if args.len() == 2 {
                if let Ok(b) = args[1].as_int() {
                    chunk.push(b as i8);
                } else if let Some(arr_id) = args[1].as_ref() {
                    if let Some(HeapObject::Array { data: ArrayData::Byte(bytes), .. }) = heap.get(arr_id) {
                        chunk.extend_from_slice(bytes);
                    }
                }
            } else if args.len() >= 4 {
                if let Some(arr_id) = args[1].as_ref() {
                    let off = args[2].as_int().unwrap_or(0).max(0) as usize;
                    let len = args[3].as_int().unwrap_or(0).max(0) as usize;
                    if let Some(HeapObject::Array { data: ArrayData::Byte(bytes), .. }) = heap.get(arr_id) {
                        let end = off.saturating_add(len).min(bytes.len());
                        if off < end {
                            chunk.extend_from_slice(&bytes[off..end]);
                        }
                    }
                }
            }
            if let Some(cid) = conn_id {
                let body_id = match heap.get(cid) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("reqBody").and_then(|v| v.as_ref()),
                    _ => None,
                };
                if let Some(bid) = body_id {
                    if let Some(HeapObject::Array { data: ArrayData::Byte(dst), .. }) = heap.get_mut(bid) {
                        dst.extend_from_slice(&chunk);
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/io/OutputStream;", "flush" | "close") => Ok(BuiltinResult::Handled(Value::Void)),
        (
            "Ljava/net/URLConnection;" | "Ljava/net/HttpURLConnection;",
            "getResponseCode" | "getHeaderFields" | "getContentLength" | "getContentType" | "getInputStream" | "getErrorStream",
        ) => {
            let Some(cid) = args.first().and_then(|v| v.as_ref()) else {
                return Ok(BuiltinResult::Handled(Value::Ref(None)));
            };
            let already = match heap.get(cid) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("executed").and_then(|v| v.as_int().ok()).unwrap_or(0) != 0,
                _ => true,
            };
            if !already {
                let (url, mut http_method, connect_ms, read_ms, headers_id, body_id) = match heap.get(cid) {
                    Some(HeapObject::Instance { fields, .. }) => (
                        fields.get("url").and_then(|v| v.as_ref()).and_then(|u| heap.as_str(u)).unwrap_or("").to_string(),
                        fields.get("method").and_then(|v| v.as_ref()).and_then(|m| heap.as_str(m)).unwrap_or("GET").to_string(),
                        fields.get("connectTimeout").and_then(|v| v.as_int().ok()).unwrap_or(15000),
                        fields.get("readTimeout").and_then(|v| v.as_int().ok()).unwrap_or(15000),
                        fields.get("reqHeaders").and_then(|v| v.as_ref()),
                        fields.get("reqBody").and_then(|v| v.as_ref()),
                    ),
                    _ => (String::new(), "GET".to_string(), 15000, 15000, None, None),
                };
                let req_body: Vec<u8> = body_id
                    .and_then(|bid| match heap.get(bid) {
                        Some(HeapObject::Array { data: ArrayData::Byte(b), .. }) => Some(b.iter().map(|&x| x as u8).collect()),
                        _ => None,
                    })
                    .unwrap_or_default();
                if !req_body.is_empty() && http_method == "GET" {
                    http_method = "POST".to_string();
                }
                let mut cmd = std::process::Command::new("curl");
                cmd.arg("-sS").arg("-i").arg("-L").arg("-X").arg(&http_method)
                    .arg("--connect-timeout").arg(((connect_ms + 999) / 1000).max(1).to_string())
                    .arg("--max-time").arg(((read_ms + 999) / 1000).max(1).to_string());
                if let Some(hid) = headers_id {
                    if let Some(HeapObject::Map(m)) = heap.get(hid) {
                        for (k, v) in m {
                            if let Some(vs) = v.as_ref().and_then(|i| heap.as_str(i)) {
                                cmd.arg("-H").arg(format!("{k}: {vs}"));
                            }
                        }
                    }
                }
                if !req_body.is_empty() {
                    cmd.arg("--data-binary").arg("@-").stdin(std::process::Stdio::piped());
                } else {
                    cmd.stdin(std::process::Stdio::null());
                }
                cmd.arg(&url).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
                let (mut status_code, mut content_type, mut resp_headers, mut resp_body) = (200i32, String::new(), Vec::<(String, String)>::new(), Vec::<u8>::new());
                if let Ok(mut child) = cmd.spawn() {
                    if !req_body.is_empty() {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = std::io::Write::write_all(&mut stdin, &req_body);
                        }
                    }
                    if let Ok(out) = child.wait_with_output() {
                        let raw = &out.stdout;
                        let mut start = 0usize;
                        loop {
                            let rest = &raw[start..];
                            let sep = rest.windows(4).position(|w| w == b"\r\n\r\n").map(|p| (p, 4))
                                .or_else(|| rest.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2)));
                            let Some((hdr_end, sep_len)) = sep else {
                                resp_body = rest.to_vec();
                                break;
                            };
                            let hdr_text = String::from_utf8_lossy(&rest[..hdr_end]);
                            let mut lines = hdr_text.lines();
                            let first = lines.next().unwrap_or("");
                            if let Some(code_str) = first.split_whitespace().nth(1) {
                                if let Ok(c) = code_str.parse::<i32>() {
                                    status_code = c;
                                }
                            }
                            let mut parsed = Vec::new();
                            let mut ct = String::new();
                            for line in lines {
                                if let Some((k, v)) = line.split_once(':') {
                                    let kt = k.trim().to_string();
                                    let vt = v.trim().to_string();
                                    if kt.eq_ignore_ascii_case("content-type") {
                                        ct = vt.clone();
                                    }
                                    parsed.push((kt, vt));
                                }
                            }
                            resp_headers = parsed;
                            if !ct.is_empty() {
                                content_type = ct;
                            }
                            start += hdr_end + sep_len;
                            if !((300..400).contains(&status_code) && raw[start..].starts_with(b"HTTP/")) {
                                resp_body = raw[start..].to_vec();
                                break;
                            }
                        }
                    }
                }
                let mut hdr_entries = HashMap::new();
                for (k, v) in resp_headers {
                    let vs = heap.new_string(v);
                    let list = heap.alloc(HeapObject::List(vec![Value::Ref(Some(vs))]));
                    hdr_entries.insert(k, Value::Ref(Some(list)));
                }
                let hdr_map = heap.alloc(HeapObject::Map(hdr_entries));
                let ct_val = if content_type.is_empty() {
                    Value::Ref(None)
                } else {
                    Value::Ref(Some(heap.new_string(content_type)))
                };
                let body_len = resp_body.len() as i32;
                let body_arr = heap.alloc(HeapObject::Array {
                    type_desc: "[B".into(),
                    data: ArrayData::Byte(resp_body.into_iter().map(|b| b as i8).collect()),
                });
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(cid) {
                    fields.insert("executed".into(), Value::Int(1));
                    fields.insert("responseCode".into(), Value::Int(status_code));
                    fields.insert("contentType".into(), ct_val);
                    fields.insert("contentLength".into(), Value::Int(body_len));
                    fields.insert("headersMap".into(), Value::Ref(Some(hdr_map)));
                    fields.insert("respBody".into(), Value::Ref(Some(body_arr)));
                }
            }
            match method_name {
                "getResponseCode" => {
                    let code = match heap.get(cid) {
                        Some(HeapObject::Instance { fields, .. }) => fields.get("responseCode").and_then(|v| v.as_int().ok()).unwrap_or(200),
                        _ => 200,
                    };
                    Ok(BuiltinResult::Handled(Value::Int(code)))
                }
                "getContentLength" => {
                    let len = match heap.get(cid) {
                        Some(HeapObject::Instance { fields, .. }) => fields.get("contentLength").and_then(|v| v.as_int().ok()).unwrap_or(0),
                        _ => 0,
                    };
                    Ok(BuiltinResult::Handled(Value::Int(len)))
                }
                "getContentType" => {
                    let ct = match heap.get(cid) {
                        Some(HeapObject::Instance { fields, .. }) => fields.get("contentType").cloned().unwrap_or(Value::Ref(None)),
                        _ => Value::Ref(None),
                    };
                    Ok(BuiltinResult::Handled(ct))
                }
                "getHeaderFields" => {
                    let hm = match heap.get(cid) {
                        Some(HeapObject::Instance { fields, .. }) => fields.get("headersMap").cloned().unwrap_or(Value::Ref(None)),
                        _ => Value::Ref(None),
                    };
                    Ok(BuiltinResult::Handled(hm))
                }
                _ => {
                    let body_arr = match heap.get(cid) {
                        Some(HeapObject::Instance { fields, .. }) => fields.get("respBody").cloned().unwrap_or(Value::Ref(None)),
                        _ => Value::Ref(None),
                    };
                    let stream = heap.new_instance("Ljava/io/ByteArrayInputStream;");
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(stream) {
                        fields.insert("data".into(), body_arr);
                        fields.insert("pos".into(), Value::Int(0));
                    }
                    Ok(BuiltinResult::Handled(Value::Ref(Some(stream))))
                }
            }
        }
        ("Ljava/nio/channels/Channels;", "newChannel") => {
            let ch = heap.new_instance("Lpengu/internal/StreamChannel;");
            if let Some(stream) = args.first().cloned() {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(ch) {
                    fields.insert("stream".into(), stream);
                }
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(ch))))
        }
        ("Ljava/nio/channels/ReadableByteChannel;", "read") => {
            let (Some(ch_id), Some(bb_id)) = (args.first().and_then(|v| v.as_ref()), args.get(1).and_then(|v| v.as_ref())) else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let stream_id = match heap.get(ch_id) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("stream").and_then(|v| v.as_ref()),
                _ => None,
            };
            let Some(sid) = stream_id else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let (data_id, pos) = match heap.get(sid) {
                Some(HeapObject::Instance { fields, .. }) => (
                    fields.get("data").and_then(|v| v.as_ref()),
                    fields.get("pos").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                ),
                _ => (None, 0),
            };
            let Some(did) = data_id else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let (bb_pos, bb_lim, bb_backing) = match heap.get(bb_id) {
                Some(HeapObject::Instance { fields, .. }) => (
                    fields.get("position").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    fields.get("limit").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    fields.get("backing").and_then(|v| v.as_ref()),
                ),
                _ => (0, 0, None),
            };
            let want = bb_lim.saturating_sub(bb_pos);
            let chunk: Vec<i8> = match heap.get(did) {
                Some(HeapObject::Array { data: ArrayData::Byte(src), .. }) => {
                    if pos >= src.len() {
                        return Ok(BuiltinResult::Handled(Value::Int(-1)));
                    }
                    let take = want.min(src.len() - pos);
                    src[pos..pos + take].to_vec()
                }
                _ => return Ok(BuiltinResult::Handled(Value::Int(-1))),
            };
            let n = chunk.len();
            if let Some(arr_id) = bb_backing {
                if let Some(HeapObject::Array { data: ArrayData::Byte(dst), .. }) = heap.get_mut(arr_id) {
                    if bb_pos + n <= dst.len() {
                        dst[bb_pos..bb_pos + n].copy_from_slice(&chunk);
                    }
                }
            } else {
                let u8s: Vec<u8> = chunk.iter().map(|&b| b as u8).collect();
                crate::jni_host::direct_buffer_slice_mut(bb_id, |buf| {
                    if bb_pos + n <= buf.len() {
                        buf[bb_pos..bb_pos + n].copy_from_slice(&u8s);
                    }
                });
            }
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(sid) {
                fields.insert("pos".into(), Value::Int((pos + n) as i32));
            }
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(bb_id) {
                fields.insert("position".into(), Value::Int((bb_pos + n) as i32));
            }
            Ok(BuiltinResult::Handled(Value::Int(n as i32)))
        }
        ("Ljava/nio/channels/ReadableByteChannel;", "close") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/nio/ByteBuffer;", "get") if descriptor == "([BII)Ljava/nio/ByteBuffer;" => {
            if let (Some(id), Some(dst_id)) = (args.first().and_then(|v| v.as_ref()), args.get(1).and_then(|v| v.as_ref())) {
                let off = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                let len = args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                let pos = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("position").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    _ => 0,
                };
                let mut tmp = vec![0i8; len];
                let copied = crate::jni_host::direct_buffer_slice_mut(id, |buf| {
                    let avail = buf.len().saturating_sub(pos).min(len);
                    for i in 0..avail {
                        tmp[i] = buf[pos + i] as i8;
                    }
                    avail
                })
                .unwrap_or(0);
                if let Some(HeapObject::Array { data: ArrayData::Byte(dst), .. }) = heap.get_mut(dst_id) {
                    if off + copied <= dst.len() {
                        dst[off..off + copied].copy_from_slice(&tmp[..copied]);
                    }
                }
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("position".into(), Value::Int((pos + copied) as i32));
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Ljava/nio/charset/Charset;", "forName") => {
            let cname = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| heap.as_str(id))
                .unwrap_or("UTF-8")
                .to_string();
            let cs = heap.new_instance("Ljava/nio/charset/Charset;");
            let s = heap.new_string(cname);
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(cs) {
                fields.insert("name".into(), Value::Ref(Some(s)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(cs))))
        }
        ("Ljava/nio/charset/Charset;", "defaultCharset") => {
            let cs = heap.new_instance("Ljava/nio/charset/Charset;");
            let s = heap.new_string("UTF-8".into());
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(cs) {
                fields.insert("name".into(), Value::Ref(Some(s)));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(cs))))
        }
        ("Ljava/nio/charset/Charset;", "name" | "displayName" | "toString") => {
            let name = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields
                        .get("name")
                        .and_then(|v| v.as_ref())
                        .and_then(|sid| heap.as_str(sid))
                        .map(|s| s.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "UTF-8".into());
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(name)))))
        }
        ("Lsun/misc/Unsafe;", "allocateInstance") => {
            let target_desc = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|cid| match heap.get(cid) {
                    Some(HeapObject::ClassObj(d)) => Some(d.clone()),
                    _ => None,
                })
                .ok_or("NullPointerException: null class for allocateInstance")?;
            let obj = heap.new_instance(&target_desc);
            Ok(BuiltinResult::Handled(Value::Ref(Some(obj))))
        }
        ("Lsun/misc/Unsafe;", "arrayBaseOffset") => Ok(BuiltinResult::Handled(Value::Int(16))),
        ("Lsun/misc/Unsafe;", "arrayIndexScale") => {
            let scale = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|cid| match heap.get(cid) {
                    Some(HeapObject::ClassObj(d)) => Some(match d.as_str() {
                        "[B" | "[Z" => 1,
                        "[S" | "[C" => 2,
                        "[I" | "[F" => 4,
                        _ => 8,
                    }),
                    _ => None,
                })
                .unwrap_or(4);
            Ok(BuiltinResult::Handled(Value::Int(scale)))
        }
        ("Lsun/misc/Unsafe;", "objectFieldOffset") => Ok(BuiltinResult::Handled(Value::Long(16))),
        ("Lsun/misc/Unsafe;", "addressSize") => Ok(BuiltinResult::Handled(Value::Int(8))),
        ("Ljava/lang/Boolean;", "getBoolean") => Ok(BuiltinResult::Handled(Value::Int(0))),
        ("Ljava/util/concurrent/TimeUnit;", "toNanos") => {
            let dur = args.get(1).and_then(|v| v.as_long().ok()).unwrap_or(0);
            let unit = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("nanos").and_then(|v| v.as_long().ok()),
                    _ => None,
                })
                .unwrap_or(1);
            Ok(BuiltinResult::Handled(Value::Long(dur.saturating_mul(unit))))
        }
        ("Ljava/util/concurrent/TimeUnit;", "toMillis") => {
            let dur = args.get(1).and_then(|v| v.as_long().ok()).unwrap_or(0);
            let unit = args
                .first()
                .and_then(|v| v.as_ref())
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("nanos").and_then(|v| v.as_long().ok()),
                    _ => None,
                })
                .unwrap_or(1_000_000);
            Ok(BuiltinResult::Handled(Value::Long(dur.saturating_mul(unit) / 1_000_000)))
        }
        ("Ljava/util/concurrent/locks/ReentrantLock;", "<init>" | "lock" | "unlock") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/locks/ReentrantLock;", "tryLock") => {
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/concurrent/locks/ReentrantLock;", "newCondition") => {
            let c = heap.new_instance("Ljava/util/concurrent/locks/Condition;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(c))))
        }
        ("Ljava/util/concurrent/locks/Condition;", "await" | "signal" | "signalAll") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/concurrent/locks/Condition;", "awaitNanos") => {
            Ok(BuiltinResult::Handled(Value::Long(0)))
        }
        ("Landroid/os/Process;", "myPid") => {
            Ok(BuiltinResult::Handled(Value::Int(std::process::id() as i32)))
        }
        ("Landroid/view/View;", "setOnApplyWindowInsetsListener") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/app/Activity;", "getWindowManager") => {
            let wm = heap.new_instance("Landroid/view/WindowManager;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(wm))))
        }
        ("Landroid/content/IntentFilter;", "<init>" | "addAction") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/content/Context;" | "Landroid/content/ContextWrapper;" | "Landroid/app/Activity;", "registerReceiver") => {
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Landroid/content/Context;" | "Landroid/content/ContextWrapper;" | "Landroid/app/Activity;", "unregisterReceiver") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljavax/net/ssl/HttpsURLConnection;", "setSSLSocketFactory" | "setHostnameVerifier") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/io/RandomAccessFile;", "<init>") => {
            let path = args.get(1).and_then(|v| v.as_ref()).and_then(|fid| {
                heap.as_str(fid).map(str::to_string).or_else(|| match heap.get(fid) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("path").and_then(|p| p.as_ref()).and_then(|pid| heap.as_str(pid)).map(str::to_string),
                    _ => None,
                })
            }).unwrap_or_default();
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let psid = heap.new_string(path);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("path".into(), Value::Ref(Some(psid)));
                    fields.insert("pos".into(), Value::Long(0));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/io/RandomAccessFile;", "length") => {
            let path = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get(id) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("path").and_then(|p| p.as_ref()).and_then(|pid| heap.as_str(pid)).map(str::to_string),
                _ => None,
            }).unwrap_or_default();
            let len = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Long(len)))
        }
        ("Ljava/io/RandomAccessFile;", "seek") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let pos = args.get(1).and_then(|v| v.as_long().ok()).unwrap_or(0);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("pos".into(), Value::Long(pos));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/io/RandomAccessFile;", "close") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/io/RandomAccessFile;", "read") => {
            let Some(id) = args.first().and_then(|v| v.as_ref()) else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let (path, pos) = match heap.get(id) {
                Some(HeapObject::Instance { fields, .. }) => (
                    fields.get("path").and_then(|p| p.as_ref()).and_then(|pid| heap.as_str(pid)).unwrap_or("").to_string(),
                    fields.get("pos").and_then(|v| v.as_long().ok()).unwrap_or(0).max(0) as u64,
                ),
                _ => return Ok(BuiltinResult::Handled(Value::Int(-1))),
            };
            let Ok(mut f) = std::fs::File::open(&path) else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let _ = std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(pos));
            if args.len() == 1 {
                let mut b = [0u8; 1];
                let n = std::io::Read::read(&mut f, &mut b).unwrap_or(0);
                if n == 0 {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                }
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("pos".into(), Value::Long((pos + 1) as i64));
                }
                return Ok(BuiltinResult::Handled(Value::Int(b[0] as i32)));
            }
            if let Some(arr_id) = args.get(1).and_then(|v| v.as_ref()) {
                let off = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                let max_len = match heap.get(arr_id) {
                    Some(HeapObject::Array { data: ArrayData::Byte(b), .. }) => b.len().saturating_sub(off),
                    _ => 0,
                };
                let want = args.get(3).and_then(|v| v.as_int().ok()).map(|l| (l.max(0) as usize).min(max_len)).unwrap_or(max_len);
                let mut tmp = vec![0u8; want];
                let n = std::io::Read::read(&mut f, &mut tmp).unwrap_or(0);
                if n == 0 {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                }
                if let Some(HeapObject::Array { data: ArrayData::Byte(dst), .. }) = heap.get_mut(arr_id) {
                    for i in 0..n {
                        dst[off + i] = tmp[i] as i8;
                    }
                }
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("pos".into(), Value::Long((pos + n as u64) as i64));
                }
                return Ok(BuiltinResult::Handled(Value::Int(n as i32)));
            }
            Ok(BuiltinResult::Handled(Value::Int(-1)))
        }
        ("Ljava/util/Arrays;", "copyOf") => {
            let new_len = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
            let copied = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get(id) {
                Some(HeapObject::Array { type_desc, data }) => {
                    let td = type_desc.clone();
                    let new_data = match data {
                        ArrayData::Byte(v) => {
                            let mut out = vec![0i8; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Byte(out)
                        }
                        ArrayData::Short(v) => {
                            let mut out = vec![0i16; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Short(out)
                        }
                        ArrayData::Char(v) => {
                            let mut out = vec![0u16; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Char(out)
                        }
                        ArrayData::Int(v) => {
                            let mut out = vec![0i32; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Int(out)
                        }
                        ArrayData::Long(v) => {
                            let mut out = vec![0i64; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Long(out)
                        }
                        ArrayData::Float(v) => {
                            let mut out = vec![0.0f32; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Float(out)
                        }
                        ArrayData::Double(v) => {
                            let mut out = vec![0.0f64; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Double(out)
                        }
                        ArrayData::Boolean(v) => {
                            let mut out = vec![false; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Boolean(out)
                        }
                        ArrayData::Object(v) => {
                            let mut out = vec![None; new_len];
                            let n = new_len.min(v.len());
                            out[..n].copy_from_slice(&v[..n]);
                            ArrayData::Object(out)
                        }
                    };
                    Some((td, new_data))
                }
                _ => None,
            });
            if let Some((type_desc, data)) = copied {
                let arr_id = heap.alloc(HeapObject::Array { type_desc, data });
                Ok(BuiltinResult::Handled(Value::Ref(Some(arr_id))))
            } else {
                Ok(BuiltinResult::Handled(Value::Ref(None)))
            }
        }
        ("Ljava/util/Arrays;", "equals") => {
            let eq = match (args.first().and_then(|v| v.as_ref()), args.get(1).and_then(|v| v.as_ref())) {
                (None, None) => true,
                (Some(a), Some(b)) if a == b => true,
                (Some(a), Some(b)) => match (heap.get(a), heap.get(b)) {
                    (Some(HeapObject::Array { data: ArrayData::Byte(x), .. }), Some(HeapObject::Array { data: ArrayData::Byte(y), .. })) => x == y,
                    (Some(HeapObject::Array { data: ArrayData::Int(x), .. }), Some(HeapObject::Array { data: ArrayData::Int(y), .. })) => x == y,
                    (Some(HeapObject::Array { data: ArrayData::Char(x), .. }), Some(HeapObject::Array { data: ArrayData::Char(y), .. })) => x == y,
                    (Some(HeapObject::Array { data: ArrayData::Long(x), .. }), Some(HeapObject::Array { data: ArrayData::Long(y), .. })) => x == y,
                    (Some(HeapObject::Array { data: ArrayData::Object(x), .. }), Some(HeapObject::Array { data: ArrayData::Object(y), .. })) => x == y,
                    _ => false,
                },
                _ => false,
            };
            Ok(BuiltinResult::Handled(Value::Int(i32::from(eq))))
        }
        ("Ljava/nio/ByteBuffer;", "getShort") => {
            let Some(id) = args.first().and_then(|v| v.as_ref()) else {
                return Ok(BuiltinResult::Handled(Value::Int(0)));
            };
            let idx = args.get(1).and_then(|v| v.as_int().ok()).map(|i| i.max(0) as usize);
            let (pos, backing) = match heap.get(id) {
                Some(HeapObject::Instance { fields, .. }) => (
                    idx.unwrap_or_else(|| fields.get("position").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize),
                    fields.get("backing").and_then(|v| v.as_ref()),
                ),
                _ => (0, None),
            };
            let mut b = [0u8; 2];
            if let Some(arr_id) = backing {
                if let Some(HeapObject::Array { data: ArrayData::Byte(src), .. }) = heap.get(arr_id) {
                    if pos + 2 <= src.len() {
                        b = [src[pos] as u8, src[pos + 1] as u8];
                    }
                }
            } else {
                crate::jni_host::direct_buffer_slice_mut(id, |buf| {
                    if pos + 2 <= buf.len() {
                        b = [buf[pos], buf[pos + 1]];
                    }
                });
            }
            if idx.is_none() {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("position".into(), Value::Int((pos + 2) as i32));
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(i16::from_be_bytes(b) as i32)))
        }
        ("Ljava/nio/ByteBuffer;", "put") if descriptor == "(B)Ljava/nio/ByteBuffer;" => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let byte = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as i8;
                let (pos, backing) = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => (
                        fields.get("position").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                        fields.get("backing").and_then(|v| v.as_ref()),
                    ),
                    _ => (0, None),
                };
                if let Some(arr_id) = backing {
                    if let Some(HeapObject::Array { data: ArrayData::Byte(dst), .. }) = heap.get_mut(arr_id) {
                        if pos < dst.len() {
                            dst[pos] = byte;
                        }
                    }
                } else {
                    crate::jni_host::direct_buffer_slice_mut(id, |buf| {
                        if pos < buf.len() {
                            buf[pos] = byte as u8;
                        }
                    });
                }
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("position".into(), Value::Int((pos + 1) as i32));
                }
                return Ok(BuiltinResult::Handled(Value::Ref(Some(id))));
            }
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Ljavax/net/ssl/SSLContext;", "getInstance") => {
            let ctx = heap.new_instance("Ljavax/net/ssl/SSLContext;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(ctx))))
        }
        ("Ljavax/net/ssl/SSLContext;", "init") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljavax/net/ssl/SSLContext;", "getSocketFactory") => {
            let sf = heap.new_instance("Ljavax/net/ssl/SSLSocketFactory;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(sf))))
        }
        ("Ljavax/net/ssl/SSLSocketFactory;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljavax/net/SocketFactory;", "getDefault") => {
            let sf = heap.new_instance("Ljavax/net/SocketFactory;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(sf))))
        }
        ("Ljavax/net/ssl/SSLSocketFactory;", "getDefault") => {
            let sf = heap.new_instance("Ljavax/net/ssl/SSLSocketFactory;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(sf))))
        }
        ("Ljavax/net/ssl/SSLSocketFactory;", "getDefaultCipherSuites" | "getSupportedCipherSuites") => {
            let suites = [
                "TLS_AES_128_GCM_SHA256",
                "TLS_AES_256_GCM_SHA384",
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
            ]
            .into_iter()
            .map(|s| Some(heap.new_string(s.into())))
            .collect();
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/lang/String;".into(),
                data: ArrayData::Object(suites),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Ljavax/net/ssl/TrustManagerFactory;", "getDefaultAlgorithm") => {
            let s = heap.new_string("X509".into());
            Ok(BuiltinResult::Handled(Value::Ref(Some(s))))
        }
        ("Ljavax/net/ssl/TrustManagerFactory;", "getInstance") => {
            let tmf = heap.new_instance("Ljavax/net/ssl/TrustManagerFactory;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(tmf))))
        }
        ("Ljavax/net/ssl/TrustManagerFactory;", "init") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljavax/net/ssl/TrustManagerFactory;", "getTrustManagers") => {
            let tm = heap.new_instance("Ljavax/net/ssl/X509TrustManager;");
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Ljavax/net/ssl/TrustManager;".into(),
                data: ArrayData::Object(vec![Some(tm)]),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Ljavax/net/ssl/X509TrustManager;", "getAcceptedIssuers") => {
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/security/cert/X509Certificate;".into(),
                data: ArrayData::Object(Vec::new()),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Ljavax/net/ssl/X509TrustManager;", "checkClientTrusted" | "checkServerTrusted") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/Arrays;", "toString") => {
            let s = match args.first().and_then(|v| v.as_ref()).and_then(|id| heap.get(id)) {
                Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => {
                    let parts: Vec<String> = v
                        .iter()
                        .map(|o| match o {
                            Some(oid) => value_to_string(heap, &Value::Ref(Some(*oid))),
                            None => "null".to_string(),
                        })
                        .collect();
                    format!("[{}]", parts.join(", "))
                }
                Some(HeapObject::Array { data: ArrayData::Int(v), .. }) => format!("{v:?}"),
                Some(HeapObject::Array { data: ArrayData::Byte(v), .. }) => format!("{v:?}"),
                _ => "null".to_string(),
            };
            Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))))
        }
        ("Landroid/bluetooth/BluetoothAdapter;", "getDefaultAdapter") => {
            Ok(BuiltinResult::Handled(Value::Ref(None)))
        }
        ("Landroid/content/BroadcastReceiver;", "<init>") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/logging/Logger;", "getLogger") => {
            let logger = heap.new_instance("Ljava/util/logging/Logger;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(logger))))
        }
        ("Ljava/util/logging/Logger;", "log" | "info" | "warning" | "severe" | "fine" | "finer" | "finest" | "config") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/logging/Logger;", "isLoggable") => {
            Ok(BuiltinResult::Handled(Value::Int(0)))
        }
        ("Landroid/util/SparseArray;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let map = heap.alloc(HeapObject::Map(HashMap::new()));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("entries".into(), Value::Ref(Some(map)));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/util/SparseArray;", "put" | "append") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let key = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).to_string();
                let val = args.get(2).cloned().unwrap_or(Value::Ref(None));
                let map_id = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("entries").and_then(|v| v.as_ref()),
                    _ => None,
                };
                if let Some(mid) = map_id {
                    if let Some(HeapObject::Map(m)) = heap.get_mut(mid) {
                        m.insert(key, val);
                    }
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Landroid/util/SparseArray;", "get") => {
            let key = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).to_string();
            let default = args.get(2).cloned().unwrap_or(Value::Ref(None));
            let val = args.first().and_then(|v| v.as_ref()).and_then(|id| {
                let mid = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("entries").and_then(|v| v.as_ref())?,
                    _ => return None,
                };
                match heap.get(mid) {
                    Some(HeapObject::Map(m)) => m.get(&key).cloned(),
                    _ => None,
                }
            });
            Ok(BuiltinResult::Handled(val.unwrap_or(default)))
        }
        ("Landroid/util/SparseArray;", "size") => {
            let len = args.first().and_then(|v| v.as_ref()).and_then(|id| {
                let mid = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields.get("entries").and_then(|v| v.as_ref())?,
                    _ => return None,
                };
                match heap.get(mid) {
                    Some(HeapObject::Map(m)) => Some(m.len() as i32),
                    _ => None,
                }
            }).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(len)))
        }
        ("Ljava/lang/Throwable;", "printStackTrace") => {
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/security/Security;", "getProviders") => {
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/security/Provider;".into(),
                data: ArrayData::Object(Vec::new()),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        ("Ljava/security/Provider;", "getName") => {
            let s = heap.new_string("AndroidOpenSSL".into());
            Ok(BuiltinResult::Handled(Value::Ref(Some(s))))
        }
        ("Ljava/net/ProxySelector;", "<init>") => Ok(BuiltinResult::Handled(Value::Void)),
        ("Ljava/net/ProxySelector;", "getDefault") => {
            let ps = heap.new_instance("Ljava/net/ProxySelector;");
            Ok(BuiltinResult::Handled(Value::Ref(Some(ps))))
        }
        ("Ljava/net/ProxySelector;", "select") => {
            let list = heap.alloc(HeapObject::List(Vec::new()));
            Ok(BuiltinResult::Handled(Value::Ref(Some(list))))
        }
        ("Ljava/util/ArrayDeque;", "<init>") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(slot) = heap.get_mut(id) {
                    *slot = HeapObject::List(Vec::new());
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        ("Ljava/util/ArrayDeque;", "add" | "addLast" | "offer" | "offerLast") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let val = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::List(vec)) = heap.get_mut(id) {
                    vec.push(val);
                }
            }
            Ok(BuiltinResult::Handled(Value::Int(1)))
        }
        ("Ljava/util/ArrayDeque;", "addFirst" | "offerFirst" | "push") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                let val = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::List(vec)) = heap.get_mut(id) {
                    vec.insert(0, val);
                }
            }
            Ok(BuiltinResult::Handled(if method_name.starts_with("offer") { Value::Int(1) } else { Value::Void }))
        }
        ("Ljava/util/ArrayDeque;", "poll" | "pollFirst" | "pop" | "remove" | "removeFirst") => {
            let val = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get_mut(id) {
                Some(HeapObject::List(vec)) if !vec.is_empty() => Some(vec.remove(0)),
                _ => None,
            }).unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/util/ArrayDeque;", "peek" | "peekFirst") => {
            let val = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get(id) {
                Some(HeapObject::List(vec)) => vec.first().cloned(),
                _ => None,
            }).unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(val))
        }
        ("Ljava/util/ArrayDeque;", "size") => {
            let n = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get(id) {
                Some(HeapObject::List(vec)) => Some(vec.len() as i32),
                _ => None,
            }).unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(n)))
        }
        ("Ljava/util/ArrayDeque;", "isEmpty") => {
            let empty = args.first().and_then(|v| v.as_ref()).and_then(|id| match heap.get(id) {
                Some(HeapObject::List(vec)) => Some(vec.is_empty()),
                _ => None,
            }).unwrap_or(true);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(empty))))
        }
        ("Ljava/util/ArrayDeque;", "clear") => {
            if let Some(id) = args.first().and_then(|v| v.as_ref()) {
                if let Some(HeapObject::List(vec)) = heap.get_mut(id) {
                    vec.clear();
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
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
    static_fields.insert("Landroid/content/DialogInterface;.BUTTON_POSITIVE".into(), Value::Int(-1));
    static_fields.insert("Landroid/content/DialogInterface;.BUTTON_NEGATIVE".into(), Value::Int(-2));
    static_fields.insert("Landroid/content/DialogInterface;.BUTTON_NEUTRAL".into(), Value::Int(-3));
    static_fields.insert("Landroid/content/Context;.MODE_PRIVATE".into(), Value::Int(0));
    static_fields.insert("Landroid/view/WindowManager$LayoutParams;.FLAG_KEEP_SCREEN_ON".into(), Value::Int(128));
    let model = heap.new_string("PenguDroid".into());
    let abi = heap.new_string("arm64-v8a".into());
    static_fields.insert("Landroid/os/Build;.MODEL".into(), Value::Ref(Some(model)));
    static_fields.insert("Landroid/os/Build;.SUPPORTED_ABIS".into(), Value::Ref(Some(abi)));
    for (field, name) in [("UTF_8", "UTF-8"), ("US_ASCII", "US-ASCII"), ("ISO_8859_1", "ISO-8859-1")] {
        let cs = heap.new_instance("Ljava/nio/charset/Charset;");
        let s = heap.new_string(name.into());
        if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(cs) {
            fields.insert("name".into(), Value::Ref(Some(s)));
        }
        static_fields.insert(format!("Ljava/nio/charset/StandardCharsets;.{field}"), Value::Ref(Some(cs)));
    }
    let unsafe_obj = heap.new_instance("Lsun/misc/Unsafe;");
    static_fields.insert("Lsun/misc/Unsafe;.theUnsafe".into(), Value::Ref(Some(unsafe_obj)));
    let json_null = heap.new_instance("Lorg/json/JSONObject$1;");
    static_fields.insert("Lorg/json/JSONObject;.NULL".into(), Value::Ref(Some(json_null)));
    for (field, lang, country) in [
        ("US", "en", "US"),
        ("UK", "en", "GB"),
        ("ENGLISH", "en", ""),
        ("ROOT", "", ""),
        ("CHINA", "zh", "CN"),
        ("SIMPLIFIED_CHINESE", "zh", "CN"),
        ("TRADITIONAL_CHINESE", "zh", "TW"),
        ("JAPAN", "ja", "JP"),
        ("KOREA", "ko", "KR"),
        ("FRANCE", "fr", "FR"),
        ("GERMANY", "de", "DE"),
    ] {
        let loc = heap.new_instance("Ljava/util/Locale;");
        let ls = heap.new_string(lang.into());
        let cs = heap.new_string(country.into());
        if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(loc) {
            fields.insert("language".into(), Value::Ref(Some(ls)));
            fields.insert("country".into(), Value::Ref(Some(cs)));
        }
        static_fields.insert(format!("Ljava/util/Locale;.{field}"), Value::Ref(Some(loc)));
    }
}
