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
                "getType" => {
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
                    let declaring = interfaces
                        .iter()
                        .flatten()
                        .find_map(|id| class_desc(*id).ok())
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
        if class == "Ljava/lang/Class;"
            && matches!(
                name,
                "isPrimitive"
                    | "isInterface"
                    | "getSuperclass"
                    | "isInstance"
                    | "isAssignableFrom"
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
                "isInterface" => {
                    return Ok(Some(Value::Int(
                        self.find_class(&declaring)
                            .is_some_and(|(_, c)| c.access_flags & 0x200 != 0)
                            as i32,
                    )));
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
                "getMethod" | "getDeclaredMethod" => {
                    let method_name = text(
                        args.get(1)
                            .and_then(Value::as_ref)
                            .ok_or("null method name")?,
                    )?;
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
