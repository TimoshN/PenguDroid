//! Dalvik bytecode VM, class loader, and method dispatcher.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::dex::{ClassDef, CodeItem, DexFile, EncodedMethod};
use crate::jni_host::{jni_env_ptr, jni_state, load_native_library, lookup_native};
use crate::stdlib::{builtin_superclass, init_builtin_statics, try_invoke_builtin, BuiltinResult};
use crate::value::{ArrayData, HeapObject, ObjId, Value};

pub type NativeLoaderFn = Box<dyn FnMut(&str) -> Result<(), String>>;
pub type SymbolResolverFn = Box<dyn FnMut(&str, &str) -> Option<usize>>;
pub type PointerWrapperFn = Box<dyn FnMut(usize, &str) -> Option<usize>>;

#[derive(Clone)]
pub(crate) struct IndexedClass {
    dex: usize,
    class: usize,
    direct: HashMap<String, HashMap<String, usize>>,
    virtual_methods: HashMap<String, HashMap<String, usize>>,
}

pub struct Vm {
    pub(crate) dexes: Vec<Arc<DexFile>>,
    pub(crate) classes: Arc<HashMap<String, IndexedClass>>,
    native_calls: HashMap<String, std::rc::Rc<crate::native_abi::NativeCall>>,
    pub missing_methods: BTreeMap<String, u64>,
    pub static_fields: HashMap<String, Value>,
    pub clinit_done: HashSet<String>,
    pub native_dirs: Vec<PathBuf>,
    pub native_loader: Option<NativeLoaderFn>,
    pub symbol_resolver: Option<SymbolResolverFn>,
    pub pointer_wrapper: Option<PointerWrapperFn>,
    pub wrapped_natives: Arc<Mutex<HashMap<(String, String, String), usize>>>,
    pub pending_exception: Option<ObjId>,
    pub trace: bool,
    pub(crate) threads: Arc<crate::threads::ThreadRuntime>,
    pub(crate) needs_bridge: bool,
    pub(crate) owns_runtime: bool,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    pub fn new() -> Self {
        let mut static_fields = HashMap::new();
        {
            let mut st = jni_state().lock().unwrap();
            init_builtin_statics(&mut st.heap, &mut static_fields);
        }
        for (k, v) in &static_fields {
            crate::jni_host::set_static_field(k, v.clone());
        }
        Self {
            dexes: Vec::new(),
            classes: Arc::new(HashMap::new()),
            native_calls: HashMap::new(),
            missing_methods: BTreeMap::new(),
            static_fields,
            clinit_done: HashSet::new(),
            native_dirs: Vec::new(),
            native_loader: None,
            symbol_resolver: None,
            pointer_wrapper: None,
            wrapped_natives: Arc::new(Mutex::new(HashMap::new())),
            pending_exception: None,
            threads: Arc::new(crate::threads::ThreadRuntime::default()),
            needs_bridge: false,
            owns_runtime: true,
            trace: std::env::var_os("PENGUDROID_TRACE_VM").is_some(),
        }
    }

    pub fn set_native_loader(&mut self, loader: NativeLoaderFn) {
        self.native_loader = Some(loader);
    }

    pub fn set_symbol_resolver(&mut self, resolver: SymbolResolverFn) {
        self.symbol_resolver = Some(resolver);
    }

    pub fn set_pointer_wrapper(&mut self, wrapper: PointerWrapperFn) {
        self.needs_bridge = true;
        self.pointer_wrapper = Some(wrapper);
    }

    pub fn add_native_dir(&mut self, dir: PathBuf) {
        self.native_dirs.push(dir);
    }

    pub fn load_dex(&mut self, bytes: &[u8]) -> Result<(), String> {
        let dex = DexFile::parse(bytes)?;
        self.load_parsed_dex(dex)
    }

    /// Add an already parsed DEX. Class identity follows first-definition wins,
    /// as before; adding a later split cannot replace an existing class.
    pub fn load_parsed_dex(&mut self, dex: DexFile) -> Result<(), String> {
        let di = self.dexes.len();
        let mut indexed = Vec::with_capacity(dex.classes.len());
        for (ci, class) in dex.classes.iter().enumerate() {
            let index_methods = |methods: &[EncodedMethod]| -> Result<HashMap<String, HashMap<String, usize>>, String> {
                let mut index: HashMap<String, HashMap<String, usize>> = HashMap::new();
                for (i, method) in methods.iter().enumerate() {
                    let id = dex.methods.get(method.method_idx).ok_or("invalid encoded method index")?;
                    index.entry(id.name.clone()).or_default().entry(id.descriptor.clone()).or_insert(i);
                }
                Ok(index)
            };
            indexed.push((class.name.clone(), IndexedClass {
                dex: di, class: ci,
                direct: index_methods(&class.direct_methods)?,
                virtual_methods: index_methods(&class.virtual_methods)?,
            }));
        }
        for (name, index) in indexed {
            if !self.classes.contains_key(&name) {
                self.clinit_done.remove(&name);
                Arc::make_mut(&mut self.classes).insert(name, index);
            }
        }
        self.dexes.push(Arc::new(dex));
        Ok(())
    }

    pub fn set_package_name(&mut self, pkg: &str) {
        let mut st = jni_state().lock().unwrap();
        let sid = st.heap.new_string(pkg.to_string());
        self.static_fields
            .insert("PENGUDROID_PACKAGE_NAME".into(), Value::Ref(Some(sid)));
    }

    pub fn set_runtime_paths(&mut self, native_lib_dir: &str, apk_path: &str, data_dir: &str) {
        let mut st = jni_state().lock().unwrap();
        let l = st.heap.new_string(native_lib_dir.to_string());
        let a = st.heap.new_string(apk_path.to_string());
        let d = st.heap.new_string(data_dir.to_string());
        self.static_fields.insert("PENGUDROID_NATIVE_LIB_DIR".into(), Value::Ref(Some(l)));
        self.static_fields.insert("PENGUDROID_APK_PATH".into(), Value::Ref(Some(a)));
        self.static_fields.insert("PENGUDROID_DATA_DIR".into(), Value::Ref(Some(d)));
    }

    pub fn set_split_apks(&mut self, paths: &[String]) {
        let mut state = jni_state().lock().unwrap();
        let values = paths.iter().map(|path| Some(state.heap.new_string(path.clone()))).collect();
        let array = state.heap.alloc(HeapObject::Array {
            type_desc: "[Ljava/lang/String;".into(), data: ArrayData::Object(values),
        });
        self.static_fields.insert("PENGUDROID_SPLIT_APKS".into(), Value::Ref(Some(array)));
    }

    pub fn find_class(&self, class_name: &str) -> Option<(usize, &ClassDef)> {
        let index = self.classes.get(class_name)?;
        Some((index.dex, &self.dexes[index.dex].classes[index.class]))
    }

    pub fn is_assignable_from(&self,target:&str,source:&str)->bool {
        if target==source {return true;}
        if source.starts_with('[') {
            if matches!(target,"Ljava/lang/Object;" | "Ljava/lang/Cloneable;" | "Ljava/io/Serializable;"){return true;}
            if let Some(target)=target.strip_prefix('['){let source=&source[1..];return source.len()>1 && target.len()>1 && self.is_assignable_from(target,source);}
            return false;
        }
        if target=="Ljava/lang/Object;" && source.starts_with('L'){return true;}
        let mut pending=vec![source.to_string()];let mut visited=HashSet::new();
        while let Some(class)=pending.pop(){if class==target{return true}if !visited.insert(class.clone()){continue}
            if let Some((_,definition))=self.find_class(&class){pending.extend(definition.interfaces.iter().cloned());if let Some(parent)=&definition.superclass{pending.push(parent.clone());}}
            else {pending.extend(crate::stdlib::builtin_interfaces(&class).iter().map(|s|s.to_string()));if let Some(parent)=builtin_superclass(&class){pending.push(parent.into());}}
        }
        false
    }
    pub fn is_instance_of(&self,object:ObjId,target:&str)->bool {
        let (source,interfaces)={let state=jni_state().lock().unwrap();let Some(source)=state.heap.class_name(object).map(str::to_owned)else{return false};let object=state.heap.get(object).unwrap();
            let interfaces=match object {HeapObject::Instance{fields,..} if source=="Ljava/lang/reflect/Proxy;"=>fields.get("interfaces").and_then(Value::as_ref).and_then(|id|match state.heap.get(id){Some(HeapObject::Array{data:ArrayData::Object(items),..})=>Some(items.iter().flatten().filter_map(|id|match state.heap.get(*id){Some(HeapObject::ClassObj(class))=>Some(class.clone()),_=>None}).collect::<Vec<_>>()),_=>None}).unwrap_or_default(),_=>Vec::new()};(source,interfaces)};
        self.is_assignable_from(target,&source) || interfaces.iter().any(|source|self.is_assignable_from(target,source))
    }

    pub(crate) fn has_dex_method(&self, class: &str, method: &str, descriptor: &str) -> bool {
        self.find_method_in_hierarchy(class, method, descriptor, true).is_some()
    }

    pub(crate) fn find_method_in_hierarchy(
        &self,
        start_class: &str,
        name: &str,
        descriptor: &str,
        is_virtual: bool,
    ) -> Option<(usize, String, EncodedMethod)> {
        let mut cur = Some(start_class.to_string());
        let mut depth = 0usize;
        while let Some(cname) = cur {
            if depth > 64 {
                break;
            }
            depth += 1;
            if let Some((di, cls)) = self.find_class(&cname) {
                let index = self.classes.get(&cname)?;
                let (first, second, first_methods, second_methods) = if is_virtual {
                    (&index.virtual_methods, &index.direct, &cls.virtual_methods, &cls.direct_methods)
                } else {
                    (&index.direct, &index.virtual_methods, &cls.direct_methods, &cls.virtual_methods)
                };
                for (map, methods) in [(first, first_methods), (second, second_methods)] {
                    if let Some(&i) = map.get(name).and_then(|overloads| overloads.get(descriptor)) {
                        return Some((di, cname, methods[i].clone()));
                    }
                }
                cur = cls.superclass.clone();
            } else {
                break;
            }
        }
        None
    }

    pub fn ensure_clinit(&mut self, class_name: &str) -> Result<(), String> {
        if self.clinit_done.contains(class_name) { return Ok(()); }
        if !self.threads.begin_init(class_name)? { return Ok(()); }
        let result = (|| {
            if let Some((di, declaring, method)) = self.find_method_in_hierarchy(class_name,"<clinit>","()V",false) {
                if declaring==class_name { if let Some(code)=method.code {self.exec_code(di,&code,&[])?;} }
            }
            if self.pending_exception.is_some() { return Err(format!("ExceptionInInitializerError: {class_name}: {}",self.describe_exception(self.pending_exception.unwrap()))); }
            Ok(())
        })();
        self.threads.finish_init(class_name,&result);
        if result.is_ok() { self.clinit_done.insert(class_name.to_string()); }
        result
    }

    pub fn all_missing_methods(&self) -> BTreeMap<String,u64> { self.threads.missing_methods.lock().unwrap().clone() }

    fn builtin_exception(&mut self,error:String,descriptor:&str)->Result<Value,String> {
        let name=error.split(':').next().unwrap_or("").rsplit('.').next().unwrap_or("");
        let exc_class = match name {
            "NoSuchMethodException" | "NoSuchFieldException" | "NullPointerException" | "InterruptedException" | "IllegalMonitorStateException" | "IllegalThreadStateException" | "IllegalArgumentException" | "IllegalStateException" | "ClassCastException" | "RuntimeException" => format!("Ljava/lang/{name};"),
            "JSONException" => "Lorg/json/JSONException;".to_string(),
            "NoSuchElementException" => "Ljava/util/NoSuchElementException;".to_string(),
            "FileNotFoundException" | "IOException" => format!("Ljava/io/{name};"),
            _ => return Err(error),
        };
        let mut state=jni_state().lock().unwrap();let id=state.heap.new_instance(&exc_class);let message=state.heap.new_string(error);
        if let Some(HeapObject::Instance{fields,..})=state.heap.get_mut(id){fields.insert("detailMessage".into(),Value::Ref(Some(message)));}
        self.pending_exception=Some(id);Ok(default_return_for_descriptor(descriptor))
    }

    pub fn invoke_method(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
        is_virtual: bool,
    ) -> Result<Value, String> {
        if class_name == "Ljava/util/Collections;" && method_name == "sort" {
            if let Some(list_id) = args.first().and_then(Value::as_ref) {
                let mut items = {
                    let st = jni_state().lock().unwrap();
                    match st.heap.get(list_id) {
                        Some(HeapObject::List(v)) => v.clone(),
                        _ => Vec::new(),
                    }
                };
                if items.len() > 1 {
                    let mut i = 1;
                    while i < items.len() {
                        let mut j = i;
                        while j > 0 {
                            let (a, b) = (&items[j - 1], &items[j]);
                            let cls = a.as_ref().and_then(|id| {
                                jni_state().lock().unwrap().heap.get(id).map(|o| o.class_name().to_string())
                            });
                            let cmp = if let Some(cls) = cls {
                                self.invoke_method(&cls, "compareTo", "(Ljava/lang/Object;)I", &[a.clone(), b.clone()], true)
                                    .ok()
                                    .and_then(|v| v.as_int().ok())
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            if cmp > 0 {
                                items.swap(j - 1, j);
                                j -= 1;
                            } else {
                                break;
                            }
                        }
                        i += 1;
                    }
                    let mut st = jni_state().lock().unwrap();
                    if let Some(HeapObject::List(dst)) = st.heap.get_mut(list_id) {
                        *dst = items;
                    }
                }
            }
            return Ok(Value::Void);
        }
        if class_name == "Ljava/util/Arrays;" && method_name == "sort" {
            if let Some(arr_id) = args.first().and_then(Value::as_ref) {
                let comp = args.get(1).and_then(Value::as_ref);
                let mut items: Vec<Option<usize>> = {
                    let st = jni_state().lock().unwrap();
                    match st.heap.get(arr_id) {
                        Some(HeapObject::Array { data: ArrayData::Object(v), .. }) => v.clone(),
                        _ => Vec::new(),
                    }
                };
                if items.len() > 1 {
                    let comp_cls = comp.and_then(|cid| {
                        jni_state().lock().unwrap().heap.get(cid).map(|o| o.class_name().to_string())
                    });
                    let mut i = 1;
                    while i < items.len() {
                        let mut j = i;
                        while j > 0 {
                            let (a, b) = (Value::Ref(items[j - 1]), Value::Ref(items[j]));
                            let cmp = if let (Some(cid), Some(ccls)) = (comp, comp_cls.as_deref()) {
                                self.invoke_method(
                                    ccls,
                                    "compare",
                                    "(Ljava/lang/Object;Ljava/lang/Object;)I",
                                    &[Value::Ref(Some(cid)), a, b],
                                    true,
                                )
                                .ok()
                                .and_then(|v| v.as_int().ok())
                                .unwrap_or(0)
                            } else if let Some(aid) = items[j - 1] {
                                let acls = jni_state().lock().unwrap().heap.get(aid).map(|o| o.class_name().to_string());
                                if let Some(acls) = acls {
                                    self.invoke_method(&acls, "compareTo", "(Ljava/lang/Object;)I", &[a, b], true)
                                        .ok()
                                        .and_then(|v| v.as_int().ok())
                                        .unwrap_or(0)
                                } else {
                                    0
                                }
                            } else {
                                0
                            };
                            if cmp > 0 {
                                items.swap(j - 1, j);
                                j -= 1;
                            } else {
                                break;
                            }
                        }
                        i += 1;
                    }
                    let mut st = jni_state().lock().unwrap();
                    if let Some(HeapObject::Array { data: ArrayData::Object(dst), .. }) = st.heap.get_mut(arr_id) {
                        *dst = items;
                    }
                }
            }
            return Ok(Value::Void);
        }
        if class_name == "Ljava/io/File;" && method_name == "listFiles" {
            let dir_path = args.first().and_then(Value::as_ref).and_then(|id| {
                let st = jni_state().lock().unwrap();
                match st.heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => fields
                        .get("path")
                        .and_then(Value::as_ref)
                        .and_then(|sid| st.heap.as_str(sid))
                        .map(str::to_string),
                    _ => None,
                }
            });
            let Some(dir_path) = dir_path else {
                return Ok(Value::Ref(None));
            };
            let Ok(entries) = std::fs::read_dir(&dir_path) else {
                return Ok(Value::Ref(None));
            };
            let filter = args.get(1).and_then(Value::as_ref);
            let filter_cls = filter.and_then(|fid| {
                jni_state().lock().unwrap().heap.get(fid).map(|o| o.class_name().to_string())
            });
            let mut results: Vec<Option<usize>> = Vec::new();
            for entry in entries.flatten() {
                let entry_path = entry.path();
                let (file_id, name_id) = {
                    let mut st = jni_state().lock().unwrap();
                    let fid = st.heap.new_instance("Ljava/io/File;");
                    let pstr = st.heap.new_string(entry_path.to_string_lossy().into_owned());
                    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(fid) {
                        fields.insert("path".into(), Value::Ref(Some(pstr)));
                    }
                    let nid = st.heap.new_string(entry.file_name().to_string_lossy().into_owned());
                    (fid, nid)
                };
                let accepted = match (descriptor, filter, filter_cls.as_deref()) {
                    ("(Ljava/io/FileFilter;)[Ljava/io/File;", Some(fid), Some(fcls)) => self
                        .invoke_method(
                            fcls,
                            "accept",
                            "(Ljava/io/File;)Z",
                            &[Value::Ref(Some(fid)), Value::Ref(Some(file_id))],
                            true,
                        )
                        .ok()
                        .and_then(|v| v.as_int().ok())
                        .unwrap_or(0)
                        != 0,
                    ("(Ljava/io/FilenameFilter;)[Ljava/io/File;", Some(fid), Some(fcls)) => self
                        .invoke_method(
                            fcls,
                            "accept",
                            "(Ljava/io/File;Ljava/lang/String;)Z",
                            &[
                                Value::Ref(Some(fid)),
                                args.first().cloned().unwrap_or(Value::Ref(None)),
                                Value::Ref(Some(name_id)),
                            ],
                            true,
                        )
                        .ok()
                        .and_then(|v| v.as_int().ok())
                        .unwrap_or(0)
                        != 0,
                    _ => true,
                };
                if accepted {
                    results.push(Some(file_id));
                }
            }
            let arr_id = jni_state().lock().unwrap().heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/io/File;".into(),
                data: ArrayData::Object(results),
            });
            return Ok(Value::Ref(Some(arr_id)));
        }
        if self.trace
            || (std::env::var_os("PENGUDROID_TRACE_OKIO").is_some()
                && (class_name.contains("okhttp3")
                    || class_name.contains("okio")
                    || class_name.contains("Arrays")
                    || class_name.contains("Collections")))
        {
            eprintln!("[pengu-vm] invoke {class_name}.{method_name}{descriptor}");
        }
        let resolved_method = self.find_method_in_hierarchy(class_name, method_name, descriptor, is_virtual);
        let synchronization = resolved_method.clone().filter(|(_,_,m)| m.access_flags & 0x20 != 0);
        let _monitor = if let Some((_, declaring, method)) = synchronization {
            let object = if method.access_flags & 0x08 != 0 { jni_state().lock().unwrap().heap.class_object(&declaring) }
                else { args.first().and_then(Value::as_ref).ok_or("null synchronized receiver")? };
            Some(crate::monitors::Guard::new(object))
        } else { None };

        if class_name == "Ljava/lang/ClassLoader;" && method_name == "loadClass" {
            let name = args
                .get(1)
                .and_then(|v| v.as_ref())
                .and_then(|id| {
                    let st = jni_state().lock().unwrap();
                    st.heap.as_str(id).map(|s| s.to_string())
                })
                .unwrap_or_default();
            let desc = format!("L{};", name.replace('.', "/"));
            if self.find_class(&desc).is_some() {
                let mut st = jni_state().lock().unwrap();
                let cid = st.heap.class_object(&desc);
                return Ok(Value::Ref(Some(cid)));
            }
            let mut st = jni_state().lock().unwrap();
            let exc = st.heap.new_instance("Ljava/lang/ClassNotFoundException;");
            self.pending_exception = Some(exc);
            return Ok(Value::Ref(None));
        }
        if class_name == "Ljava/lang/Class;" && method_name == "newInstance" && descriptor == "()Ljava/lang/Object;" {
            let target_cls = args.first().and_then(|v| v.as_ref()).and_then(|cid| {
                let st = jni_state().lock().unwrap();
                match st.heap.get(cid) {
                    Some(HeapObject::ClassObj(c)) => Some(c.clone()),
                    _ => None,
                }
            });
            if let Some(tc) = target_cls {
                if self.find_class(&tc).is_some() {
                    let obj = self.new_instance(&tc)?;
                    let _ = self.invoke_method(&tc, "<init>", "()V", &[Value::Ref(Some(obj))], false);
                    return Ok(Value::Ref(Some(obj)));
                }
            }
            return Ok(Value::Ref(None));
        }
        if matches!(method_name,"getApplication" | "getApplicationContext") && (self.is_assignable_from("Landroid/content/Context;",class_name) || self.is_assignable_from("Landroid/view/View;",class_name)) {
            if let Some(value)=self.threads.statics.lock().unwrap().get("PENGUDROID_APPLICATION").cloned(){return Ok(value);}
        }
        match self.invoke_reflection(class_name,method_name,descriptor,args,is_virtual) {
            Ok(Some(value)) => return Ok(value), Ok(None) => {},
            Err(error) => return self.builtin_exception(error,descriptor),
        }
        match self.invoke_thread_builtin(class_name,method_name,descriptor,args,is_virtual) {
            Ok(Some(value)) => return Ok(value), Ok(None) => {},
            Err(error) => return self.builtin_exception(error,descriptor),
        }
        // Check registered JNI native methods first.
        if let Some(mut fn_ptr) = lookup_native(class_name, method_name, descriptor) {
            let key = (class_name.to_string(), method_name.to_string(), descriptor.to_string());
            if self.wrapped_natives.lock().unwrap().get(&key) != Some(&fn_ptr) {
                if self.needs_bridge && self.pointer_wrapper.is_none() {
                    fn_ptr = self.request_native_wrap(class_name, method_name, descriptor, fn_ptr)?;
                } else if let Some(wrapper) = self.pointer_wrapper.as_mut() {
                    let shorty = descriptor_to_shorty(descriptor);
                    if let Some(wrapped) = wrapper(fn_ptr, &shorty) {
                        crate::jni_host::register_native(class_name, method_name, descriptor, wrapped);
                        fn_ptr = wrapped;
                    }
                    self.wrapped_natives.lock().unwrap().insert(key, fn_ptr);
                }
            }
            let is_static = resolved_method.as_ref()
                .map(|(_, _, method)| method.access_flags & 0x0008 != 0)
                .unwrap_or(!is_virtual);
            return self.call_jni_native(fn_ptr, class_name, descriptor, args, is_static);
        }

        // Built-in Java / Android framework classes (walk builtin_superclass chain too).
        let builtin = {
            let mut st = jni_state().lock().unwrap();
            let mut cur = class_name.to_string();
            let mut res = try_invoke_builtin(
                &mut st.heap,
                &mut self.static_fields,
                &cur,
                method_name,
                descriptor,
                args,
            );
            while matches!(res, Ok(BuiltinResult::NotBuiltin)) {
                let Some(sup) = crate::stdlib::builtin_superclass(&cur) else {
                    break;
                };
                cur = sup.to_string();
                res = try_invoke_builtin(
                    &mut st.heap,
                    &mut self.static_fields,
                    &cur,
                    method_name,
                    descriptor,
                    args,
                );
            }
            res
        };
        let builtin = match builtin {
            Ok(b) => b,
            Err(error) => return self.builtin_exception(error, descriptor),
        };
        match builtin {
            BuiltinResult::Handled(v) => return Ok(v),
            BuiltinResult::LoadLibrary(lib) => {
                self.load_library_by_name(&lib)?;
                return Ok(Value::Void);
            }
            BuiltinResult::NotBuiltin => {}
        }

        // Resolve in loaded DEX hierarchy.
        if let Some((di, resolved_cls, method)) = resolved_method
        {
            self.ensure_clinit(&resolved_cls)?;
            if let Some(code) = method.code {
                return self.exec_code(di, &code, args);
            }
            // Native method declared in DEX without RegisterNatives yet -> check dynamic JNI symbol.
            if (method.access_flags & 0x0100) != 0 {
                let is_static = (method.access_flags & 0x0008) != 0;
                if let Some(fn_ptr) = lookup_native(&resolved_cls, method_name, descriptor) {
                    return self.call_jni_native(fn_ptr, &resolved_cls, descriptor, args, is_static);
                }
                let sym = jni_mangle_name(&resolved_cls, method_name);
                let shorty = descriptor_to_shorty(descriptor);
                let resolved_ptr = if let Some(resolver) = self.symbol_resolver.as_mut() {
                    resolver(&sym, &shorty)
                } else {
                    None
                };
                if let Some(fn_ptr) = resolved_ptr {
                    crate::jni_host::register_native(&resolved_cls, method_name, descriptor, fn_ptr);
                    self.wrapped_natives.lock().unwrap().insert((resolved_cls.clone(),method_name.into(),descriptor.into()),fn_ptr);
                    return self.call_jni_native(fn_ptr, &resolved_cls, descriptor, args, is_static);
                }
            }
            if method.access_flags & 0x0100 != 0 { return Err(format!("UnsatisfiedLinkError: {resolved_cls}.{method_name}{descriptor}")); }
            return Ok(Value::Void);
        }

        // Walk up superclasses into built-ins if DEX subclass inherits e.g. Activity/Object.
        let mut cur = self
            .find_class(class_name)
            .and_then(|(_, c)| c.superclass.clone());
        while let Some(sc) = cur {
            let b = {
                let mut st = jni_state().lock().unwrap();
                try_invoke_builtin(
                    &mut st.heap,
                    &mut self.static_fields,
                    &sc,
                    method_name,
                    descriptor,
                    args,
                )?
            };
            match b {
                BuiltinResult::Handled(v) => return Ok(v),
                BuiltinResult::LoadLibrary(lib) => {
                    self.load_library_by_name(&lib)?;
                    return Ok(Value::Void);
                }
                BuiltinResult::NotBuiltin => {
                    cur = self
                        .find_class(&sc)
                        .and_then(|(_, c)| c.superclass.clone())
                        .or_else(|| builtin_superclass(&sc).map(str::to_string));
                }
            }
        }

        if matches!((method_name,descriptor),("getClass","()Ljava/lang/Class;") | ("getClass","()Ljava/lang/Object;") | ("hashCode","()I") | ("equals","(Ljava/lang/Object;)Z") | ("toString","()Ljava/lang/String;")) {
            let mut state=jni_state().lock().unwrap();
            if let BuiltinResult::Handled(value)=try_invoke_builtin(&mut state.heap,&mut self.static_fields,"Ljava/lang/Object;",method_name,descriptor,args)? { return Ok(value); }
        }
        let signature = format!("{class_name}.{method_name}{descriptor}");
        *self.threads.missing_methods.lock().unwrap().entry(signature.clone()).or_insert(0) += 1;
        let count = self.missing_methods.entry(signature.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            eprintln!("[pengu-vm] UNIMPLEMENTED method {signature}");
        }
        if std::env::var_os("PENGUDROID_VM_STRICT").is_some() {
            return Err(format!("unimplemented method {signature}"));
        }
        Ok(default_return_for_descriptor(descriptor))
    }

    pub(crate) fn load_library_by_name(&mut self, short_or_path: &str) -> Result<(), String> {
        if !self.owns_runtime && self.native_loader.is_none() { return self.request_native_load(short_or_path); }
        let so_name = if let Some(base) = short_or_path.rsplit('/').next().filter(|s| s.ends_with(".so")) {
            base.to_string()
        } else if short_or_path.ends_with(".so") {
            short_or_path.to_string()
        } else {
            format!("lib{short_or_path}.so")
        };
        for dir in &self.native_dirs {
            let candidate = dir.join(&so_name);
            if candidate.is_file() {
                let p = candidate.to_string_lossy().into_owned();
                if self.trace {
                    eprintln!("[pengu-vm] loading native library {p}");
                }
                if let Some(loader) = self.native_loader.as_mut() {
                    return loader(&p);
                }
                return load_native_library(&p);
            }
        }
        if self.trace {
            eprintln!("[pengu-vm] native library {so_name} not in native_dirs, trying dlopen");
        }
        if let Some(loader) = self.native_loader.as_mut() {
            return loader(&so_name).map_err(|e| {
                if e.contains("requires a path containing") || e.contains("No such file") {
                    format!("UnsatisfiedLinkError: {e}")
                } else {
                    e
                }
            });
        }
        load_native_library(&so_name)
    }

    fn call_jni_native(
        &mut self, fn_ptr: usize, class_desc: &str, descriptor: &str,
        args: &[Value], is_static: bool,
    ) -> Result<Value, String> {
        let env = jni_env_ptr();
        let (receiver, params) = if is_static {
            let mut state = jni_state().lock().unwrap();
            (state.heap.class_object(class_desc) as *mut c_void, args)
        } else {
            let receiver = args.first().and_then(Value::as_ref).ok_or("null JNI native receiver")?;
            (receiver as *mut c_void, &args[1..])
        };
        let shorty = descriptor_to_shorty(descriptor);
        if !self.native_calls.contains_key(&shorty) {
            self.native_calls.insert(shorty.clone(), std::rc::Rc::new(crate::native_abi::NativeCall::new(&shorty)?));
        }
        // JNI can reenter the VM; keep the prepared interface alive without
        // retaining a map borrow across the foreign call.
        let interface = std::rc::Rc::clone(&self.native_calls[&shorty]);
        if self.trace { eprintln!("[pengu-vm] call_jni_native {class_desc}{descriptor} static={is_static} receiver={receiver:p}"); }
        let result = unsafe { interface.call(fn_ptr, env, receiver, params) };
        if let Some(exception) = crate::jni_host::take_pending_exception() {
            self.pending_exception = Some(exception);
        }
        result
    }

    pub fn new_instance(&mut self, class_name: &str) -> Result<ObjId, String> {
        self.ensure_clinit(class_name)?;
        let mut st = jni_state().lock().unwrap();
        Ok(st.heap.new_instance(class_name))
    }

    pub(crate) unsafe fn vm_jni_invoke_trampoline(
            ctx: usize,
            class_desc: &str,
            name: &str,
            sig: &str,
            args: &[Value],
        ) -> Value {
            let vm = &mut *(ctx as *mut Vm);
            let saved_exc = vm.pending_exception.take();
            let res = match vm.invoke_method(class_desc, name, sig, args, true) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("[pengu-jni] callback failed in {class_desc}.{name}{sig}: {error}");
                    let mut state = jni_state().lock().unwrap();
                    let id = state.heap.new_instance("Ljava/lang/RuntimeException;");
                    let message = state.heap.new_string(error);
                    if let Some(HeapObject::Instance{fields,..})=state.heap.get_mut(id){fields.insert("detailMessage".into(),Value::Ref(Some(message)));}
                    vm.pending_exception = Some(id);
                    Value::Void
                }
            };
            if let Some(exc_id) = vm.pending_exception.take() {
                crate::jni_host::set_pending_exception_object(exc_id);
                if vm.trace {
                    eprintln!("[pengu-jni] uncaught Dalvik exception in {class_desc}.{name}{sig} obj={exc_id}");
                }
            }
            vm.pending_exception = saved_exc;
            res
        }

    fn describe_exception(&self,id:ObjId)->String {
        let state=jni_state().lock().unwrap();let Some(object)=state.heap.get(id) else {return format!("invalid exception {id}")};
        let message=match object{HeapObject::Instance{fields,..}=>fields.get("detailMessage").and_then(Value::as_ref).and_then(|id|state.heap.as_str(id)).unwrap_or(""),_=>""};
        format!("{}: {message} (obj={id})",object.class_name())
    }
    pub fn launch_application(&mut self, application_class: &str) -> Result<ObjId, String> {
        let _invoker = crate::jni_host::scoped_vm_invoker(self as *mut Vm as usize, Self::vm_jni_invoke_trampoline);
        self.prepare_main_thread()?;
        crate::threads::publish_template(self.thread_template()?);
        let object = self.new_instance(application_class)?;
        let key = "PENGUDROID_APPLICATION".to_string();
        self.static_fields.insert(key.clone(), Value::Ref(Some(object)));
        self.threads.statics.lock().unwrap().insert(key.clone(), Value::Ref(Some(object)));
        crate::jni_host::set_static_field(&key, Value::Ref(Some(object)));
        crate::jni_host::set_static_field("PENGUDROID_ACTIVITY", Value::Ref(Some(object)));
        let context = self.new_instance("Landroid/content/Context;")?;
        for (name, signature, arguments) in [
            ("<init>", "()V", vec![Value::Ref(Some(object))]),
            ("attachBaseContext", "(Landroid/content/Context;)V", vec![Value::Ref(Some(object)), Value::Ref(Some(context))]),
            ("onCreate", "()V", vec![Value::Ref(Some(object))]),
        ] {
            self.invoke_method(application_class,name,signature,&arguments,name!="<init>")?;
            if let Some(exception)=self.pending_exception.take(){return Err(format!("Application.{name}: {}",self.describe_exception(exception)));}
        }
        Ok(object)
    }
    pub fn launch_activity(&mut self, activity_class: &str) -> Result<ObjId, String> {
        let _invoker = crate::jni_host::scoped_vm_invoker(self as *mut Vm as usize, Self::vm_jni_invoke_trampoline);
        self.prepare_main_thread()?;
        crate::threads::publish_template(self.thread_template()?);
        let obj = self.new_instance(activity_class)?;
        self.invoke_method(activity_class, "<init>", "()V", &[Value::Ref(Some(obj))], false)?;
        if let Some(exc_id) = self.pending_exception.take() {
            return Err(format!("uncaught Dalvik exception in <init>: {}",self.describe_exception(exc_id)));
        }
        let bundle = Value::Ref(None);
        self.invoke_method(
            activity_class,
            "onCreate",
            "(Landroid/os/Bundle;)V",
            &[Value::Ref(Some(obj)), bundle],
            true,
        )?;
        if let Some(exc_id) = self.pending_exception.take() {
            return Err(format!("uncaught Dalvik exception in onCreate: {}",self.describe_exception(exc_id)));
        }
        self.invoke_method(activity_class, "onStart", "()V", &[Value::Ref(Some(obj))], true)?;
        if let Some(exc_id) = self.pending_exception.take() {
            return Err(format!("uncaught Dalvik exception in onStart: {}",self.describe_exception(exc_id)));
        }
        self.invoke_method(activity_class, "onResume", "()V", &[Value::Ref(Some(obj))], true)?;
        if let Some(exc_id) = self.pending_exception.take() {
            return Err(format!("uncaught Dalvik exception in onResume: {}",self.describe_exception(exc_id)));
        }

        // Deliver SurfaceHolder callbacks (surfaceCreated / surfaceChanged) and window focus.
        let (holder_opt, callbacks) = {
            let st = jni_state().lock().unwrap();
            let holder_id = self
                .static_fields
                .get("PENGUDROID_ACTIVE_HOLDER")
                .and_then(|v| v.as_ref());
            let mut cbs = Vec::new();
            if let Some(hid) = holder_id {
                if let Some(HeapObject::Instance { fields, .. }) = st.heap.get(hid) {
                    if let Some(lid) = fields.get("callbacks").and_then(|v| v.as_ref()) {
                        if let Some(HeapObject::List(vec)) = st.heap.get(lid) {
                            for item in vec {
                                if let Some(cid) = item.as_ref() {
                                    if let Some(o) = st.heap.get(cid) {
                                        cbs.push((cid, o.class_name().to_string()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            (holder_id, cbs)
        };
        if let Some(hid) = holder_opt {
            let w = std::env::var("PENGUDROID_SURFACE_W")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(1280);
            let h = std::env::var("PENGUDROID_SURFACE_H")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(720);
            for (cid, cls) in callbacks {
                self.invoke_method(
                    &cls,
                    "surfaceCreated",
                    "(Landroid/view/SurfaceHolder;)V",
                    &[Value::Ref(Some(cid)), Value::Ref(Some(hid))],
                    true,
                )?;
                if let Some(exc) = self.pending_exception.take() { return Err(format!("surfaceCreated: {}", self.describe_exception(exc))); }
                self.invoke_method(
                    &cls,
                    "surfaceChanged",
                    "(Landroid/view/SurfaceHolder;III)V",
                    &[
                        Value::Ref(Some(cid)),
                        Value::Ref(Some(hid)),
                        Value::Int(1),
                        Value::Int(w),
                        Value::Int(h),
                    ],
                    true,
                )?;
                if let Some(exc) = self.pending_exception.take() { return Err(format!("surfaceChanged: {}", self.describe_exception(exc))); }
            }
        }
        let _ = self.invoke_method(
            activity_class,
            "onWindowFocusChanged",
            "(Z)V",
            &[Value::Ref(Some(obj)), Value::Int(1)],
            true,
        );

        Ok(obj)
    }

    fn exec_code(&mut self, dex_idx: usize, code: &CodeItem, args: &[Value]) -> Result<Value, String> {
        let n_regs = code.registers_size as usize;
        let mut regs = vec![Value::Int(0); n_regs.max(16)];
        let ins_start = n_regs.saturating_sub(code.ins_size as usize);

        // Place incoming arguments into the last `ins_size` registers (wide types occupy 2 slots).
        let mut slot = ins_start;
        for arg in args {
            if slot < regs.len() {
                regs[slot] = arg.clone();
            }
            slot += match arg {
                Value::Long(_) | Value::Double(_) => 2,
                _ => 1,
            };
        }

        let mut last_result = Value::Void;
        let mut last_exception: Option<ObjId> = None;
        let mut pc: usize = 0;
        let insns = &code.insns;
        let mut steps: usize = 0;

        while pc < insns.len() {
            steps += 1;
            if self.owns_runtime && steps > 200_000_000 {
                return Err(format!(
                    "VM instruction budget exceeded at pc={pc} op=0x{:02x}",
                    insns[pc] & 0xff
                ));
            }
            let op_pc = pc;
            let unit = insns[pc];
            let op = (unit & 0xff) as u8;
            let aa = ((unit >> 8) & 0xff) as usize;

            let step_res: Result<Option<Value>, OpErr> = (|| -> Result<Option<Value>, OpErr> {
                match op {
                    0x00 => pc += 1, // nop
                    0x01 => {
                        // move vA, vB
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        regs[a] = regs[b].clone();
                        pc += 1;
                    }
                    0x02 => {
                        // move/from16 vAA, vBBBB
                        let b = insns[pc + 1] as usize;
                        regs[aa] = regs[b].clone();
                        pc += 2;
                    }
                    0x03 | 0x06 | 0x09 => {
                        // move/16, move-wide/16, move-object/16 vAAAA, vBBBB
                        let a = insns[pc + 1] as usize;
                        let b = insns[pc + 2] as usize;
                        regs[a] = regs[b].clone();
                        if op == 0x06 && a + 1 < regs.len() && b + 1 < regs.len() {
                            regs[a + 1] = regs[b + 1].clone();
                        }
                        pc += 3;
                    }
                    0x04 => {
                        // move-wide vA, vB
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        regs[a] = regs[b].clone();
                        if a + 1 < regs.len() && b + 1 < regs.len() {
                            regs[a + 1] = regs[b + 1].clone();
                        }
                        pc += 1;
                    }
                    0x05 => {
                        // move-wide/from16 vAA, vBBBB
                        let b = insns[pc + 1] as usize;
                        regs[aa] = regs[b].clone();
                        if aa + 1 < regs.len() && b + 1 < regs.len() {
                            regs[aa + 1] = regs[b + 1].clone();
                        }
                        pc += 2;
                    }
                    0x07 => {
                        // move-object vA, vB
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        regs[a] = regs[b].clone();
                        pc += 1;
                    }
                    0x08 => {
                        // move-object/from16 vAA, vBBBB
                        let b = insns[pc + 1] as usize;
                        regs[aa] = regs[b].clone();
                        pc += 2;
                    }
                    0x0a..=0x0c => {
                        // move-result, move-result-wide, move-result-object
                        regs[aa] = last_result.clone();
                        pc += 1;
                    }
                    0x0d => {
                        // move-exception vAA
                        regs[aa] = Value::Ref(last_exception.take());
                        pc += 1;
                    }
                    0x0e => return Ok(Some(Value::Void)), // return-void
                    0x0f | 0x10 | 0x11 => return Ok(Some(regs[aa].clone())), // return*
                    0x12 => {
                        // const/4 vA, #+B
                        let a = aa & 0x0f;
                        let b = ((aa as i8) >> 4) as i32;
                        regs[a] = Value::Int(b);
                        pc += 1;
                    }
                    0x13 => {
                        // const/16 vAA, #+BBBB
                        let b = insns[pc + 1] as i16 as i32;
                        regs[aa] = Value::Int(b);
                        pc += 2;
                    }
                    0x14 => {
                        // const vAA, #+BBBBBBBB
                        let lo = insns[pc + 1] as u32;
                        let hi = insns[pc + 2] as u32;
                        regs[aa] = Value::Int(((hi << 16) | lo) as i32);
                        pc += 3;
                    }
                    0x15 => {
                        // const/high16 vAA, #+BBBB0000
                        let hi = insns[pc + 1] as u32;
                        regs[aa] = Value::Int((hi << 16) as i32);
                        pc += 2;
                    }
                    0x16 => {
                        // const-wide/16 vAA, #+BBBB
                        let b = insns[pc + 1] as i16 as i64;
                        regs[aa] = Value::Long(b);
                        pc += 2;
                    }
                    0x17 => {
                        // const-wide/32 vAA, #+BBBBBBBB
                        let lo = insns[pc + 1] as u32;
                        let hi = insns[pc + 2] as u32;
                        regs[aa] = Value::Long(((hi << 16) | lo) as i32 as i64);
                        pc += 3;
                    }
                    0x18 => {
                        // const-wide vAA, #+64
                        let v = (insns[pc + 1] as u64)
                            | ((insns[pc + 2] as u64) << 16)
                            | ((insns[pc + 3] as u64) << 32)
                            | ((insns[pc + 4] as u64) << 48);
                        regs[aa] = Value::Long(v as i64);
                        pc += 5;
                    }
                    0x19 => {
                        // const-wide/high16 vAA, #+BBBB000000000000
                        let v = (insns[pc + 1] as u64) << 48;
                        regs[aa] = Value::Long(v as i64);
                        pc += 2;
                    }
                    0x1a => {
                        // const-string vAA, string@BBBB
                        let idx = insns[pc + 1] as usize;
                        let s = self.dexes[dex_idx].strings[idx].clone();
                        let mut st = jni_state().lock().unwrap();
                        let id = st.heap.intern_string(&s);
                        regs[aa] = Value::Ref(Some(id));
                        pc += 2;
                    }
                    0x1b => {
                        // const-string/jumbo vAA, string@BBBBBBBB
                        let idx = ((insns[pc + 1] as u32) | ((insns[pc + 2] as u32) << 16)) as usize;
                        let s = self.dexes[dex_idx].strings[idx].clone();
                        let mut st = jni_state().lock().unwrap();
                        let id = st.heap.intern_string(&s);
                        regs[aa] = Value::Ref(Some(id));
                        pc += 3;
                    }
                    0x1c => {
                        // const-class vAA, type@BBBB
                        let idx = insns[pc + 1] as usize;
                        let t = self.dexes[dex_idx].types[idx].clone();
                        let mut st = jni_state().lock().unwrap();
                        let id = st.heap.class_object(&t);
                        regs[aa] = Value::Ref(Some(id));
                        pc += 2;
                    }
                    0x1d | 0x1e => {
                        let id = regs[aa].as_ref().ok_or_else(|| OpErr::Fatal("null monitor receiver".into()))?;
                        if op == 0x1d { crate::monitors::enter(id); }
                        else { crate::monitors::exit(id).map_err(OpErr::Fatal)?; }
                        pc += 1;
                    }
                    0x1f => {
                        let target=&self.dexes[dex_idx].types[insns[pc+1] as usize];
                        if let Some(id)=regs[aa].as_ref(){if !self.is_instance_of(id,target){let mut st=jni_state().lock().unwrap();let actual=st.heap.class_name(id).unwrap_or("?").to_string();let exception=st.heap.new_instance("Ljava/lang/ClassCastException;");let msg=st.heap.new_string(format!("{actual} cannot be cast to {target}"));if let Some(HeapObject::Instance{fields,..})=st.heap.get_mut(exception){fields.insert("detailMessage".into(),Value::Ref(Some(msg)));}return Err(OpErr::Thrown(exception));}}
                        pc+=2;
                    }
                    0x20 => {
                        let a=aa&0x0f;let b=(aa>>4)&0x0f;let target=&self.dexes[dex_idx].types[insns[pc+1] as usize];
                        regs[a]=Value::Int(regs[b].as_ref().is_some_and(|id|self.is_instance_of(id,target)) as i32);pc+=2;
                    }
                    0x21 => {
                        // array-length vA, vB
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        let len = if let Some(id) = regs[b].as_ref() {
                            let st = jni_state().lock().unwrap();
                            match st.heap.get(id) {
                                Some(HeapObject::Array { data, .. }) => data.len() as i32,
                                _ => 0,
                            }
                        } else {
                            0
                        };
                        regs[a] = Value::Int(len);
                        pc += 1;
                    }
                    0x22 => {
                        // new-instance vAA, type@BBBB
                        let idx = insns[pc + 1] as usize;
                        let cname = self.dexes[dex_idx].types[idx].clone();
                        let id = self.new_instance(&cname).map_err(OpErr::Fatal)?;
                        regs[aa] = Value::Ref(Some(id));
                        pc += 2;
                    }
                    0x23 => {
                        // new-array vA, vB, type@CCCC
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        let len = regs[b].as_int().unwrap_or(0).max(0) as usize;
                        let idx = insns[pc + 1] as usize;
                        let tname = self.dexes[dex_idx].types[idx].clone();
                        let mut st = jni_state().lock().unwrap();
                        let id = st.heap.new_array(&tname, len).map_err(OpErr::Fatal)?;
                        regs[a] = Value::Ref(Some(id));
                        pc += 2;
                    }
                    0x24 => {
                        // filled-new-array {vC, vD, vE, vF, vG}, type@BBBB
                        let count = (aa >> 4) & 0x0f;
                        let g = aa & 0x0f;
                        let idx = insns[pc + 1] as usize;
                        let w2 = insns[pc + 2] as usize;
                        let arg_regs = [
                            w2 & 0x0f,
                            (w2 >> 4) & 0x0f,
                            (w2 >> 8) & 0x0f,
                            (w2 >> 12) & 0x0f,
                            g,
                        ];
                        let tname = self.dexes[dex_idx].types[idx].clone();
                        let mut st = jni_state().lock().unwrap();
                        let id = st.heap.new_array(&tname, count).map_err(OpErr::Fatal)?;
                        let vals: Vec<Value> = (0..count).map(|i| regs[arg_regs[i]].clone()).collect();
                        if let Some(HeapObject::Array { data, .. }) = st.heap.get_mut(id) {
                            match data {
                                ArrayData::Int(v) => {
                                    for (i, val) in vals.iter().enumerate() {
                                        v[i] = val.as_int().unwrap_or(0);
                                    }
                                }
                                ArrayData::Object(v) => {
                                    for (i, val) in vals.iter().enumerate() {
                                        v[i] = val.as_ref();
                                    }
                                }
                                _ => {}
                            }
                        }
                        last_result = Value::Ref(Some(id));
                        pc += 3;
                    }
                    0x25 => {
                        // filled-new-array/range {vCCCC .. vNNNN}, type@BBBB
                        let count = aa;
                        let idx = insns[pc + 1] as usize;
                        let start = insns[pc + 2] as usize;
                        let tname = self.dexes[dex_idx].types[idx].clone();
                        let mut st = jni_state().lock().unwrap();
                        let id = st.heap.new_array(&tname, count).map_err(OpErr::Fatal)?;
                        let vals: Vec<Value> = (0..count).map(|i| regs[start + i].clone()).collect();
                        if let Some(HeapObject::Array { data, .. }) = st.heap.get_mut(id) {
                            match data {
                                ArrayData::Int(v) => {
                                    for (i, val) in vals.iter().enumerate() {
                                        v[i] = val.as_int().unwrap_or(0);
                                    }
                                }
                                ArrayData::Object(v) => {
                                    for (i, val) in vals.iter().enumerate() {
                                        v[i] = val.as_ref();
                                    }
                                }
                                _ => {}
                            }
                        }
                        last_result = Value::Ref(Some(id));
                        pc += 3;
                    }
                    0x26 => {
                        // fill-array-data vAA, +BBBBBBBB
                        let off = ((insns[pc + 1] as u32) | ((insns[pc + 2] as u32) << 16)) as i32;
                        let data_pc = (pc as isize + off as isize) as usize;
                        let Some(arr_id) = regs[aa].as_ref() else {
                            let id = jni_state().lock().unwrap().heap.new_instance("Ljava/lang/NullPointerException;");
                            return Err(OpErr::Thrown(id));
                        };
                        if data_pc + 4 <= insns.len() && insns[data_pc] == 0x0300 {
                            let width = insns[data_pc + 1] as usize;
                            let size = ((insns[data_pc + 2] as u32) | ((insns[data_pc + 3] as u32) << 16)) as usize;
                            let payload = &insns[data_pc + 4..];
                            let mut st = jni_state().lock().unwrap();
                            if let Some(HeapObject::Array { data, .. }) = st.heap.get_mut(arr_id) {
                                match (data, width) {
                                    (ArrayData::Byte(dst), 1) => {
                                        for i in 0..size.min(dst.len()) {
                                            let w = payload[i / 2];
                                            let b = if i % 2 == 0 { w & 0xff } else { w >> 8 };
                                            dst[i] = b as i8;
                                        }
                                    }
                                    (ArrayData::Boolean(dst), 1) => {
                                        for i in 0..size.min(dst.len()) {
                                            let w = payload[i / 2];
                                            let b = if i % 2 == 0 { w & 0xff } else { w >> 8 };
                                            dst[i] = b != 0;
                                        }
                                    }
                                    (ArrayData::Short(dst), 2) => {
                                        for i in 0..size.min(dst.len()) {
                                            dst[i] = payload[i] as i16;
                                        }
                                    }
                                    (ArrayData::Char(dst), 2) => {
                                        for i in 0..size.min(dst.len()) {
                                            dst[i] = payload[i];
                                        }
                                    }
                                    (ArrayData::Int(dst), 4) => {
                                        for i in 0..size.min(dst.len()) {
                                            dst[i] = ((payload[i * 2] as u32) | ((payload[i * 2 + 1] as u32) << 16)) as i32;
                                        }
                                    }
                                    (ArrayData::Float(dst), 4) => {
                                        for i in 0..size.min(dst.len()) {
                                            let bits = (payload[i * 2] as u32) | ((payload[i * 2 + 1] as u32) << 16);
                                            dst[i] = f32::from_bits(bits);
                                        }
                                    }
                                    (ArrayData::Long(dst), 8) => {
                                        for i in 0..size.min(dst.len()) {
                                            let lo = (payload[i * 4] as u64) | ((payload[i * 4 + 1] as u64) << 16);
                                            let hi = (payload[i * 4 + 2] as u64) | ((payload[i * 4 + 3] as u64) << 16);
                                            dst[i] = (lo | (hi << 32)) as i64;
                                        }
                                    }
                                    (ArrayData::Double(dst), 8) => {
                                        for i in 0..size.min(dst.len()) {
                                            let lo = (payload[i * 4] as u64) | ((payload[i * 4 + 1] as u64) << 16);
                                            let hi = (payload[i * 4 + 2] as u64) | ((payload[i * 4 + 3] as u64) << 16);
                                            dst[i] = f64::from_bits(lo | (hi << 32));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        pc += 3;
                    }
                    0x2b => {
                        // packed-switch vAA, +BBBBBBBB
                        let off = ((insns[pc + 1] as u32) | ((insns[pc + 2] as u32) << 16)) as i32;
                        let table_pc = (pc as isize + off as isize) as usize;
                        let val = regs[aa].as_int().unwrap_or(0);
                        let mut jumped = false;
                        if table_pc + 3 < insns.len() && insns[table_pc] == 0x0100 {
                            let size = insns[table_pc + 1] as usize;
                            let first_key = ((insns[table_pc + 2] as u32)
                                | ((insns[table_pc + 3] as u32) << 16))
                                as i32;
                            let diff = val.wrapping_sub(first_key);
                            if diff >= 0 && (diff as usize) < size {
                                let base = table_pc + 4 + (diff as usize) * 2;
                                if base + 1 < insns.len() {
                                    let target_off = ((insns[base] as u32)
                                        | ((insns[base + 1] as u32) << 16))
                                        as i32;
                                    pc = (pc as isize + target_off as isize) as usize;
                                    jumped = true;
                                }
                            }
                        }
                        if !jumped {
                            pc += 3;
                        }
                    }
                    0x2c => {
                        // sparse-switch vAA, +BBBBBBBB
                        let off = ((insns[pc + 1] as u32) | ((insns[pc + 2] as u32) << 16)) as i32;
                        let table_pc = (pc as isize + off as isize) as usize;
                        let val = regs[aa].as_int().unwrap_or(0);
                        let mut jumped = false;
                        if table_pc + 1 < insns.len() && insns[table_pc] == 0x0200 {
                            let size = insns[table_pc + 1] as usize;
                            for i in 0..size {
                                let k_base = table_pc + 2 + i * 2;
                                let t_base = table_pc + 2 + size * 2 + i * 2;
                                if t_base + 1 < insns.len() {
                                    let key = ((insns[k_base] as u32)
                                        | ((insns[k_base + 1] as u32) << 16))
                                        as i32;
                                    if key == val {
                                        let target_off = ((insns[t_base] as u32)
                                            | ((insns[t_base + 1] as u32) << 16))
                                            as i32;
                                        pc = (pc as isize + target_off as isize) as usize;
                                        jumped = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if !jumped {
                            pc += 3;
                        }
                    }
                    0x27 => {
                        // throw vAA
                        let exc_id = regs[aa].as_ref().unwrap_or(0);
                        return Err(OpErr::Thrown(exc_id));
                    }
                    0x28 => {
                        // goto +AA
                        let off = (aa as i8) as isize;
                        pc = (pc as isize + off) as usize;
                    }
                    0x29 => {
                        // goto/16 +AAAA
                        let off = (insns[pc + 1] as i16) as isize;
                        pc = (pc as isize + off) as usize;
                    }
                    0x2a => {
                        // goto/32 +AAAAAAAA
                        let off = ((insns[pc + 1] as u32) | ((insns[pc + 2] as u32) << 16)) as i32 as isize;
                        pc = (pc as isize + off) as usize;
                    }
                    0x2d | 0x2e => {
                        // cmpl-float / cmpg-float vAA, vBB, vCC
                        let w1 = insns[pc + 1];
                        let b = (w1 & 0xff) as usize;
                        let c = ((w1 >> 8) & 0xff) as usize;
                        let fb = regs[b].as_float().unwrap_or(0.0);
                        let fc = regs[c].as_float().unwrap_or(0.0);
                        let res = if fb < fc {
                            -1
                        } else if fb > fc {
                            1
                        } else if fb == fc {
                            0
                        } else if op == 0x2d {
                            -1
                        } else {
                            1
                        };
                        regs[aa] = Value::Int(res);
                        pc += 2;
                    }
                    0x2f | 0x30 => {
                        // cmpl-double / cmpg-double vAA, vBB, vCC
                        let w1 = insns[pc + 1];
                        let b = (w1 & 0xff) as usize;
                        let c = ((w1 >> 8) & 0xff) as usize;
                        let db = regs[b].as_double().unwrap_or(0.0);
                        let dc = regs[c].as_double().unwrap_or(0.0);
                        let res = if db < dc {
                            -1
                        } else if db > dc {
                            1
                        } else if db == dc {
                            0
                        } else if op == 0x2f {
                            -1
                        } else {
                            1
                        };
                        regs[aa] = Value::Int(res);
                        pc += 2;
                    }
                    0x31 => {
                        // cmp-long vAA, vBB, vCC
                        let b = (insns[pc + 1] & 0xff) as usize;
                        let c = (insns[pc + 1] >> 8) as usize;
                        let vb = regs[b].as_long().unwrap_or(0);
                        let vc = regs[c].as_long().unwrap_or(0);
                        regs[aa] = Value::Int(vb.cmp(&vc) as i32);
                        pc += 2;
                    }
                    0x32..=0x37 => {
                        // if-eq, if-ne, if-lt, if-ge, if-gt, if-le vA, vB, +CCCC
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        let off = (insns[pc + 1] as i16) as isize;
                        let cond = cmp_vals(&regs[a], &regs[b], op - 0x32);
                        if cond {
                            pc = (pc as isize + off) as usize;
                        } else {
                            pc += 2;
                        }
                    }
                    0x38..=0x3d => {
                        // if-eqz..if-lez vAA, +BBBB
                        let off = (insns[pc + 1] as i16) as isize;
                        let v = match &regs[aa] {
                            Value::Int(i) => *i,
                            Value::Ref(None) => 0,
                            Value::Ref(Some(_)) => 1,
                            _ => 0,
                        };
                        let cond = match op {
                            0x38 => v == 0,
                            0x39 => v != 0,
                            0x3a => v < 0,
                            0x3b => v >= 0,
                            0x3c => v > 0,
                            0x3d => v <= 0,
                            _ => false,
                        };
                        if cond {
                            pc = (pc as isize + off) as usize;
                        } else {
                            pc += 2;
                        }
                    }
                    0x44..=0x4a => {
                        // aget* vAA, vBB, vCC
                        let b = (insns[pc + 1] & 0xff) as usize;
                        let c = (insns[pc + 1] >> 8) as usize;
                        let idx = regs[c].as_int().unwrap_or(0).max(0) as usize;
                        if let Some(arr_id) = regs[b].as_ref() {
                            let st = jni_state().lock().unwrap();
                            if let Some(HeapObject::Array { data, .. }) = st.heap.get(arr_id) {
                                regs[aa] = match data {
                                    ArrayData::Int(v) => Value::Int(*v.get(idx).unwrap_or(&0)),
                                    ArrayData::Long(v) => Value::Long(*v.get(idx).unwrap_or(&0)),
                                    ArrayData::Byte(v) => Value::Int(*v.get(idx).unwrap_or(&0) as i32),
                                    ArrayData::Boolean(v) => Value::Int(i32::from(*v.get(idx).unwrap_or(&false))),
                                    ArrayData::Short(v) => Value::Int(*v.get(idx).unwrap_or(&0) as i32),
                                    ArrayData::Char(v) => Value::Int(*v.get(idx).unwrap_or(&0) as i32),
                                    ArrayData::Float(v) => Value::Float(*v.get(idx).unwrap_or(&0.0)),
                                    ArrayData::Double(v) => Value::Double(*v.get(idx).unwrap_or(&0.0)),
                                    ArrayData::Object(v) => Value::Ref(*v.get(idx).unwrap_or(&None)),
                                };
                            }
                        }
                        pc += 2;
                    }
                    0x4b..=0x51 => {
                        // aput* vAA, vBB, vCC
                        let b = (insns[pc + 1] & 0xff) as usize;
                        let c = (insns[pc + 1] >> 8) as usize;
                        let idx = regs[c].as_int().unwrap_or(0).max(0) as usize;
                        if let Some(arr_id) = regs[b].as_ref() {
                            let mut st = jni_state().lock().unwrap();
                            if let Some(HeapObject::Array { data, .. }) = st.heap.get_mut(arr_id) {
                                match data {
                                    ArrayData::Int(v) if idx < v.len() => v[idx] = regs[aa].as_int().unwrap_or(0),
                                    ArrayData::Long(v) if idx < v.len() => v[idx] = regs[aa].as_long().unwrap_or(0),
                                    ArrayData::Byte(v) if idx < v.len() => v[idx] = regs[aa].as_int().unwrap_or(0) as i8,
                                    ArrayData::Boolean(v) if idx < v.len() => v[idx] = regs[aa].as_int().unwrap_or(0) != 0,
                                    ArrayData::Object(v) if idx < v.len() => v[idx] = regs[aa].as_ref(),
                                    _ => {}
                                }
                            }
                        }
                        pc += 2;
                    }
                    0x52..=0x58 => {
                        // iget* vA, vB, field@CCCC
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        let fidx = insns[pc + 1] as usize;
                        let fname = self.dexes[dex_idx].fields[fidx].name.clone();
                        let val = if let Some(obj_id) = regs[b].as_ref() {
                            let st = jni_state().lock().unwrap();
                            match st.heap.get(obj_id) {
                                Some(HeapObject::Instance { fields, .. }) => {
                                    fields.get(&fname).cloned().unwrap_or(Value::Int(0))
                                }
                                _ => Value::Int(0),
                            }
                        } else {
                            Value::Int(0)
                        };
                        regs[a] = val;
                        pc += 2;
                    }
                    0x59..=0x5f => {
                        // iput* vA, vB, field@CCCC
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        let fidx = insns[pc + 1] as usize;
                        let fname = self.dexes[dex_idx].fields[fidx].name.clone();
                        if let Some(obj_id) = regs[b].as_ref() {
                            let mut st = jni_state().lock().unwrap();
                            if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(obj_id) {
                                fields.insert(fname, regs[a].clone());
                            }
                        }
                        pc += 2;
                    }
                    0x60..=0x66 => {
                        // sget* vAA, field@BBBB
                        let fidx = insns[pc + 1] as usize;
                        let f = self.dexes[dex_idx].fields[fidx].clone();
                        self.ensure_clinit(&f.class_name).map_err(OpErr::Fatal)?;
                        let key = format!("{}.{}", f.class_name, f.name);
                        regs[aa] = self.threads.statics.lock().unwrap().get(&key).cloned()
                            .or_else(|| self.static_fields.get(&key).cloned())
                            .unwrap_or_else(|| {
                                if f.type_name.starts_with('L') || f.type_name.starts_with('[') {
                                    Value::Ref(None)
                                } else if f.type_name == "J" {
                                    Value::Long(0)
                                } else {
                                    Value::Int(0)
                                }
                            });
                        pc += 2;
                    }
                    0x67..=0x6d => {
                        // sput* vAA, field@BBBB
                        let fidx = insns[pc + 1] as usize;
                        let f = self.dexes[dex_idx].fields[fidx].clone();
                        self.ensure_clinit(&f.class_name).map_err(OpErr::Fatal)?;
                        let key = format!("{}.{}", f.class_name, f.name);
                        self.threads.statics.lock().unwrap().insert(key.clone(), regs[aa].clone());
                        crate::jni_host::set_static_field(&key, regs[aa].clone());
                        self.static_fields.insert(key, regs[aa].clone());
                        pc += 2;
                    }
                    0x6e..=0x72 => {
                        // invoke-{virtual,super,direct,static,interface} {vC..vG}, meth@BBBB
                        let count = (aa >> 4) & 0x0f;
                        let g = aa & 0x0f;
                        let midx = insns[pc + 1] as usize;
                        let cd = insns[pc + 2];
                        let mut arg_regs = Vec::with_capacity(count);
                        let reg_ids = [
                            (cd & 0x0f) as usize,
                            ((cd >> 4) & 0x0f) as usize,
                            ((cd >> 8) & 0x0f) as usize,
                            ((cd >> 12) & 0x0f) as usize,
                            g,
                        ];
                        for r in reg_ids.into_iter().take(count) {
                            arg_regs.push(regs[r].clone());
                        }
                        let m = self.dexes[dex_idx].methods[midx].clone();
                        let is_virtual = op == 0x6e || op == 0x72;
                        let target_class = if is_virtual {
                            arg_regs
                                .first()
                                .and_then(|v| v.as_ref())
                                .and_then(|id| {
                                    jni_state()
                                        .lock()
                                        .unwrap()
                                        .heap
                                        .class_name(id).map(str::to_owned)
                                })
                                .unwrap_or_else(|| m.class_name.clone())
                        } else {
                            m.class_name.clone()
                        };
                        let compact = compact_wide_args(&arg_regs, &self.dexes[dex_idx].protos[m.proto_idx].params, op != 0x71);
                        last_result = self
                            .invoke_method(&target_class, &m.name, &m.descriptor, &compact, is_virtual)
                            .map_err(OpErr::Fatal)?;
                        if let Some(exc_id) = self.pending_exception.take() {
                            return Err(OpErr::Thrown(exc_id));
                        }
                        pc += 3;
                    }
                    0x74..=0x78 => {
                        // invoke-*/range {vCCCC..vNNNN}, meth@BBBB
                        let midx = insns[pc + 1] as usize;
                        let start = insns[pc + 2] as usize;
                        let arg_regs: Vec<Value> = regs[start..start + aa].to_vec();
                        let m = self.dexes[dex_idx].methods[midx].clone();
                        let is_virtual = op == 0x74 || op == 0x78;
                        let target_class = if is_virtual {
                            arg_regs
                                .first()
                                .and_then(|v| v.as_ref())
                                .and_then(|id| {
                                    jni_state()
                                        .lock()
                                        .unwrap()
                                        .heap
                                        .class_name(id).map(str::to_owned)
                                })
                                .unwrap_or_else(|| m.class_name.clone())
                        } else {
                            m.class_name.clone()
                        };
                        let compact = compact_wide_args(&arg_regs, &self.dexes[dex_idx].protos[m.proto_idx].params, op != 0x77);
                        last_result = self
                            .invoke_method(&target_class, &m.name, &m.descriptor, &compact, is_virtual)
                            .map_err(OpErr::Fatal)?;
                        if let Some(exc_id) = self.pending_exception.take() {
                            return Err(OpErr::Thrown(exc_id));
                        }
                        pc += 3;
                    }
                    0x7b..=0x8f => {
                        // Unary ops
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        regs[a] = eval_unary(op, &regs[b]);
                        pc += 1;
                    }
                    0x90..=0xaf => {
                        // Binary 3-reg ops
                        let b = (insns[pc + 1] & 0xff) as usize;
                        let c = (insns[pc + 1] >> 8) as usize;
                        regs[aa] = eval_binary(op, &regs[b], &regs[c]);
                        pc += 2;
                    }
                    0xb0..=0xcf => {
                        // Binary /2addr ops
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        regs[a] = eval_binary(op - 0x20, &regs[a], &regs[b]);
                        pc += 1;
                    }
                    0xd0..=0xd7 => {
                        // Binary /lit16
                        let a = aa & 0x0f;
                        let b = (aa >> 4) & 0x0f;
                        let lit = insns[pc + 1] as i16 as i32;
                        regs[a] = eval_int_lit(op - 0xd0, regs[b].as_int().unwrap_or(0), lit);
                        pc += 2;
                    }
                    0xd8..=0xe2 => {
                        // Binary /lit8
                        let b = (insns[pc + 1] & 0xff) as usize;
                        let lit = ((insns[pc + 1] >> 8) as u8 as i8) as i32;
                        regs[aa] = eval_int_lit(op - 0xd8, regs[b].as_int().unwrap_or(0), lit);
                        pc += 2;
                    }
                    other => {
                        return Err(OpErr::Fatal(format!(
                            "unimplemented Dalvik opcode 0x{other:02x} at pc={op_pc}"
                        )));
                    }
                }
                Ok(None)
            })();

            match step_res {
                Ok(Some(ret)) => return Ok(ret),
                Ok(None) => {}
                Err(OpErr::Fatal(e)) => return Err(e),
                Err(OpErr::Thrown(exc_id)) => {
                    if let Some(handler_pc) = self.find_catch_handler(code, op_pc as u32, exc_id) {
                        last_exception = Some(exc_id);
                        pc = handler_pc as usize;
                    } else {
                        self.pending_exception = Some(exc_id);
                        return Ok(Value::Void);
                    }
                }
            }
        }
        Ok(Value::Void)
    }

    fn is_subclass_of(&self, sub: &str, target: &str) -> bool {
        let mut cur = sub.to_string();
        for _ in 0..16 {
            if cur == target {
                return true;
            }
            if let Some(sup) = self.find_class(&cur).and_then(|(_, c)| c.superclass.clone()) {
                cur = sup;
            } else if let Some(sup) = builtin_superclass(&cur) {
                cur = sup.to_string();
            } else {
                break;
            }
        }
        target == "Ljava/lang/Object;" || target == "Ljava/lang/Throwable;"
    }

    fn find_catch_handler(&self, code: &CodeItem, pc: u32, exc_id: ObjId) -> Option<u32> {
        let exc_class = {
            let st = jni_state().lock().unwrap();
            st.heap
                .class_name(exc_id).map(str::to_owned)
                .unwrap_or_default()
        };
        for t in &code.tries {
            if pc >= t.start_addr && pc < t.start_addr + t.insn_count as u32 {
                for h in &t.handlers {
                    if h.type_name == exc_class
                        || h.type_name == "Ljava/lang/Throwable;"
                        || self.is_subclass_of(&exc_class, &h.type_name)
                    {
                        return Some(h.addr);
                    }
                }
                if let Some(addr) = t.catch_all_addr {
                    return Some(addr);
                }
            }
        }
        None
    }
}

enum OpErr {
    Thrown(ObjId),
    Fatal(String),
}

fn compact_wide_args(raw: &[Value], params: &[String], has_this: bool) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    if has_this && i < raw.len() {
        out.push(raw[i].clone());
        i += 1;
    }
    for p in params {
        if i >= raw.len() {
            break;
        }
        out.push(raw[i].clone());
        i += if p == "J" || p == "D" { 2 } else { 1 };
    }
    out
}

pub(crate) fn jni_mangle_name(class_desc: &str, method_name: &str) -> String {
    let cls = class_desc
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(class_desc)
        .replace('_', "_1")
        .replace('/', "_")
        .replace('$', "_00024");
    let meth = method_name.replace('_', "_1");
    format!("Java_{cls}_{meth}")
}

pub(crate) fn descriptor_to_shorty(sig: &str) -> String {
    let mut params = String::new();
    let mut ret = String::new();
    let mut chars = sig.chars().peekable();
    let mut in_params = false;
    while let Some(c) = chars.next() {
        if c == '(' {
            in_params = true;
            continue;
        }
        if c == ')' {
            in_params = false;
            continue;
        }
        let ch = match c {
            'L' => {
                for x in chars.by_ref() {
                    if x == ';' {
                        break;
                    }
                }
                'L'
            }
            '[' => {
                while chars.peek() == Some(&'[') {
                    chars.next();
                }
                if chars.peek() == Some(&'L') {
                    for x in chars.by_ref() {
                        if x == ';' {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
                'L'
            }
            other => other,
        };
        if in_params {
            params.push(ch);
        } else {
            ret.push(ch);
        }
    }
    format!("{}{}", if ret.is_empty() { "V" } else { &ret }, params)
}

pub(crate) fn default_return_for_descriptor(desc: &str) -> Value {
    let ret = desc.rsplit(')').next().unwrap_or("V");
    match ret.chars().next() {
        Some('V') | None => Value::Void,
        Some('J') => Value::Long(0),
        Some('F') => Value::Float(0.0),
        Some('D') => Value::Double(0.0),
        Some('L') | Some('[') => Value::Ref(None),
        _ => Value::Int(0),
    }
}

fn value_to_ffi(v: &Value) -> usize {
    match v {
        Value::Int(i) => *i as isize as usize,
        Value::Long(l) => *l as usize,
        Value::Ref(Some(id)) => *id,
        _ => 0,
    }
}

fn cmp_vals(a: &Value, b: &Value, kind: u8) -> bool {
    let ia = match a {
        Value::Int(i) => *i as isize,
        Value::Ref(Some(id)) => *id as isize,
        Value::Ref(None) => 0,
        _ => 0,
    };
    let ib = match b {
        Value::Int(i) => *i as isize,
        Value::Ref(Some(id)) => *id as isize,
        Value::Ref(None) => 0,
        _ => 0,
    };
    match kind {
        0 => ia == ib,
        1 => ia != ib,
        2 => ia < ib,
        3 => ia >= ib,
        4 => ia > ib,
        5 => ia <= ib,
        _ => false,
    }
}

fn eval_unary(op: u8, v: &Value) -> Value {
    match op {
        0x7b => Value::Int(-v.as_int().unwrap_or(0)),
        0x7c => Value::Int(!v.as_int().unwrap_or(0)),
        0x7d => Value::Long(-v.as_long().unwrap_or(0)),
        0x7e => Value::Long(!v.as_long().unwrap_or(0)),
        0x81 => Value::Long(v.as_int().unwrap_or(0) as i64),
        0x82 => Value::Float(v.as_int().unwrap_or(0) as f32),
        0x83 => Value::Double(v.as_int().unwrap_or(0) as f64),
        0x84 => Value::Int(v.as_long().unwrap_or(0) as i32),
        0x8d => Value::Int(v.as_int().unwrap_or(0) as i8 as i32),
        0x8e => Value::Int(v.as_int().unwrap_or(0) as u16 as i32),
        0x8f => Value::Int(v.as_int().unwrap_or(0) as i16 as i32),
        _ => v.clone(),
    }
}

fn eval_binary(op: u8, a: &Value, b: &Value) -> Value {
    let ia = a.as_int().unwrap_or(0);
    let ib = b.as_int().unwrap_or(0);
    match op {
        0x90 => Value::Int(ia.wrapping_add(ib)),
        0x91 => Value::Int(ia.wrapping_sub(ib)),
        0x92 => Value::Int(ia.wrapping_mul(ib)),
        0x93 => Value::Int(if ib != 0 { ia.wrapping_div(ib) } else { 0 }),
        0x94 => Value::Int(if ib != 0 { ia.wrapping_rem(ib) } else { 0 }),
        0x95 => Value::Int(ia & ib),
        0x96 => Value::Int(ia | ib),
        0x97 => Value::Int(ia ^ ib),
        0x98 => Value::Int(ia.wrapping_shl(ib as u32)),
        0x99 => Value::Int(ia.wrapping_shr(ib as u32)),
        0x9a => Value::Int(((ia as u32).wrapping_shr(ib as u32)) as i32),
        0x9b..=0xa5 => {
            let la = a.as_long().unwrap_or(0);
            let lb = b.as_long().unwrap_or(0);
            match op {
                0x9b => Value::Long(la.wrapping_add(lb)),
                0x9c => Value::Long(la.wrapping_sub(lb)),
                0x9d => Value::Long(la.wrapping_mul(lb)),
                0x9e => Value::Long(if lb != 0 { la.wrapping_div(lb) } else { 0 }),
                0x9f => Value::Long(if lb != 0 { la.wrapping_rem(lb) } else { 0 }),
                0xa0 => Value::Long(la & lb),
                0xa1 => Value::Long(la | lb),
                0xa2 => Value::Long(la ^ lb),
                0xa3 => Value::Long(la.wrapping_shl(lb as u32)),
                0xa4 => Value::Long(la.wrapping_shr(lb as u32)),
                0xa5 => Value::Long(((la as u64).wrapping_shr(lb as u32)) as i64),
                _ => Value::Long(0),
            }
        }
        0xa6..=0xaa => {
            let fa = a.as_float().unwrap_or(0.0);
            let fb = b.as_float().unwrap_or(0.0);
            match op {
                0xa6 => Value::Float(fa + fb),
                0xa7 => Value::Float(fa - fb),
                0xa8 => Value::Float(fa * fb),
                0xa9 => Value::Float(fa / fb),
                0xaa => Value::Float(fa % fb),
                _ => Value::Float(0.0),
            }
        }
        0xab..=0xaf => {
            let da = a.as_double().unwrap_or(0.0);
            let db = b.as_double().unwrap_or(0.0);
            match op {
                0xab => Value::Double(da + db),
                0xac => Value::Double(da - db),
                0xad => Value::Double(da * db),
                0xae => Value::Double(da / db),
                0xaf => Value::Double(da % db),
                _ => Value::Double(0.0),
            }
        }
        _ => Value::Int(0),
    }
}

fn eval_int_lit(kind: u8, a: i32, lit: i32) -> Value {
    match kind {
        0 => Value::Int(a.wrapping_add(lit)),
        1 => Value::Int(lit.wrapping_sub(a)), // rsub-int
        2 => Value::Int(a.wrapping_mul(lit)),
        3 => Value::Int(if lit != 0 { a.wrapping_div(lit) } else { 0 }),
        4 => Value::Int(if lit != 0 { a.wrapping_rem(lit) } else { 0 }),
        5 => Value::Int(a & lit),
        6 => Value::Int(a | lit),
        7 => Value::Int(a ^ lit),
        8 => Value::Int(a.wrapping_shl(lit as u32)),
        9 => Value::Int(a.wrapping_shr(lit as u32)),
        10 => Value::Int(((a as u32).wrapping_shr(lit as u32)) as i32),
        _ => Value::Int(0),
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.owns_runtime {
            self.shutdown_workers();
            if self.needs_bridge {
                if let Some(loader) = self.native_loader.take() { std::mem::forget(loader); }
                if let Some(resolver) = self.symbol_resolver.take() { std::mem::forget(resolver); }
                if let Some(wrapper) = self.pointer_wrapper.take() { std::mem::forget(wrapper); }
            }
        }
    }
}
