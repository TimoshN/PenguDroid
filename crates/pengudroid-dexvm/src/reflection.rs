//! Reflection metadata and InvocationHandler-backed dynamic proxy dispatch.
use crate::jni_host::jni_state;
use crate::{ArrayData, HeapObject, ObjId, Value, Vm};
use std::collections::HashMap;
pub(crate) fn get(id: ObjId, key: &str) -> Value {
    match jni_state().lock().unwrap().heap.get(id) {
        Some(HeapObject::Instance { fields, .. }) => {
            fields.get(key).cloned().unwrap_or(Value::Ref(None))
        }
        _ => Value::Ref(None),
    }
}
fn text(id: ObjId) -> Result<String, String> {
    jni_state()
        .lock()
        .unwrap()
        .heap
        .as_str(id)
        .map(str::to_owned)
        .ok_or("expected reflection string".into())
}
fn class_desc(id: ObjId) -> Result<String, String> {
    match jni_state().lock().unwrap().heap.get(id) {
        Some(HeapObject::ClassObj(c)) => Ok(c.clone()),
        _ => Err("expected Class object".into()),
    }
}
fn array(id: ObjId) -> Result<Vec<Option<ObjId>>, String> {
    match jni_state().lock().unwrap().heap.get(id) {
        Some(HeapObject::Array {
            data: ArrayData::Object(v),
            ..
        }) => Ok(v.clone()),
        _ => Err("expected reflection array".into()),
    }
}
fn string(value: &str) -> ObjId {
    jni_state().lock().unwrap().heap.new_string(value.into())
}
fn object(class: &str, fields: HashMap<String, Value>) -> ObjId {
    jni_state()
        .lock()
        .unwrap()
        .heap
        .alloc(HeapObject::Instance {
            class_name: class.into(),
            fields,
        })
}
pub(crate) fn method(class: &str, name: &str, sig: &str, flags: u32) -> ObjId {
    let declaring = jni_state().lock().unwrap().heap.class_object(class);
    let name = string(name);
    let signature = string(sig);
    object(
        "Ljava/lang/reflect/Method;",
        HashMap::from([
            ("declaringClass".into(), Value::Ref(Some(declaring))),
            ("name".into(), Value::Ref(Some(name))),
            ("descriptor".into(), Value::Ref(Some(signature))),
            ("modifiers".into(), Value::Int(flags as i32)),
        ]),
    )
}
pub(crate) fn method_info(id: ObjId) -> Result<(String, String, String, u32), String> {
    Ok((
        class_desc(
            get(id, "declaringClass")
                .as_ref()
                .ok_or("no declaring class")?,
        )?,
        text(get(id, "name").as_ref().ok_or("no method name")?)?,
        text(
            get(id, "descriptor")
                .as_ref()
                .ok_or("no method descriptor")?,
        )?,
        get(id, "modifiers").as_int().unwrap_or(0) as u32,
    ))
}
pub(crate) fn parameter_types(sig: &str) -> Vec<String> {
    let bytes = sig.as_bytes();
    let mut i = 1;
    let mut result = Vec::new();
    while i < bytes.len() && bytes[i] != b')' {
        let begin = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        result.push(sig[begin..i.min(bytes.len())].into());
    }
    result
}
fn box_value(value: Value) -> Value {
    let class = match value {
        Value::Int(_) => "Ljava/lang/Integer;",
        Value::Long(_) => "Ljava/lang/Long;",
        Value::Float(_) => "Ljava/lang/Float;",
        Value::Double(_) => "Ljava/lang/Double;",
        _ => return value,
    };
    Value::Ref(Some(object(
        class,
        HashMap::from([("value".into(), value)]),
    )))
}
fn unbox(value: Value, ty: &str) -> Result<Value, String> {
    if ty.starts_with(['L', '[']) {
        return Ok(value);
    }
    if ty == "V" {
        return Ok(Value::Void);
    }
    let id = value
        .as_ref()
        .ok_or("NullPointerException: null boxed primitive")?;
    let value = get(id, "value");
    match ty {
        "J" => Ok(Value::Long(value.as_long()?)),
        "F" => Ok(Value::Float(value.as_float()?)),
        "D" => Ok(Value::Double(value.as_double()?)),
        _ => Ok(Value::Int(value.as_int()?)),
    }
}
impl Vm {
    fn reflected_fields(&self, class: &str) -> Vec<(String, String, u32)> {
        if let Some((di, definition)) = self.find_class(class) {
            return definition
                .static_fields
                .iter()
                .chain(&definition.instance_fields)
                .map(|encoded| {
                    let f = &self.dexes[di].fields[encoded.field_idx];
                    (f.name.clone(), f.type_name.clone(), encoded.access_flags)
                })
                .collect();
        }
        if class == "Landroid/content/res/Configuration;" {
            return [
                "uiMode",
                "screenLayout",
                "orientation",
                "densityDpi",
                "screenWidthDp",
                "screenHeightDp",
                "smallestScreenWidthDp",
                "keyboard",
                "keyboardHidden",
                "hardKeyboardHidden",
                "navigation",
                "navigationHidden",
                "touchscreen",
                "mcc",
                "mnc",
                "colorMode",
            ]
            .into_iter()
            .map(|name| (name.into(), "I".into(), 1))
            .chain(std::iter::once(("fontScale".into(), "F".into(), 1)))
            .collect();
        }
        Vec::new()
    }
    fn reflected_field(&self, class: &str, name: &str, ty: &str, flags: u32) -> ObjId {
        let class = jni_state().lock().unwrap().heap.class_object(class);
        object(
            "Ljava/lang/reflect/Field;",
            HashMap::from([
                ("declaringClass".into(), Value::Ref(Some(class))),
                ("name".into(), Value::Ref(Some(string(name)))),
                ("descriptor".into(), Value::Ref(Some(string(ty)))),
                ("modifiers".into(), Value::Int(flags as i32)),
            ]),
        )
    }
    pub(crate) fn invoke_reflection(
        &mut self,
        class: &str,
        name: &str,
        sig: &str,
        args: &[Value],
        virtual_call: bool,
    ) -> Result<Option<Value>, String> {
        let receiver = args.first().and_then(Value::as_ref);
        if let Some(id) = receiver {
            let ann_hit = {
                let st = jni_state().lock().unwrap();
                match st.heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) if fields.contains_key("__pengu_annotation") => {
                        Some(fields.get(&format!("elem:{name}")).cloned())
                    }
                    _ => None,
                }
            };
            if let Some(elem_opt) = ann_hit {
                if name == "annotationType" {
                    let cid = jni_state().lock().unwrap().heap.class_object(class);
                    return Ok(Some(Value::Ref(Some(cid))));
                }
                if let Some(val) = elem_opt {
                    return Ok(Some(val));
                }
                if sig.ends_with(")[Ljava/lang/String;") {
                    let arr = jni_state().lock().unwrap().heap.alloc(HeapObject::Array {
                        type_desc: "[Ljava/lang/String;".into(),
                        data: ArrayData::Object(Vec::new()),
                    });
                    return Ok(Some(Value::Ref(Some(arr))));
                }
                return Ok(Some(crate::vm::default_return_for_descriptor(sig)));
            }
        }
        if class == "Ljava/lang/Class;"
            && matches!(name, "getDeclaredConstructor" | "getConstructor")
        {
            let declaring = class_desc(receiver.ok_or("NullPointerException: null Class")?)?;
            let parameters = args
                .get(1)
                .and_then(Value::as_ref)
                .map(array)
                .transpose()?
                .unwrap_or_default();
            let mut descriptor = "(".to_string();
            for parameter in parameters {
                descriptor.push_str(&class_desc(
                    parameter.ok_or("NullPointerException: null constructor parameter")?,
                )?);
            }
            descriptor.push_str(")V");
            let flags = self.find_class(&declaring).and_then(|(di, c)| {
                c.direct_methods
                    .iter()
                    .find(|m| {
                        let id = &self.dexes[di].methods[m.method_idx];
                        id.name == "<init>" && id.descriptor == descriptor
                    })
                    .map(|m| m.access_flags)
            });
            let flags = flags.or_else(|| {
                (declaring == "Ljava/lang/invoke/MethodHandles$Lookup;"
                    && descriptor == "(Ljava/lang/Class;I)V")
                    .then_some(2)
            });
            let flags = flags
                .filter(|flags| name == "getDeclaredConstructor" || flags & 1 != 0)
                .ok_or_else(|| format!("NoSuchMethodException: {declaring}.<init>{descriptor}"))?;
            let id = method(&declaring, "<init>", &descriptor, flags);
            if let Some(HeapObject::Instance { class_name, .. }) =
                jni_state().lock().unwrap().heap.get_mut(id)
            {
                *class_name = "Ljava/lang/reflect/Constructor;".into();
            }
            return Ok(Some(Value::Ref(Some(id))));
        }
        if class == "Ljava/lang/invoke/MethodHandles$Lookup;" && name == "<init>" {
            let id = receiver.ok_or("NullPointerException: null Lookup")?;
            let mut state = jni_state().lock().unwrap();
            if let Some(HeapObject::Instance { fields, .. }) = state.heap.get_mut(id) {
                fields.insert(
                    "lookupClass".into(),
                    args.get(1).cloned().unwrap_or(Value::Ref(None)),
                );
                fields.insert(
                    "allowedModes".into(),
                    args.get(2).cloned().unwrap_or(Value::Int(0)),
                );
            }
            return Ok(Some(Value::Void));
        }
        if class == "Ljava/lang/invoke/MethodHandles$Lookup;" && name == "in" {
            let mut state = jni_state().lock().unwrap();
            let id = state.heap.new_instance("Ljava/lang/invoke/MethodHandles$Lookup;");
            if let Some(HeapObject::Instance { fields, .. }) = state.heap.get_mut(id) {
                fields.insert("lookupClass".into(), args.get(1).cloned().unwrap_or(Value::Ref(None)));
                fields.insert("allowedModes".into(), Value::Int(-1));
            }
            return Ok(Some(Value::Ref(Some(id))));
        }
        if class == "Ljava/lang/invoke/MethodHandles$Lookup;" && name == "unreflectSpecial" {
            let mut state = jni_state().lock().unwrap();
            let id = state.heap.new_instance("Ljava/lang/invoke/MethodHandle;");
            if let Some(HeapObject::Instance { fields, .. }) = state.heap.get_mut(id) {
                fields.insert("method".into(), args.get(1).cloned().unwrap_or(Value::Ref(None)));
                fields.insert("specialCaller".into(), args.get(2).cloned().unwrap_or(Value::Ref(None)));
                fields.insert("bound".into(), Value::Ref(None));
            }
            return Ok(Some(Value::Ref(Some(id))));
        }
        if class == "Ljava/lang/invoke/MethodHandle;" && name == "bindTo" {
            let id = receiver.ok_or("NullPointerException: null MethodHandle")?;
            let mut state = jni_state().lock().unwrap();
            let cloned_fields = match state.heap.get(id) {
                Some(HeapObject::Instance { fields, .. }) => fields.clone(),
                _ => HashMap::new(),
            };
            let copy_id = state.heap.new_instance("Ljava/lang/invoke/MethodHandle;");
            if let Some(HeapObject::Instance { fields, .. }) = state.heap.get_mut(copy_id) {
                *fields = cloned_fields;
                fields.insert("bound".into(), args.get(1).cloned().unwrap_or(Value::Ref(None)));
            }
            return Ok(Some(Value::Ref(Some(copy_id))));
        }
        if class == "Ljava/lang/invoke/MethodHandle;" && name == "invokeWithArguments" {
            let id = receiver.ok_or("NullPointerException: null MethodHandle")?;
            let (method_id, bound) = {
                let state = jni_state().lock().unwrap();
                match state.heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => (
                        fields.get("method").and_then(Value::as_ref),
                        fields.get("bound").cloned().unwrap_or(Value::Ref(None)),
                    ),
                    _ => (None, Value::Ref(None)),
                }
            };
            let method_id = method_id.ok_or("NullPointerException: null MethodHandle target")?;
            let (declaring, method_name, descriptor, flags) = method_info(method_id)?;
            let mut values = Vec::new();
            if flags & 8 == 0 && !matches!(bound, Value::Ref(None)) {
                values.push(bound);
            }
            let parameters = parameter_types(&descriptor);
            let supplied = args
                .get(1)
                .and_then(Value::as_ref)
                .map(array)
                .transpose()?
                .unwrap_or_default();
            for (ty, value) in parameters.iter().zip(supplied) {
                values.push(unbox(Value::Ref(value), ty)?);
            }
            let result = self.invoke_method(&declaring, &method_name, &descriptor, &values, false)?;
            return Ok(Some(if descriptor.ends_with('V') {
                Value::Ref(None)
            } else {
                box_value(result)
            }));
        }
        if matches!(
            (class, name),
            ("Landroid/app/AlertDialog$Builder;", "show") | ("Landroid/app/AlertDialog;", "show")
        ) {
            let dlg = if class == "Landroid/app/AlertDialog$Builder;" {
                let mut st = jni_state().lock().unwrap();
                let copied = receiver
                    .and_then(|id| match st.heap.get(id) {
                        Some(HeapObject::Instance { fields, .. }) => Some(fields.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let dlg = st.heap.new_instance("Landroid/app/AlertDialog;");
                if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(dlg) {
                    for (k, v) in copied {
                        fields.insert(k, v);
                    }
                }
                dlg
            } else {
                receiver.ok_or("NullPointerException: null AlertDialog")?
            };
            let listener = {
                let st = jni_state().lock().unwrap();
                match st.heap.get(dlg) {
                    Some(HeapObject::Instance { fields, .. }) => fields
                        .get("positiveListener")
                        .and_then(Value::as_ref)
                        .map(|lid| {
                            let cls = st.heap.get(lid).map(|o| o.class_name().to_string()).unwrap_or_default();
                            (lid, cls)
                        }),
                    _ => None,
                }
            };
            if let Some((lid, lcls)) = listener {
                if !lcls.is_empty() {
                    self.post_dialog_click(lid, dlg, -1);
                }
            }
            return Ok(Some(if class == "Landroid/app/AlertDialog$Builder;" {
                Value::Ref(Some(dlg))
            } else {
                Value::Void
            }));
        }
        if class == "Landroid/net/ConnectivityManager;" && name == "registerDefaultNetworkCallback" {
            if let Some(cb) = args.get(1).and_then(Value::as_ref) {
                let (cb_cls, net, caps) = {
                    let mut st = jni_state().lock().unwrap();
                    let cls = st.heap.get(cb).map(|o| o.class_name().to_string()).unwrap_or_default();
                    let net = st.heap.new_instance("Landroid/net/Network;");
                    let caps = st.heap.new_instance("Landroid/net/NetworkCapabilities;");
                    (cls, net, caps)
                };
                if !cb_cls.is_empty() {
                    let _ = self.invoke_method(
                        &cb_cls,
                        "onAvailable",
                        "(Landroid/net/Network;)V",
                        &[Value::Ref(Some(cb)), Value::Ref(Some(net))],
                        true,
                    );
                    let _ = self.invoke_method(
                        &cb_cls,
                        "onCapabilitiesChanged",
                        "(Landroid/net/Network;Landroid/net/NetworkCapabilities;)V",
                        &[Value::Ref(Some(cb)), Value::Ref(Some(net)), Value::Ref(Some(caps))],
                        true,
                    );
                }
            }
            return Ok(Some(Value::Void));
        }
        if class == "Lcom/google/android/play/core/assetpacks/AssetPackManagerFactory;" && name == "getInstance" {
            let id = jni_state().lock().unwrap().heap.new_instance("Lcom/google/android/play/core/assetpacks/AssetPackManager;");
            return Ok(Some(Value::Ref(Some(id))));
        }
        if class == "Lcom/google/android/play/core/assetpacks/AssetPackManager;" {
            match name {
                "registerListener" => return Ok(Some(Value::Void)),
                "getPackStates" => {
                    let id = jni_state().lock().unwrap().heap.new_instance("Lcom/google/android/play/core/tasks/Task;");
                    return Ok(Some(Value::Ref(Some(id))));
                }
                _ => {}
            }
        }
        if class == "Lcom/google/android/play/core/tasks/Task;" {
            match name {
                "addOnCompleteListener" => {
                    let task_id = receiver.ok_or("null Task")?;
                    if let Some(listener) = args.get(1).and_then(Value::as_ref) {
                        let listener_class = jni_state()
                            .lock()
                            .unwrap()
                            .heap
                            .get(listener)
                            .map(|o| o.class_name().to_string())
                            .unwrap_or_default();
                        if !listener_class.is_empty() {
                            let _ = self.invoke_method(
                                &listener_class,
                                "onComplete",
                                "(Lcom/google/android/play/core/tasks/Task;)V",
                                &[Value::Ref(Some(listener)), Value::Ref(Some(task_id))],
                                true,
                            );
                        }
                    }
                    return Ok(Some(Value::Ref(Some(task_id))));
                }
                "isSuccessful" => return Ok(Some(Value::Int(0))),
                "isComplete" => return Ok(Some(Value::Int(1))),
                "getException" => {
                    let mut st = jni_state().lock().unwrap();
                    let ex = st.heap.new_instance("Ljava/lang/RuntimeException;");
                    let msg = st.heap.new_string("Play Asset Delivery unavailable".into());
                    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(ex) {
                        fields.insert("detailMessage".into(), Value::Ref(Some(msg)));
                    }
                    return Ok(Some(Value::Ref(Some(ex))));
                }
                "getResult" => {
                    let states = jni_state()
                        .lock()
                        .unwrap()
                        .heap
                        .new_instance("Lcom/google/android/play/core/assetpacks/AssetPackStates;");
                    return Ok(Some(Value::Ref(Some(states))));
                }
                _ => {}
            }
        }
        if let Some(recv) = receiver {
            let recv_class = jni_state()
                .lock()
                .unwrap()
                .heap
                .get(recv)
                .map(|o| o.class_name().to_string());
            if recv_class
                .as_deref()
                .is_some_and(|c| {
                    self.is_assignable_from("Ljava/util/Observable;", c)
                        && (class == "Ljava/util/Observable;"
                            || self
                                .find_method_in_hierarchy(c, name, sig, virtual_call)
                                .is_none())
                })
                && matches!(
                    name,
                    "<init>"
                        | "addObserver"
                        | "deleteObserver"
                        | "deleteObservers"
                        | "setChanged"
                        | "clearChanged"
                        | "hasChanged"
                        | "countObservers"
                        | "notifyObservers"
                )
            {
                match name {
                    "<init>" => {
                        let mut st = jni_state().lock().unwrap();
                        if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                            fields.entry("obs_changed".into()).or_insert(Value::Int(0));
                        }
                        return Ok(Some(Value::Void));
                    }
                    "addObserver" => {
                        if let Some(obs) = args.get(1).and_then(Value::as_ref) {
                            let mut st = jni_state().lock().unwrap();
                            let list_id = match st.heap.get(recv) {
                                Some(HeapObject::Instance { fields, .. }) => {
                                    fields.get("obs_list").and_then(Value::as_ref)
                                }
                                _ => None,
                            }
                            .unwrap_or_else(|| {
                                let lid = st.heap.new_instance("Ljava/util/ArrayList;");
                                if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                                    fields.insert("obs_list".into(), Value::Ref(Some(lid)));
                                }
                                lid
                            });
                            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(list_id) {
                                let arr_id = fields.get("items").and_then(Value::as_ref);
                                let _ = arr_id;
                            }
                            // Store directly in a Vec-backed Object array field `obs_items`
                            let cur = match st.heap.get(recv) {
                                Some(HeapObject::Instance { fields, .. }) => fields.get("obs_items").and_then(Value::as_ref),
                                _ => None,
                            };
                            let mut items = cur
                                .and_then(|aid| match st.heap.get(aid) {
                                    Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => Some(v.clone()),
                                    _ => None,
                                })
                                .unwrap_or_default();
                            if !items.contains(&Some(obs)) {
                                items.push(Some(obs));
                            }
                            let new_arr = st.heap.alloc(HeapObject::Array {
                                type_desc: "[Ljava/lang/Object;".into(),
                                data: ArrayData::Object(items),
                            });
                            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                                fields.insert("obs_items".into(), Value::Ref(Some(new_arr)));
                            }
                        }
                        return Ok(Some(Value::Void));
                    }
                    "deleteObserver" => {
                        if let Some(obs) = args.get(1).and_then(Value::as_ref) {
                            let mut st = jni_state().lock().unwrap();
                            let cur = match st.heap.get(recv) {
                                Some(HeapObject::Instance { fields, .. }) => fields.get("obs_items").and_then(Value::as_ref),
                                _ => None,
                            };
                            if let Some(aid) = cur {
                                if let Some(HeapObject::Array { data: ArrayData::Object(v), .. }) = st.heap.get_mut(aid) {
                                    v.retain(|x| *x != Some(obs));
                                }
                            }
                        }
                        return Ok(Some(Value::Void));
                    }
                    "deleteObservers" => {
                        let mut st = jni_state().lock().unwrap();
                        if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                            fields.remove("obs_items");
                        }
                        return Ok(Some(Value::Void));
                    }
                    "setChanged" => {
                        let mut st = jni_state().lock().unwrap();
                        if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                            fields.insert("obs_changed".into(), Value::Int(1));
                        }
                        return Ok(Some(Value::Void));
                    }
                    "clearChanged" => {
                        let mut st = jni_state().lock().unwrap();
                        if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                            fields.insert("obs_changed".into(), Value::Int(0));
                        }
                        return Ok(Some(Value::Void));
                    }
                    "hasChanged" => {
                        let st = jni_state().lock().unwrap();
                        let changed = match st.heap.get(recv) {
                            Some(HeapObject::Instance { fields, .. }) => {
                                fields.get("obs_changed").and_then(|v| v.as_int().ok()).unwrap_or(0) != 0
                            }
                            _ => false,
                        };
                        return Ok(Some(Value::Int(changed as i32)));
                    }
                    "countObservers" => {
                        let st = jni_state().lock().unwrap();
                        let count = match st.heap.get(recv) {
                            Some(HeapObject::Instance { fields, .. }) => fields
                                .get("obs_items")
                                .and_then(Value::as_ref)
                                .and_then(|aid| match st.heap.get(aid) {
                                    Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => Some(v.len() as i32),
                                    _ => None,
                                })
                                .unwrap_or(0),
                            _ => 0,
                        };
                        return Ok(Some(Value::Int(count)));
                    }
                    "notifyObservers" => {
                        let arg_val = args.get(1).cloned().unwrap_or(Value::Ref(None));
                        let snapshot: Vec<(ObjId, String)> = {
                            let mut st = jni_state().lock().unwrap();
                            let changed = match st.heap.get(recv) {
                                Some(HeapObject::Instance { fields, .. }) => {
                                    fields.get("obs_changed").and_then(|v| v.as_int().ok()).unwrap_or(0) != 0
                                }
                                _ => false,
                            };
                            if !changed {
                                return Ok(Some(Value::Void));
                            }
                            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(recv) {
                                fields.insert("obs_changed".into(), Value::Int(0));
                            }
                            let arr_id = match st.heap.get(recv) {
                                Some(HeapObject::Instance { fields, .. }) => fields.get("obs_items").and_then(Value::as_ref),
                                _ => None,
                            };
                            let ids = arr_id
                                .and_then(|aid| match st.heap.get(aid) {
                                    Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => {
                                        Some(v.iter().flatten().copied().collect::<Vec<_>>())
                                    }
                                    _ => None,
                                })
                                .unwrap_or_default();
                            ids.into_iter()
                                .filter_map(|oid| {
                                    st.heap.get(oid).map(|o| (oid, o.class_name().to_string()))
                                })
                                .collect()
                        };
                        for (oid, oclass) in snapshot.into_iter().rev() {
                            let _ = self.invoke_method(
                                &oclass,
                                "update",
                                "(Ljava/util/Observable;Ljava/lang/Object;)V",
                                &[Value::Ref(Some(oid)), Value::Ref(Some(recv)), arg_val.clone()],
                                true,
                            );
                        }
                        return Ok(Some(Value::Void));
                    }
                    _ => {}
                }
            }
        }
        if class == "Ljava/lang/reflect/Modifier;" {
            let mask = match name {
                "isPublic" => 1,
                "isPrivate" => 2,
                "isProtected" => 4,
                "isStatic" => 8,
                "isFinal" => 16,
                "isSynchronized" => 32,
                "isVolatile" => 64,
                "isTransient" => 128,
                "isNative" => 256,
                "isInterface" => 512,
                "isAbstract" => 1024,
                "isStrict" => 2048,
                _ => 0,
            };
            if mask != 0 {
                return Ok(Some(Value::Int(
                    (args.first().and_then(|v| v.as_int().ok()).unwrap_or(0) & mask != 0) as i32,
                )));
            }
        }
        if class == "Ljava/lang/Class;"
            && matches!(
                name,
                "getDeclaredMethods" | "getMethods" | "getDeclaredConstructors" | "getConstructors"
            )
        {
            let declaring = class_desc(receiver.ok_or("NullPointerException: null Class")?)?;
            let constructors = name.ends_with("Constructors");
            let mut current = Some(declaring);
            let mut values = Vec::new();
            let mut seen = std::collections::HashSet::new();
            while let Some(c) = current {
                if let Some((di, definition)) = self.find_class(&c) {
                    for encoded in definition
                        .direct_methods
                        .iter()
                        .chain(&definition.virtual_methods)
                    {
                        let id = &self.dexes[di].methods[encoded.method_idx];
                        if (id.name == "<init>") != constructors
                            || id.name == "<clinit>"
                            || (!name.starts_with("getDeclared") && encoded.access_flags & 1 == 0)
                        {
                            continue;
                        }
                        if !seen.insert((id.name.clone(), id.descriptor.clone())) {
                            continue;
                        }
                        let id = method(&c, &id.name, &id.descriptor, encoded.access_flags);
                        if constructors {
                            if let Some(HeapObject::Instance { class_name, .. }) =
                                jni_state().lock().unwrap().heap.get_mut(id)
                            {
                                *class_name = "Ljava/lang/reflect/Constructor;".into();
                            }
                        }
                        values.push(Some(id));
                    }
                    current = if name.starts_with("getDeclared") || constructors {
                        None
                    } else {
                        definition.superclass.clone()
                    };
                } else {
                    break;
                }
            }
            let ty = if constructors {
                "[Ljava/lang/reflect/Constructor;"
            } else {
                "[Ljava/lang/reflect/Method;"
            };
            let id = jni_state().lock().unwrap().heap.alloc(HeapObject::Array {
                type_desc: ty.into(),
                data: ArrayData::Object(values),
            });
            return Ok(Some(Value::Ref(Some(id))));
        }

        if class == "Ljava/lang/Class;"
            && matches!(
                name,
                "getField" | "getDeclaredField" | "getDeclaredFields" | "getFields"
            )
        {
            let declaring = class_desc(receiver.ok_or("NullPointerException: null Class")?)?;
            let wanted = if name.ends_with("Fields") {
                None
            } else {
                Some(text(
                    args.get(1)
                        .and_then(Value::as_ref)
                        .ok_or("NullPointerException: null field name")?,
                )?)
            };
            if declaring == "Lsun/misc/Unsafe;" && wanted.as_deref() == Some("theUnsafe") {
                return Ok(Some(Value::Ref(Some(self.reflected_field(
                    "Lsun/misc/Unsafe;",
                    "theUnsafe",
                    "Lsun/misc/Unsafe;",
                    0x9,
                )))));
            }
            let mut current = Some(declaring.clone());
            let mut fields = Vec::new();
            while let Some(c) = current {
                for (field_name, ty, flags) in self.reflected_fields(&c) {
                    if name.starts_with("getDeclared") || flags & 1 != 0 {
                        if wanted.as_ref().is_none_or(|w| w == &field_name) {
                            fields.push(Some(self.reflected_field(&c, &field_name, &ty, flags)));
                            if wanted.is_some() {
                                return Ok(Some(Value::Ref(fields[0])));
                            }
                        }
                    }
                }
                current = if name.starts_with("getDeclared") {
                    None
                } else {
                    self.find_class(&c)
                        .and_then(|(_, d)| d.superclass.clone())
                        .or_else(|| crate::stdlib::builtin_superclass(&c).map(str::to_owned))
                };
            }
            if let Some(wanted) = wanted {
                return Err(format!("NoSuchFieldException: {declaring}.{wanted}"));
            }
            let array = jni_state().lock().unwrap().heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/lang/reflect/Field;".into(),
                data: ArrayData::Object(fields),
            });
            return Ok(Some(Value::Ref(Some(array))));
        }
        if class == "Ljava/lang/reflect/Field;" {
            let id = receiver.ok_or("NullPointerException: null Field")?;
            let (declaring, field_name, ty, flags) = method_info(id)?;
            match name {
                "getName" => return Ok(Some(get(id, "name"))),
                "getDeclaringClass" => return Ok(Some(get(id, "declaringClass"))),
                "getModifiers" => return Ok(Some(Value::Int(flags as i32))),
                "isSynthetic" => return Ok(Some(Value::Int(((flags & 0x1000) != 0) as i32))),
                "getAnnotation" => {
                    let Some(ann_class_id) = args.get(1).and_then(Value::as_ref) else {
                        return Ok(Some(Value::Ref(None)));
                    };
                    let ann_desc = class_desc(ann_class_id)?;
                    let found = self.find_class(&declaring).and_then(|(di, cls)| {
                        let field_idx = cls
                            .static_fields
                            .iter()
                            .chain(cls.instance_fields.iter())
                            .find(|ef| self.dexes[di].fields[ef.field_idx].name == field_name)
                            .map(|ef| ef.field_idx)?;
                        cls.field_annotations
                            .get(&field_idx)?
                            .iter()
                            .find(|a| a.type_name == ann_desc)
                            .cloned()
                    });
                    if let Some(ann) = found {
                        let mut fields = HashMap::new();
                        fields.insert("__pengu_annotation".into(), Value::Int(1));
                        let mut st = jni_state().lock().unwrap();
                        for (k, v) in ann.elements {
                            let val = match v {
                                crate::dex::AnnotationValue::String(s) => Value::Ref(Some(st.heap.new_string(s))),
                                crate::dex::AnnotationValue::StringArray(items) => {
                                    let refs = items.into_iter().map(|s| Some(st.heap.new_string(s))).collect();
                                    let arr = st.heap.alloc(HeapObject::Array {
                                        type_desc: "[Ljava/lang/String;".into(),
                                        data: ArrayData::Object(refs),
                                    });
                                    Value::Ref(Some(arr))
                                }
                                crate::dex::AnnotationValue::Int(i) => Value::Int(i),
                                crate::dex::AnnotationValue::Boolean(b) => Value::Int(i32::from(b)),
                                crate::dex::AnnotationValue::Other => Value::Ref(None),
                            };
                            fields.insert(format!("elem:{k}"), val);
                        }
                        let proxy = st.heap.alloc(HeapObject::Instance { class_name: ann_desc, fields });
                        return Ok(Some(Value::Ref(Some(proxy))));
                    }
                    return Ok(Some(Value::Ref(None)));
                }
                "getType" | "getGenericType" => {
                    return Ok(Some(Value::Ref(Some(
                        jni_state().lock().unwrap().heap.class_object(&ty),
                    ))));
                }
                "setAccessible" => return Ok(Some(Value::Void)),
                "get" | "getInt" | "getLong" | "getBoolean" | "getFloat" | "getDouble" => {
                    let value = if flags & 8 != 0 {
                        self.ensure_clinit(&declaring)?;
                        let key = format!("{declaring}.{field_name}");
                        self.threads
                            .statics
                            .lock()
                            .unwrap()
                            .get(&key)
                            .cloned()
                            .or_else(|| self.static_fields.get(&key).cloned())
                    } else {
                        let target = args
                            .get(1)
                            .and_then(Value::as_ref)
                            .ok_or("NullPointerException: null Field receiver")?;
                        if !self.is_instance_of(target, &declaring) {
                            return Err("IllegalArgumentException: wrong Field receiver".into());
                        }
                        let state = jni_state().lock().unwrap();
                        match state.heap.get(target) {
                            Some(HeapObject::Instance { fields, .. }) => {
                                fields.get(&field_name).cloned()
                            }
                            _ => None,
                        }
                    }
                    .unwrap_or_else(|| match ty.as_str() {
                        "J" => Value::Long(0),
                        "F" => Value::Float(0.0),
                        "D" => Value::Double(0.0),
                        _ if ty.len() == 1 => Value::Int(0),
                        _ => Value::Ref(None),
                    });
                    return Ok(Some(if name == "get" {
                        if ty == "Z" {
                            Value::Ref(Some(object(
                                "Ljava/lang/Boolean;",
                                HashMap::from([("value".into(), value)]),
                            )))
                        } else {
                            box_value(value)
                        }
                    } else {
                        value
                    }));
                }
                "set" | "setInt" | "setLong" | "setBoolean" | "setFloat" | "setDouble" => {
                    let raw_val = args.get(2).cloned().unwrap_or(Value::Ref(None));
                    let value = if matches!(ty.as_str(), "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D") {
                        if name == "set" {
                            unbox(raw_val, &ty)?
                        } else {
                            raw_val
                        }
                    } else {
                        raw_val
                    };
                    if flags & 8 != 0 {
                        self.ensure_clinit(&declaring)?;
                        let key = format!("{declaring}.{field_name}");
                        self.threads.statics.lock().unwrap().insert(key.clone(), value.clone());
                        self.static_fields.insert(key, value);
                    } else {
                        let target = args
                            .get(1)
                            .and_then(Value::as_ref)
                            .ok_or("NullPointerException: null Field receiver")?;
                        if !self.is_instance_of(target, &declaring) {
                            return Err("IllegalArgumentException: wrong Field receiver".into());
                        }
                        let mut st = jni_state().lock().unwrap();
                        if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(target) {
                            fields.insert(field_name, value);
                        }
                    }
                    return Ok(Some(Value::Void));
                }
                _ => {}
            }
        }
        if class == "Ljava/lang/reflect/Proxy;" && name == "newProxyInstance" {
            let interfaces = args
                .get(1)
                .and_then(Value::as_ref)
                .ok_or("null proxy interface array")?;
            array(interfaces)?;
            let handler = args
                .get(2)
                .and_then(Value::as_ref)
                .ok_or("null InvocationHandler")?;
            return Ok(Some(Value::Ref(Some(object(
                "Ljava/lang/reflect/Proxy;",
                HashMap::from([
                    ("h".into(), Value::Ref(Some(handler))),
                    ("interfaces".into(), Value::Ref(Some(interfaces))),
                ]),
            )))));
        }
        if class == "Ljava/lang/reflect/Proxy;" && name == "getInvocationHandler" {
            return Ok(Some(get(
                args.first().and_then(Value::as_ref).ok_or("null proxy")?,
                "h",
            )));
        }
        if virtual_call {
            if let Some(proxy) = receiver {
                if let Some(handler) = get(proxy, "h").as_ref().filter(|_| {
                    jni_state()
                        .lock()
                        .unwrap()
                        .heap
                        .get(proxy)
                        .is_some_and(|o| o.class_name() == "Ljava/lang/reflect/Proxy;")
                }) {
                    let interfaces = get(proxy, "interfaces")
                        .as_ref()
                        .map(array)
                        .transpose()?
                        .unwrap_or_default();
                    let iface_descs: Vec<String> = interfaces
                        .iter()
                        .flatten()
                        .filter_map(|id| class_desc(*id).ok())
                        .collect();
                    let declaring = iface_descs
                        .iter()
                        .find(|iface| {
                            self.find_method_in_hierarchy(iface, name, sig, true).is_some()
                                || matches!(
                                    (iface.as_str(), name, sig),
                                    ("Ljava/lang/Runnable;", "run", "()V")
                                        | ("Ljava/util/concurrent/Callable;", "call", "()Ljava/lang/Object;")
                                        | ("Landroid/view/Choreographer$FrameCallback;", "doFrame", "(J)V")
                                        | ("Landroid/os/Handler$Callback;", "handleMessage", "(Landroid/os/Message;)Z")
                                        | ("Landroid/view/View$OnSystemUiVisibilityChangeListener;", "onSystemUiVisibilityChange", "(I)V")
                                        | ("Landroid/view/View$OnClickListener;", "onClick", "(Landroid/view/View;)V")
                                        | ("Landroid/content/DialogInterface$OnClickListener;", "onClick", "(Landroid/content/DialogInterface;I)V")
                                )
                        })
                        .cloned()
                        .or_else(|| iface_descs.into_iter().next())
                        .unwrap_or_else(|| "Ljava/lang/Object;".into());
                    let reflected = method(&declaring, name, sig, 1);
                    let parameters = parameter_types(sig);
                    let mut boxed = Vec::new();
                    for (value, ty) in args[1..].iter().zip(&parameters) {
                        let boxed_value = if ty == "Z" {
                            Value::Ref(Some(object(
                                "Ljava/lang/Boolean;",
                                HashMap::from([("value".into(), value.clone())]),
                            )))
                        } else {
                            box_value(value.clone())
                        };
                        boxed.push(boxed_value.as_ref());
                    }
                    let values = if boxed.is_empty() {
                        None
                    } else {
                        Some(jni_state().lock().unwrap().heap.alloc(HeapObject::Array {
                            type_desc: "[Ljava/lang/Object;".into(),
                            data: ArrayData::Object(boxed),
                        }))
                    };
                    let handler_class = jni_state()
                        .lock()
                        .unwrap()
                        .heap
                        .get(handler)
                        .ok_or("invalid InvocationHandler")?
                        .class_name()
                        .to_string();
                    let result=self.invoke_method(&handler_class,"invoke","(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",&[Value::Ref(Some(handler)),Value::Ref(Some(proxy)),Value::Ref(Some(reflected)),Value::Ref(values)],true)?;
                    return unbox(
                        result,
                        sig.split(')').nth(1).ok_or("invalid proxy descriptor")?,
                    )
                    .map(Some);
                }
            }
        }
        if matches!(
            class,
            "Ljava/lang/reflect/Method;" | "Ljava/lang/reflect/Constructor;"
        ) {
            let id = receiver.ok_or("null Method")?;
            let (declaring, method_name, descriptor, flags) = method_info(id)?;
            if class == "Ljava/lang/reflect/Constructor;" && name == "newInstance" {
                let supplied = args
                    .get(1)
                    .and_then(Value::as_ref)
                    .map(array)
                    .transpose()?
                    .unwrap_or_default();
                let parameters = parameter_types(&descriptor);
                if parameters.len() != supplied.len() {
                    return Err("IllegalArgumentException: wrong constructor argument count".into());
                }
                let target = self.new_instance(&declaring)?;
                let mut values = vec![Value::Ref(Some(target))];
                for (ty, value) in parameters.iter().zip(supplied) {
                    values.push(unbox(Value::Ref(value), ty)?);
                }
                self.invoke_method(&declaring, "<init>", &descriptor, &values, false)?;
                return Ok(Some(Value::Ref(Some(target))));
            }
            match name {
                "getName" => return Ok(Some(Value::Ref(Some(string(&method_name))))),
                "getDeclaringClass" => return Ok(Some(get(id, "declaringClass"))),
                "getModifiers" => return Ok(Some(Value::Int(flags as i32))),
                "isSynthetic" => return Ok(Some(Value::Int(((flags & 0x1000) != 0) as i32))),
                "getAnnotation" => return Ok(Some(Value::Ref(None))),
                "setAccessible" => return Ok(Some(Value::Void)),
                "getReturnType" => {
                    let ty = descriptor
                        .split(')')
                        .nth(1)
                        .ok_or("invalid Method descriptor")?;
                    return Ok(Some(Value::Ref(Some(
                        jni_state().lock().unwrap().heap.class_object(ty),
                    ))));
                }
                "getParameterTypes" => {
                    let params = parameter_types(&descriptor);
                    let mut state = jni_state().lock().unwrap();
                    let values = params
                        .iter()
                        .map(|ty| Some(state.heap.class_object(ty)))
                        .collect();
                    let id = state.heap.alloc(HeapObject::Array {
                        type_desc: "[Ljava/lang/Class;".into(),
                        data: ArrayData::Object(values),
                    });
                    return Ok(Some(Value::Ref(Some(id))));
                }
                "invoke" => {
                    let mut values = Vec::new();
                    if flags & 8 == 0 {
                        values.push(args.get(1).cloned().ok_or("null reflection receiver")?);
                    }
                    let parameters = parameter_types(&descriptor);
                    let supplied = args
                        .get(2)
                        .and_then(Value::as_ref)
                        .map(array)
                        .transpose()?
                        .unwrap_or_default();
                    if parameters.len() != supplied.len() {
                        return Err(
                            "IllegalArgumentException: wrong reflection argument count".into()
                        );
                    }
                    for (ty, value) in parameters.iter().zip(supplied) {
                        values.push(unbox(Value::Ref(value), ty)?);
                    }
                    let result = self.invoke_method(
                        &declaring,
                        &method_name,
                        &descriptor,
                        &values,
                        flags & 8 == 0,
                    )?;
                    return Ok(Some(if descriptor.ends_with('V') {
                        Value::Ref(None)
                    } else {
                        box_value(result)
                    }));
                }
                _ => {}
            }
        }
        if matches!(
            class,
            "Ljava/lang/reflect/ParameterizedType;" | "Lpengu/reflect/ParameterizedTypeImpl;"
        ) {
            let id = receiver.ok_or("null ParameterizedType")?;
            let st = jni_state().lock().unwrap();
            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get(id) {
                match name {
                    "getRawType" => {
                        return Ok(Some(fields.get("rawType").cloned().unwrap_or(Value::Ref(None))));
                    }
                    "getActualTypeArguments" => {
                        return Ok(Some(fields.get("args").cloned().unwrap_or(Value::Ref(None))));
                    }
                    "getOwnerType" => {
                        return Ok(Some(fields.get("owner").cloned().unwrap_or(Value::Ref(None))));
                    }
                    _ => {}
                }
            }
        }
        if matches!(
            class,
            "Ljava/lang/reflect/TypeVariable;" | "Lpengu/reflect/TypeVariableImpl;"
        ) {
            let id = receiver.ok_or("null TypeVariable")?;
            let st = jni_state().lock().unwrap();
            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get(id) {
                match name {
                    "getName" => {
                        return Ok(Some(fields.get("name").cloned().unwrap_or(Value::Ref(None))));
                    }
                    "getGenericDeclaration" => {
                        return Ok(Some(fields.get("decl").cloned().unwrap_or(Value::Ref(None))));
                    }
                    "getBounds" => {
                        return Ok(Some(fields.get("bounds").cloned().unwrap_or(Value::Ref(None))));
                    }
                    "equals" => {
                        let other = args.get(1).and_then(Value::as_ref);
                        let eq = other.is_some_and(|oid| {
                            if oid == id {
                                return true;
                            }
                            let Some(HeapObject::Instance {
                                class_name: ocls,
                                fields: ofields,
                            }) = st.heap.get(oid)
                            else {
                                return false;
                            };
                            ocls == "Lpengu/reflect/TypeVariableImpl;"
                                && fields.get("decl").and_then(Value::as_ref)
                                    == ofields.get("decl").and_then(Value::as_ref)
                                && fields
                                    .get("name")
                                    .and_then(Value::as_ref)
                                    .and_then(|s| st.heap.as_str(s))
                                    == ofields
                                        .get("name")
                                        .and_then(Value::as_ref)
                                        .and_then(|s| st.heap.as_str(s))
                        });
                        return Ok(Some(Value::Int(i32::from(eq))));
                    }
                    _ => {}
                }
            }
        }
        if class == "Ljava/lang/Class;"
            && matches!(
                name,
                "isPrimitive"
                    | "isInterface"
                    | "isArray"
                    | "getModifiers"
                    | "getEnclosingClass"
                    | "getSuperclass"
                    | "getGenericSuperclass"
                    | "getInterfaces"
                    | "getGenericInterfaces"
                    | "getTypeParameters"
                    | "isInstance"
                    | "isAssignableFrom"
                    | "cast"
                    | "isLocalClass"
                    | "isAnonymousClass"
                    | "getAnnotation"
                    | "getMethod"
                    | "getDeclaredMethod"
                    | "asSubclass"
            )
        {
            let id = receiver.ok_or("null Class")?;
            let declaring = class_desc(id)?;
            match name {
                "asSubclass" => {
                    let target = class_desc(
                        args.get(1)
                            .and_then(Value::as_ref)
                            .ok_or("NullPointerException: null superclass")?,
                    )?;
                    if !self.is_assignable_from(&target, &declaring) {
                        return Err("ClassCastException: class is not a subclass".into());
                    }
                    return Ok(Some(Value::Ref(Some(id))));
                }
                "isInstance" => {
                    return Ok(Some(Value::Int(
                        args.get(1)
                            .and_then(Value::as_ref)
                            .is_some_and(|id| self.is_instance_of(id, &declaring))
                            as i32,
                    )));
                }
                "cast" => {
                    let obj = args.get(1).cloned().unwrap_or(Value::Ref(None));
                    if let Value::Ref(Some(oid)) = obj {
                        if !self.is_instance_of(oid, &declaring) {
                            return Err(format!("ClassCastException: cannot cast to {declaring}"));
                        }
                    }
                    return Ok(Some(obj));
                }
                "isLocalClass" => return Ok(Some(Value::Int(0))),
                "isAnonymousClass" => {
                    let anon = declaring
                        .rsplit('$')
                        .next()
                        .is_some_and(|s| s.trim_end_matches(';').chars().next().is_some_and(|c| c.is_ascii_digit()));
                    return Ok(Some(Value::Int(anon as i32)));
                }
                "getAnnotation" => return Ok(Some(Value::Ref(None))),
                "isAssignableFrom" => {
                    let source = class_desc(
                        args.get(1)
                            .and_then(Value::as_ref)
                            .ok_or("NullPointerException: null source class")?,
                    )?;
                    return Ok(Some(Value::Int(
                        self.is_assignable_from(&declaring, &source) as i32,
                    )));
                }
                "isPrimitive" => return Ok(Some(Value::Int((declaring.len() == 1) as i32))),
                "isArray" => return Ok(Some(Value::Int(declaring.starts_with('[') as i32))),
                "getModifiers" => {
                    let flags = self
                        .find_class(&declaring)
                        .map_or(1, |(_, c)| c.access_flags as i32);
                    return Ok(Some(Value::Int(flags)));
                }
                "getEnclosingClass" => return Ok(Some(Value::Ref(None))),
                "isInterface" => {
                    let is_iface = self
                        .find_class(&declaring)
                        .is_some_and(|(_, c)| c.access_flags & 0x200 != 0)
                        || crate::stdlib::is_builtin_interface(&declaring);
                    return Ok(Some(Value::Int(is_iface as i32)));
                }
                "getSuperclass" => {
                    let parent = self
                        .find_class(&declaring)
                        .and_then(|(_, c)| c.superclass.clone())
                        .or_else(|| {
                            crate::stdlib::builtin_superclass(&declaring).map(str::to_owned)
                        });
                    let value = parent.map(|p| jni_state().lock().unwrap().heap.class_object(&p));
                    return Ok(Some(Value::Ref(value)));
                }
                "getGenericSuperclass" => {
                    let (sig_opt, parent) = match self.find_class(&declaring) {
                        Some((_, c)) => (c.signature.clone(), c.superclass.clone()),
                        None => (
                            None,
                            crate::stdlib::builtin_superclass(&declaring).map(str::to_owned),
                        ),
                    };
                    let mut st = jni_state().lock().unwrap();
                    if let Some(sig) = sig_opt {
                        let mut s = sig.as_str();
                        if s.starts_with('<') {
                            let mut depth = 0usize;
                            for (idx, ch) in s.char_indices() {
                                match ch {
                                    '<' => depth += 1,
                                    '>' => {
                                        depth -= 1;
                                        if depth == 0 {
                                            s = &s[idx + 1..];
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if let Some((ty_val, _)) = parse_generic_type(&mut st.heap, s) {
                            return Ok(Some(ty_val));
                        }
                    }
                    let value = parent.map(|p| st.heap.class_object(&p));
                    return Ok(Some(Value::Ref(value)));
                }
                "getInterfaces" => {
                    let ifaces: Vec<String> = match self.find_class(&declaring) {
                        Some((_, c)) => c.interfaces.clone(),
                        None => crate::stdlib::builtin_interfaces(&declaring)
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect(),
                    };
                    let mut st = jni_state().lock().unwrap();
                    let objs: Vec<Option<usize>> =
                        ifaces.iter().map(|i| Some(st.heap.class_object(i))).collect();
                    let arr = st.heap.alloc(crate::HeapObject::Array {
                        type_desc: "[Ljava/lang/Class;".into(),
                        data: crate::ArrayData::Object(objs),
                    });
                    return Ok(Some(Value::Ref(Some(arr))));
                }
                "getGenericInterfaces" => {
                    let ifaces: Vec<String> = match self.find_class(&declaring) {
                        Some((_, c)) => c.interfaces.clone(),
                        None => crate::stdlib::builtin_interfaces(&declaring)
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect(),
                    };
                    let mut st = jni_state().lock().unwrap();
                    let mut objs = Vec::with_capacity(ifaces.len());
                    for iface in &ifaces {
                        if matches!(
                            declaring.as_str(),
                            "Ljava/util/List;" | "Ljava/util/Set;" | "Ljava/util/Queue;"
                        ) && iface == "Ljava/util/Collection;"
                        {
                            let raw_cls = st.heap.class_object(iface);
                            let tv = make_type_variable(&mut st.heap, id, "E");
                            let args_arr = st.heap.alloc(crate::HeapObject::Array {
                                type_desc: "[Ljava/lang/reflect/Type;".into(),
                                data: crate::ArrayData::Object(vec![Some(tv)]),
                            });
                            let pt = st.heap.new_instance("Lpengu/reflect/ParameterizedTypeImpl;");
                            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(pt) {
                                fields.insert("rawType".into(), Value::Ref(Some(raw_cls)));
                                fields.insert("args".into(), Value::Ref(Some(args_arr)));
                                fields.insert("owner".into(), Value::Ref(None));
                            }
                            objs.push(Some(pt));
                        } else {
                            objs.push(Some(st.heap.class_object(iface)));
                        }
                    }
                    let arr = st.heap.alloc(crate::HeapObject::Array {
                        type_desc: "[Ljava/lang/reflect/Type;".into(),
                        data: crate::ArrayData::Object(objs),
                    });
                    return Ok(Some(Value::Ref(Some(arr))));
                }
                "getTypeParameters" => {
                    let mut st = jni_state().lock().unwrap();
                    let objs = if matches!(
                        declaring.as_str(),
                        "Ljava/util/List;"
                            | "Ljava/util/Set;"
                            | "Ljava/util/Queue;"
                            | "Ljava/util/Collection;"
                    ) {
                        vec![Some(make_type_variable(&mut st.heap, id, "E"))]
                    } else {
                        vec![]
                    };
                    let arr = st.heap.alloc(crate::HeapObject::Array {
                        type_desc: "[Ljava/lang/reflect/TypeVariable;".into(),
                        data: crate::ArrayData::Object(objs),
                    });
                    return Ok(Some(Value::Ref(Some(arr))));
                }
                "getMethod" | "getDeclaredMethod" => {
                    let method_name = text(
                        args.get(1)
                            .and_then(Value::as_ref)
                            .ok_or("null method name")?,
                    )?;
                    if declaring == "Lsun/misc/Unsafe;" && method_name == "allocateInstance" {
                        return Ok(Some(Value::Ref(Some(method(
                            "Lsun/misc/Unsafe;",
                            "allocateInstance",
                            "(Ljava/lang/Class;)Ljava/lang/Object;",
                            0x1,
                        )))));
                    }
                    let parameters = args
                        .get(2)
                        .and_then(Value::as_ref)
                        .map(array)
                        .transpose()?
                        .unwrap_or_default();
                    let mut prefix = "(".to_string();
                    for parameter in parameters {
                        prefix.push_str(&class_desc(parameter.ok_or("null parameter class")?)?)
                    }
                    prefix.push(')');
                    let mut current = Some(declaring.clone());
                    let mut found = None;
                    while let Some(c) = current {
                        if let Some((di, definition)) = self.find_class(&c) {
                            for encoded in definition
                                .direct_methods
                                .iter()
                                .chain(&definition.virtual_methods)
                            {
                                let method = &self.dexes[di].methods[encoded.method_idx];
                                if method.name == method_name
                                    && method.descriptor.starts_with(&prefix)
                                {
                                    found = Some((
                                        c.clone(),
                                        method.descriptor.clone(),
                                        encoded.access_flags,
                                    ));
                                    break;
                                }
                            }
                            current = if name == "getDeclaredMethod" {
                                None
                            } else {
                                definition.superclass.clone()
                            };
                        } else {
                            break;
                        }
                        if found.is_some() {
                            break;
                        }
                    }
                    if let Some((class, sig, flags)) = found {
                        return Ok(Some(Value::Ref(Some(method(
                            &class,
                            &method_name,
                            &sig,
                            flags,
                        )))));
                    }
                    return Err(format!(
                        "NoSuchMethodException: {declaring}.{method_name}{prefix}"
                    ));
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

fn parse_generic_type<'a>(heap: &mut crate::Heap, s: &'a str) -> Option<(Value, &'a str)> {
    let s = s.trim_start_matches(['+', '-']);
    if s.starts_with('*') {
        let cls = heap.class_object("Ljava/lang/Object;");
        return Some((Value::Ref(Some(cls)), &s[1..]));
    }
    if s.starts_with('[') {
        let mut end = 0usize;
        let bytes = s.as_bytes();
        while end < bytes.len() && bytes[end] == b'[' {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'L' {
            let semi = s[end..].find(';')?;
            let desc = &s[..end + semi + 1];
            let cls = heap.class_object(desc);
            return Some((Value::Ref(Some(cls)), &s[end + semi + 1..]));
        } else if end < bytes.len() {
            let desc = &s[..end + 1];
            let cls = heap.class_object(desc);
            return Some((Value::Ref(Some(cls)), &s[end + 1..]));
        }
        return None;
    }
    if !s.starts_with('L') {
        return None;
    }
    let mut depth = 0usize;
    let mut lt_pos = None;
    let mut semi_pos = None;
    for (idx, ch) in s[1..].char_indices() {
        let pos = 1 + idx;
        match ch {
            '<' => {
                if depth == 0 && lt_pos.is_none() {
                    lt_pos = Some(pos);
                }
                depth += 1;
            }
            '>' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                semi_pos = Some(pos);
                break;
            }
            _ => {}
        }
    }
    let semi = semi_pos?;
    let rest = &s[semi + 1..];
    let Some(lt) = lt_pos else {
        let raw_desc = &s[..=semi];
        let cls = heap.class_object(raw_desc);
        return Some((Value::Ref(Some(cls)), rest));
    };
    let raw_desc = format!("{};", &s[..lt]);
    let raw_cls = heap.class_object(&raw_desc);
    let mut arg_vals = Vec::new();
    let mut inner = &s[lt + 1..semi - 1];
    while !inner.is_empty() {
        if inner.starts_with('>') {
            break;
        }
        let Some((val, next)) = parse_generic_type(heap, inner) else {
            break;
        };
        arg_vals.push(val.as_ref());
        inner = next;
    }
    let arr_id = heap.alloc(crate::HeapObject::Array {
        type_desc: "[Ljava/lang/reflect/Type;".into(),
        data: crate::ArrayData::Object(arg_vals),
    });
    let pt = heap.new_instance("Lpengu/reflect/ParameterizedTypeImpl;");
    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(pt) {
        fields.insert("rawType".into(), Value::Ref(Some(raw_cls)));
        fields.insert("args".into(), Value::Ref(Some(arr_id)));
        fields.insert("owner".into(), Value::Ref(None));
    }
    Some((Value::Ref(Some(pt)), rest))
}

fn make_type_variable(heap: &mut crate::Heap, decl_cls: usize, name: &str) -> usize {
    let name_id = heap.new_string(name.to_string());
    let obj_cls = heap.class_object("Ljava/lang/Object;");
    let bounds = heap.alloc(crate::HeapObject::Array {
        type_desc: "[Ljava/lang/reflect/Type;".into(),
        data: crate::ArrayData::Object(vec![Some(obj_cls)]),
    });
    let tv = heap.new_instance("Lpengu/reflect/TypeVariableImpl;");
    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(tv) {
        fields.insert("name".into(), Value::Ref(Some(name_id)));
        fields.insert("decl".into(), Value::Ref(Some(decl_cls)));
        fields.insert("bounds".into(), Value::Ref(Some(bounds)));
    }
    tv
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn handler(
        _env: *mut std::ffi::c_void,
        _receiver: *mut std::ffi::c_void,
        _proxy: *mut std::ffi::c_void,
        reflected: *mut std::ffi::c_void,
        arguments: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void {
        let (declaring, name, sig, _) = method_info(reflected as usize).unwrap();
        assert_eq!(declaring, "Landroid/os/Handler$Callback;");
        assert_eq!(name, "handleMessage");
        assert_eq!(sig, "(Landroid/os/Message;)Z");
        assert_eq!(array(arguments as usize).unwrap().len(), 1);
        object(
            "Ljava/lang/Boolean;",
            HashMap::from([("value".into(), Value::Int(1))]),
        ) as *mut std::ffi::c_void
    }
    #[test]
    fn proxy_invokes_the_real_invocation_handler_with_method_and_argument_metadata() {
        let mut vm = Vm::new();
        crate::jni_host::register_native(
            "Lpengu/test/ProxyHandler;",
            "invoke",
            "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            handler as *const () as usize,
        );
        let callback = object("Lpengu/test/ProxyHandler;", HashMap::new());
        let interface = jni_state()
            .lock()
            .unwrap()
            .heap
            .class_object("Landroid/os/Handler$Callback;");
        let interfaces = jni_state().lock().unwrap().heap.alloc(HeapObject::Array {
            type_desc: "[Ljava/lang/Class;".into(),
            data: ArrayData::Object(vec![Some(interface)]),
        });
        let proxy=vm.invoke_method("Ljava/lang/reflect/Proxy;","newProxyInstance","(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)Ljava/lang/Object;",&[Value::Ref(None),Value::Ref(Some(interfaces)),Value::Ref(Some(callback))],false).unwrap().as_ref().unwrap();
        assert!(vm.is_instance_of(proxy, "Landroid/os/Handler$Callback;"));
        assert!(!vm.is_instance_of(proxy, "Ljava/lang/Runnable;"));
        let msg = object("Landroid/os/Message;", HashMap::new());
        let result = vm
            .invoke_method(
                "Landroid/os/Handler$Callback;",
                "handleMessage",
                "(Landroid/os/Message;)Z",
                &[Value::Ref(Some(proxy)), Value::Ref(Some(msg))],
                true,
            )
            .unwrap();
        assert_eq!(result.as_int().unwrap(), 1);
    }
}
