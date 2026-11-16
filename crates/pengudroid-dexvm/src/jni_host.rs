//! C-ABI `JavaVM` and `JNIEnv` host implementation for `pengudroid-dexvm`.
//!
//! Native `.so` libraries loaded via `System.loadLibrary` receive a real
//! `JavaVM*` / `JNIEnv*` backed by our Rust VM instead of `libart.so`.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};

use crate::value::{ArrayData, Heap, HeapObject, ObjId, Value};

pub const JNI_OK: c_int = 0;
pub const JNI_ERR: c_int = -1;
pub const JNI_VERSION_1_6: c_int = 0x0001_0006;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct JNINativeMethod {
    pub name: *const c_char,
    pub signature: *const c_char,
    pub fn_ptr: *mut c_void,
}

type JniOnLoadFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

#[derive(Default)]
pub struct JniRegistry {
    /// `(class_desc, method_name, signature) -> fn_ptr`
    pub natives: HashMap<(String, String, String), usize>,
    pub loaded_libs: Vec<String>,
    pub utf_pins: HashMap<usize, CString>,
}

static JNI_STATE: OnceLock<Mutex<JniState>> = OnceLock::new();

pub struct JniState {
    pub heap: Heap,
    pub registry: JniRegistry,
    pub method_ids: Vec<(String, String, String)>,
    pub field_ids: Vec<(String, String, String)>,
}

impl JniState {
    fn new() -> Self {
        Self {
            heap: Heap::new(),
            registry: JniRegistry::default(),
            method_ids: vec![(String::new(), String::new(), String::new())],
            field_ids: vec![(String::new(), String::new(), String::new())],
        }
    }
}

pub fn jni_state() -> &'static Mutex<JniState> {
    JNI_STATE.get_or_init(|| Mutex::new(JniState::new()))
}

pub fn register_native(class_desc: &str, name: &str, sig: &str, fn_ptr: usize) {
    let mut st = jni_state().lock().unwrap();
    st.registry
        .natives
        .insert((class_desc.to_string(), name.to_string(), sig.to_string()), fn_ptr);
}

pub fn lookup_native(class_desc: &str, name: &str, sig: &str) -> Option<usize> {
    let st = jni_state().lock().unwrap();
    st.registry
        .natives
        .get(&(class_desc.to_string(), name.to_string(), sig.to_string()))
        .copied()
}

pub fn load_native_library(path_or_name: &str) -> Result<(), String> {
    let c_path = CString::new(path_or_name).map_err(|_| "invalid library name")?;
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let err = unsafe {
            let p = libc::dlerror();
            if p.is_null() {
                "unknown dlopen error".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        return Err(format!("dlopen({path_or_name}) failed: {err}"));
    }
    jni_state()
        .lock()
        .unwrap()
        .registry
        .loaded_libs
        .push(path_or_name.to_string());

    let sym = unsafe { libc::dlsym(handle, b"JNI_OnLoad\0".as_ptr() as *const c_char) };
    if !sym.is_null() {
        let on_load: JniOnLoadFn = unsafe { std::mem::transmute(sym) };
        let vm_ptr = java_vm_ptr();
        let ver = unsafe { on_load(vm_ptr, std::ptr::null_mut()) };
        if ver < 0 {
            return Err(format!("JNI_OnLoad({path_or_name}) returned {ver}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// C-ABI JavaVM & JNIEnv vtables
// ---------------------------------------------------------------------------

#[repr(C)]
struct JavaVMVtable {
    reserved0: *const c_void,
    reserved1: *const c_void,
    reserved2: *const c_void,
    destroy_java_vm: unsafe extern "C" fn(*mut c_void) -> c_int,
    attach_current_thread: unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut c_void) -> c_int,
    detach_current_thread: unsafe extern "C" fn(*mut c_void) -> c_int,
    get_env: unsafe extern "C" fn(*mut c_void, *mut *mut c_void, c_int) -> c_int,
    attach_current_thread_as_daemon:
        unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut c_void) -> c_int,
}

unsafe impl Sync for JavaVMVtable {}

static JAVA_VM_VTABLE: JavaVMVtable = JavaVMVtable {
    reserved0: std::ptr::null(),
    reserved1: std::ptr::null(),
    reserved2: std::ptr::null(),
    destroy_java_vm: vm_destroy,
    attach_current_thread: vm_attach,
    detach_current_thread: vm_detach,
    get_env: vm_get_env,
    attach_current_thread_as_daemon: vm_attach,
};

#[repr(C)]
struct JavaVMObj {
    functions: *const JavaVMVtable,
}

static mut JAVA_VM_OBJ: JavaVMObj = JavaVMObj {
    functions: &JAVA_VM_VTABLE,
};

#[repr(C)]
struct JNIEnvObj {
    functions: *const *const c_void,
}

static JNI_ENV_TABLE: OnceLock<Vec<usize>> = OnceLock::new();
static mut JNI_ENV_OBJ: JNIEnvObj = JNIEnvObj {
    functions: std::ptr::null(),
};

macro_rules! fill_unimpl {
    ($t:expr, $($i:expr),* $(,)?) => {
        $(
            $t[$i] = jni_unimplemented::<$i> as *const () as usize;
        )*
    };
}

pub fn jni_env_ptr() -> *mut c_void {
    let table = JNI_ENV_TABLE.get_or_init(|| {
        let mut t = vec![jni_unimplemented::<999> as *const () as usize; 233];
        fill_unimpl!(
            t, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
            20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39,
            40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59,
            60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79,
            80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99,
            100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119,
            120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139,
            140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159,
            160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179,
            180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199,
            200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219,
            220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232
        );
        t[4] = jni_get_version as *const () as usize;
        t[6] = jni_find_class as *const () as usize;
        t[10] = jni_get_superclass as *const () as usize;
        t[13] = jni_throw as *const () as usize;
        t[14] = jni_throw_new as *const () as usize;
        t[15] = jni_exception_occurred as *const () as usize;
        t[16] = jni_exception_describe as *const () as usize;
        t[17] = jni_exception_clear as *const () as usize;
        t[19] = jni_push_local_frame as *const () as usize;
        t[20] = jni_pop_local_frame as *const () as usize;
        t[21] = jni_new_global_ref as *const () as usize;
        t[22] = jni_delete_global_ref as *const () as usize;
        t[23] = jni_delete_local_ref as *const () as usize;
        t[24] = jni_is_same_object as *const () as usize;
        t[25] = jni_new_local_ref as *const () as usize;
        t[26] = jni_ensure_local_capacity as *const () as usize;
        t[27] = jni_alloc_object as *const () as usize;
        t[30] = jni_new_object_a as *const () as usize;
        t[31] = jni_get_object_class as *const () as usize;
        t[32] = jni_is_instance_of as *const () as usize;
        t[33] = jni_get_method_id as *const () as usize;
        t[34] = jni_call_object_method as *const () as usize;
        t[36] = jni_call_object_method_a as *const () as usize;
        t[37] = jni_call_boolean_method as *const () as usize;
        t[39] = jni_call_boolean_method_a as *const () as usize;
        t[49] = jni_call_int_method as *const () as usize;
        t[51] = jni_call_int_method_a as *const () as usize;
        t[52] = jni_call_long_method as *const () as usize;
        t[54] = jni_call_long_method_a as *const () as usize;
        t[55] = jni_call_float_method as *const () as usize;
        t[57] = jni_call_float_method_a as *const () as usize;
        t[61] = jni_call_void_method as *const () as usize;
        t[63] = jni_call_void_method_a as *const () as usize;
        t[94] = jni_get_field_id as *const () as usize;
        t[95] = jni_get_object_field as *const () as usize;
        t[100] = jni_get_int_field as *const () as usize;
        t[113] = jni_get_static_method_id as *const () as usize;
        t[114] = jni_call_static_object_method as *const () as usize;
        t[115] = jni_call_static_object_method_v as *const () as usize;
        t[116] = jni_call_static_object_method_a as *const () as usize;
        t[117] = jni_call_static_boolean_method as *const () as usize;
        t[119] = jni_call_static_boolean_method_a as *const () as usize;
        t[129] = jni_call_static_int_method as *const () as usize;
        t[131] = jni_call_static_int_method_a as *const () as usize;
        t[141] = jni_call_static_void_method as *const () as usize;
        t[143] = jni_call_static_void_method_a as *const () as usize;
        t[144] = jni_get_static_field_id as *const () as usize;
        t[145] = jni_get_static_object_field as *const () as usize;
        t[150] = jni_get_static_int_field as *const () as usize;
        t[167] = jni_new_string_utf as *const () as usize;
        t[168] = jni_get_string_utf_length as *const () as usize;
        t[169] = jni_get_string_utf_chars as *const () as usize;
        t[170] = jni_release_string_utf_chars as *const () as usize;
        t[171] = jni_get_array_length as *const () as usize;
        t[172] = jni_new_object_array as *const () as usize;
        t[173] = jni_get_object_array_element as *const () as usize;
        t[174] = jni_set_object_array_element as *const () as usize;
        t[176] = jni_new_byte_array as *const () as usize;
        t[179] = jni_new_int_array as *const () as usize;
        t[184] = jni_get_byte_array_elements as *const () as usize;
        t[192] = jni_release_byte_array_elements as *const () as usize;
        t[215] = jni_register_natives as *const () as usize;
        t[216] = jni_unregister_natives as *const () as usize;
        t[219] = jni_get_java_vm as *const () as usize;
        t[228] = jni_exception_check as *const () as usize;
        t
    });
    unsafe {
        JNI_ENV_OBJ.functions = table.as_ptr() as *const *const c_void;
        &raw mut JNI_ENV_OBJ as *mut c_void
    }
}

pub fn java_vm_ptr() -> *mut c_void {
    let _ = jni_env_ptr();
    &raw mut JAVA_VM_OBJ as *mut c_void
}

unsafe extern "C" fn vm_destroy(_vm: *mut c_void) -> c_int {
    JNI_OK
}

unsafe extern "C" fn vm_attach(_vm: *mut c_void, p_env: *mut *mut c_void, _args: *mut c_void) -> c_int {
    if !p_env.is_null() {
        *p_env = jni_env_ptr();
    }
    JNI_OK
}

unsafe extern "C" fn vm_detach(_vm: *mut c_void) -> c_int {
    JNI_OK
}

unsafe extern "C" fn vm_get_env(_vm: *mut c_void, p_env: *mut *mut c_void, _version: c_int) -> c_int {
    if !p_env.is_null() {
        *p_env = jni_env_ptr();
    }
    JNI_OK
}

unsafe extern "C" fn jni_unimplemented<const SLOT: usize>() -> usize {
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] UNIMPLEMENTED JNI slot {SLOT} called!");
    }
    0
}

unsafe extern "C" fn jni_get_version(_env: *mut c_void) -> c_int {
    JNI_VERSION_1_6
}

fn cstr_to_desc(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
    if s.starts_with('L') && s.ends_with(';') {
        s.into_owned()
    } else if s.starts_with('[') {
        s.into_owned()
    } else {
        format!("L{};", s.replace('.', "/"))
    }
}

unsafe extern "C" fn jni_find_class(_env: *mut c_void, name: *const c_char) -> *mut c_void {
    let desc = cstr_to_desc(name);
    let mut st = jni_state().lock().unwrap();
    let id = st.heap.class_object(&desc);
    id as *mut c_void
}

unsafe extern "C" fn jni_get_superclass(_env: *mut c_void, _cls: *mut c_void) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    st.heap.class_object("Ljava/lang/Object;") as *mut c_void
}

unsafe extern "C" fn jni_throw(_env: *mut c_void, _obj: *mut c_void) -> c_int {
    0
}

unsafe extern "C" fn jni_throw_new(_env: *mut c_void, _cls: *mut c_void, msg: *const c_char) -> c_int {
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() && !msg.is_null() {
        eprintln!("[pengu-jni] ThrowNew: {:?}", CStr::from_ptr(msg));
    }
    0
}

unsafe extern "C" fn jni_exception_occurred(_env: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn jni_exception_describe(_env: *mut c_void) {}
unsafe extern "C" fn jni_exception_clear(_env: *mut c_void) {}
unsafe extern "C" fn jni_exception_check(_env: *mut c_void) -> u8 {
    0
}

unsafe extern "C" fn jni_new_global_ref(_env: *mut c_void, obj: *mut c_void) -> *mut c_void {
    obj
}

unsafe extern "C" fn jni_delete_global_ref(_env: *mut c_void, _obj: *mut c_void) {}
unsafe extern "C" fn jni_delete_local_ref(_env: *mut c_void, _obj: *mut c_void) {}

unsafe extern "C" fn jni_is_same_object(_env: *mut c_void, a: *mut c_void, b: *mut c_void) -> u8 {
    u8::from(a == b)
}

unsafe extern "C" fn jni_new_local_ref(_env: *mut c_void, obj: *mut c_void) -> *mut c_void {
    obj
}

pub type VmInvokeFn = unsafe fn(usize, &str, &str, &str, &[Value]) -> Value;

std::thread_local! {
    static VM_INVOKER: std::cell::Cell<Option<(usize, VmInvokeFn)>> = const { std::cell::Cell::new(None) };
}

pub fn set_vm_invoker(ctx: usize, f: VmInvokeFn) {
    VM_INVOKER.with(|c| c.set(Some((ctx, f))));
}

fn invoke_via_vm(class_desc: &str, name: &str, sig: &str, args: &[Value]) -> Value {
    let invoker = VM_INVOKER.with(|c| c.get());
    if let Some((ctx, f)) = invoker {
        unsafe { f(ctx, class_desc, name, sig, args) }
    } else {
        Value::Void
    }
}

unsafe fn unpack_jvalues(sig: &str, args_ptr: *const u64) -> Vec<Value> {
    let mut out = Vec::new();
    if args_ptr.is_null() {
        return out;
    }
    let inner = sig
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    let mut chars = inner.chars().peekable();
    let mut idx = 0usize;
    while let Some(c) = chars.next() {
        let raw = *args_ptr.add(idx);
        idx += 1;
        match c {
            'Z' | 'B' | 'C' | 'S' | 'I' => out.push(Value::Int(raw as i32)),
            'J' => out.push(Value::Long(raw as i64)),
            'F' => out.push(Value::Float(f32::from_bits(raw as u32))),
            'D' => out.push(Value::Double(f64::from_bits(raw))),
            'L' => {
                for x in chars.by_ref() {
                    if x == ';' {
                        break;
                    }
                }
                let id = raw as usize;
                out.push(Value::Ref(if id == 0 { None } else { Some(id) }));
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
                let id = raw as usize;
                out.push(Value::Ref(if id == 0 { None } else { Some(id) }));
            }
            _ => out.push(Value::Int(raw as i32)),
        }
    }
    out
}

unsafe extern "C" fn jni_push_local_frame(_env: *mut c_void, _cap: c_int) -> c_int {
    JNI_OK
}

unsafe extern "C" fn jni_pop_local_frame(_env: *mut c_void, result: *mut c_void) -> *mut c_void {
    result
}

unsafe extern "C" fn jni_ensure_local_capacity(_env: *mut c_void, _cap: c_int) -> c_int {
    JNI_OK
}

unsafe extern "C" fn jni_alloc_object(_env: *mut c_void, cls: *mut c_void) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    let cname = match st.heap.get(cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => c.clone(),
        _ => "Ljava/lang/Object;".to_string(),
    };
    st.heap.new_instance(&cname) as *mut c_void
}

unsafe extern "C" fn jni_new_object_a(
    _env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> *mut c_void {
    let (obj_id, cname, mname, msig) = {
        let mut st = jni_state().lock().unwrap();
        let cname = match st.heap.get(cls as ObjId) {
            Some(HeapObject::ClassObj(c)) => c.clone(),
            _ => "Ljava/lang/Object;".to_string(),
        };
        let obj_id = st.heap.new_instance(&cname);
        let (mc, mn, ms) = st
            .method_ids
            .get(mid as usize)
            .cloned()
            .unwrap_or_else(|| (cname.clone(), "<init>".to_string(), "()V".to_string()));
        (obj_id, mc, mn, ms)
    };
    let mut args = vec![Value::Ref(Some(obj_id))];
    args.extend(unpack_jvalues(&msig, args_ptr));
    let _ = invoke_via_vm(&cname, &mname, &msig, &args);
    obj_id as *mut c_void
}

unsafe extern "C" fn jni_get_object_class(_env: *mut c_void, obj: *mut c_void) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    let cname = st
        .heap
        .get(obj as ObjId)
        .map(|o| o.class_name().to_string())
        .unwrap_or_else(|| "Ljava/lang/Object;".to_string());
    st.heap.class_object(&cname) as *mut c_void
}

unsafe extern "C" fn jni_is_instance_of(
    _env: *mut c_void,
    obj: *mut c_void,
    cls: *mut c_void,
) -> u8 {
    if obj.is_null() || cls.is_null() {
        return 0;
    }
    let st = jni_state().lock().unwrap();
    let Some(obj_cls) = st.heap.get(obj as ObjId).map(|o| o.class_name()) else {
        return 0;
    };
    let Some(HeapObject::ClassObj(target_cls)) = st.heap.get(cls as ObjId) else {
        return 0;
    };
    u8::from(obj_cls == target_cls || target_cls == "Ljava/lang/Object;")
}

unsafe extern "C" fn jni_call_object_method(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> *mut c_void {
    let raw = [a1, a2, a3];
    jni_call_object_method_a(_env, obj, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_object_method_a(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> *mut c_void {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return std::ptr::null_mut();
        };
        entry
    };
    let mut args = vec![Value::Ref(if obj.is_null() {
        None
    } else {
        Some(obj as ObjId)
    })];
    args.extend(unpack_jvalues(&msig, args_ptr));
    let res = match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Ref(Some(id)) => id as *mut c_void,
        _ => std::ptr::null_mut(),
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] CallStaticObjectMethodA {cname}.{mname}{msig} -> {res:p}");
    }
    res
}

unsafe extern "C" fn jni_call_boolean_method(
    env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> u8 {
    let raw = [a1, a2, a3];
    jni_call_boolean_method_a(env, obj, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_int_method(
    env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> c_int {
    let raw = [a1, a2, a3];
    jni_call_int_method_a(env, obj, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_long_method(
    env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> i64 {
    let raw = [a1, a2, a3];
    jni_call_long_method_a(env, obj, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_float_method(
    env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> f32 {
    let raw = [a1, a2, a3];
    jni_call_float_method_a(env, obj, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_void_method(
    env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) {
    let raw = [a1, a2, a3];
    jni_call_void_method_a(env, obj, mid, raw.as_ptr());
}

unsafe extern "C" fn jni_call_static_object_method(
    env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> *mut c_void {
    let raw = [a1, a2, a3];
    jni_call_static_object_method_a(env, cls, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_static_void_method(
    env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) {
    let raw = [a1, a2, a3];
    jni_call_static_void_method_a(env, cls, mid, raw.as_ptr());
}

unsafe extern "C" fn jni_call_static_boolean_method(
    env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> u8 {
    let raw = [a1, a2, a3];
    jni_call_static_boolean_method_a(env, cls, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_static_boolean_method_a(
    _env: *mut c_void,
    _cls: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> u8 {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return 0;
        };
        entry
    };
    let args = unpack_jvalues(&msig, args_ptr);
    match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Int(v) => u8::from(v != 0),
        _ => 0,
    }
}

unsafe extern "C" fn jni_call_static_int_method(
    env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    a1: u64,
    a2: u64,
    a3: u64,
) -> c_int {
    let raw = [a1, a2, a3];
    jni_call_static_int_method_a(env, cls, mid, raw.as_ptr())
}

unsafe extern "C" fn jni_call_static_int_method_a(
    _env: *mut c_void,
    _cls: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> c_int {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return 0;
        };
        entry
    };
    let args = unpack_jvalues(&msig, args_ptr);
    match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Int(v) => v,
        _ => 0,
    }
}

unsafe extern "C" fn jni_call_boolean_method_a(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> u8 {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return 0;
        };
        entry
    };
    let mut args = vec![Value::Ref(if obj.is_null() {
        None
    } else {
        Some(obj as ObjId)
    })];
    args.extend(unpack_jvalues(&msig, args_ptr));
    match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Int(v) => u8::from(v != 0),
        _ => 0,
    }
}

unsafe extern "C" fn jni_call_int_method_a(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> c_int {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return 0;
        };
        entry
    };
    let mut args = vec![Value::Ref(if obj.is_null() {
        None
    } else {
        Some(obj as ObjId)
    })];
    args.extend(unpack_jvalues(&msig, args_ptr));
    match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Int(v) => v,
        Value::Long(v) => v as c_int,
        _ => 0,
    }
}

unsafe extern "C" fn jni_call_long_method_a(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> i64 {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return 0;
        };
        entry
    };
    let mut args = vec![Value::Ref(if obj.is_null() {
        None
    } else {
        Some(obj as ObjId)
    })];
    args.extend(unpack_jvalues(&msig, args_ptr));
    match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    }
}

unsafe extern "C" fn jni_call_float_method_a(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> f32 {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return 0.0;
        };
        entry
    };
    let mut args = vec![Value::Ref(if obj.is_null() {
        None
    } else {
        Some(obj as ObjId)
    })];
    args.extend(unpack_jvalues(&msig, args_ptr));
    match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Float(v) => v,
        Value::Double(v) => v as f32,
        Value::Int(v) => v as f32,
        _ => 0.0,
    }
}

unsafe extern "C" fn jni_call_void_method_a(
    _env: *mut c_void,
    obj: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return;
        };
        entry
    };
    let mut args = vec![Value::Ref(if obj.is_null() {
        None
    } else {
        Some(obj as ObjId)
    })];
    args.extend(unpack_jvalues(&msig, args_ptr));
    let _ = invoke_via_vm(&cname, &mname, &msig, &args);
}

unsafe extern "C" fn jni_get_object_field(
    _env: *mut c_void,
    obj: *mut c_void,
    fid: *mut c_void,
) -> *mut c_void {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize) else {
        return std::ptr::null_mut();
    };
    let res = match st.heap.get(obj as ObjId) {
        Some(HeapObject::Instance { fields, .. }) => fields
            .get(fname)
            .and_then(|v| v.as_ref())
            .map(|id| id as *mut c_void)
            .unwrap_or(std::ptr::null_mut()),
        _ => std::ptr::null_mut(),
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetObjectField .{fname} -> {res:p}");
    }
    res
}

unsafe extern "C" fn jni_get_int_field(
    _env: *mut c_void,
    obj: *mut c_void,
    fid: *mut c_void,
) -> c_int {
    if obj.is_null() {
        return 0;
    }
    let st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize) else {
        return 0;
    };
    let res = match st.heap.get(obj as ObjId) {
        Some(HeapObject::Instance { fields, .. }) => fields
            .get(fname)
            .and_then(|v| v.as_int().ok())
            .unwrap_or(0),
        _ => 0,
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetIntField .{fname} -> {res}");
    }
    res
}

unsafe extern "C" fn jni_call_static_object_method_a(
    _env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) -> *mut c_void {
    let (cname, mname, msig) = {
        let mut st = jni_state().lock().unwrap();
        let Some(mut entry) = st.method_ids.get(mid as usize).cloned() else {
            return std::ptr::null_mut();
        };
        if entry.0.is_empty() {
            if let Some(HeapObject::ClassObj(c)) = st.heap.get(cls as ObjId) {
                entry.0 = c.clone();
            }
        }
        entry
    };
    let args = unpack_jvalues(&msig, args_ptr);
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] CallStaticObjectMethodA {cname}.{mname}{msig}");
    }
    let res = match invoke_via_vm(&cname, &mname, &msig, &args) {
        Value::Ref(Some(id)) => id as *mut c_void,
        _ => std::ptr::null_mut(),
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] CallStaticObjectMethodA {cname}.{mname}{msig} -> {res:p}");
    }
    res
}

unsafe extern "C" fn jni_call_static_void_method_a(
    _env: *mut c_void,
    cls: *mut c_void,
    mid: *mut c_void,
    args_ptr: *const u64,
) {
    let (cname, mname, msig) = {
        let mut st = jni_state().lock().unwrap();
        let Some(mut entry) = st.method_ids.get(mid as usize).cloned() else {
            return;
        };
        if entry.0.is_empty() {
            if let Some(HeapObject::ClassObj(c)) = st.heap.get(cls as ObjId) {
                entry.0 = c.clone();
            }
        }
        entry
    };
    let args = unpack_jvalues(&msig, args_ptr);
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] CallStaticVoidMethodA {cname}.{mname}{msig}");
    }
    let _ = invoke_via_vm(&cname, &mname, &msig, &args);
}

unsafe extern "C" fn jni_get_static_object_field(
    _env: *mut c_void,
    _cls: *mut c_void,
    fid: *mut c_void,
) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    let Some((cname, fname, _fsig)) = st.field_ids.get(fid as usize).cloned() else {
        return std::ptr::null_mut();
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetStaticObjectField {cname}.{fname}");
    }
    match (cname.as_str(), fname.as_str()) {
        ("Landroid/os/Build;", "MODEL") => st.heap.new_string("PenguDroid".into()) as *mut c_void,
        ("Landroid/os/Build;", "MANUFACTURER" | "BRAND") => {
            st.heap.new_string("PenguDroid".into()) as *mut c_void
        }
        ("Landroid/os/Build$VERSION;", "RELEASE") => {
            st.heap.new_string("15".into()) as *mut c_void
        }
        ("Landroid/content/Context;", "WINDOW_SERVICE") => {
            st.heap.new_string("window".into()) as *mut c_void
        }
        ("Landroid/content/Context;", "DISPLAY_SERVICE") => {
            st.heap.new_string("display".into()) as *mut c_void
        }
        ("Landroid/content/Context;", "AUDIO_SERVICE") => {
            st.heap.new_string("audio".into()) as *mut c_void
        }
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn jni_get_static_int_field(
    _env: *mut c_void,
    _cls: *mut c_void,
    fid: *mut c_void,
) -> c_int {
    let st = jni_state().lock().unwrap();
    let Some((cname, fname, _fsig)) = st.field_ids.get(fid as usize) else {
        return 0;
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetStaticIntField {cname}.{fname}");
    }
    match (cname.as_str(), fname.as_str()) {
        ("Landroid/os/Build$VERSION;", "SDK_INT") => 37,
        _ => 0,
    }
}

unsafe extern "C" fn jni_call_static_object_method_v(
    _env: *mut c_void,
    _cls: *mut c_void,
    mid: *mut c_void,
    _va: *mut c_void,
) -> *mut c_void {
    let (cname, mname, msig) = {
        let st = jni_state().lock().unwrap();
        let Some(entry) = st.method_ids.get(mid as usize).cloned() else {
            return std::ptr::null_mut();
        };
        entry
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] CallStaticObjectMethodV {cname}.{mname}{msig}");
    }
    match invoke_via_vm(&cname, &mname, &msig, &[]) {
        Value::Ref(Some(id)) => id as *mut c_void,
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn jni_get_method_id(
    _env: *mut c_void,
    cls: *mut c_void,
    name: *const c_char,
    sig: *const c_char,
) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    let cname = match st.heap.get(cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => c.clone(),
        _ => "Ljava/lang/Object;".to_string(),
    };
    let mname = CStr::from_ptr(name).to_string_lossy().into_owned();
    let msig = CStr::from_ptr(sig).to_string_lossy().into_owned();
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetMethodID {cname}.{mname}{msig}");
    }
    let idx = st.method_ids.len();
    st.method_ids.push((cname, mname, msig));
    idx as *mut c_void
}

unsafe extern "C" fn jni_get_static_method_id(
    env: *mut c_void,
    cls: *mut c_void,
    name: *const c_char,
    sig: *const c_char,
) -> *mut c_void {
    jni_get_method_id(env, cls, name, sig)
}

unsafe extern "C" fn jni_get_field_id(
    _env: *mut c_void,
    cls: *mut c_void,
    name: *const c_char,
    sig: *const c_char,
) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    let cname = match st.heap.get(cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => c.clone(),
        _ => "Ljava/lang/Object;".to_string(),
    };
    let fname = CStr::from_ptr(name).to_string_lossy().into_owned();
    let fsig = CStr::from_ptr(sig).to_string_lossy().into_owned();
    let idx = st.field_ids.len();
    st.field_ids.push((cname, fname, fsig));
    idx as *mut c_void
}

unsafe extern "C" fn jni_get_static_field_id(
    env: *mut c_void,
    cls: *mut c_void,
    name: *const c_char,
    sig: *const c_char,
) -> *mut c_void {
    jni_get_field_id(env, cls, name, sig)
}

unsafe extern "C" fn jni_new_string_utf(_env: *mut c_void, bytes: *const c_char) -> *mut c_void {
    if bytes.is_null() {
        return std::ptr::null_mut();
    }
    let s = CStr::from_ptr(bytes).to_string_lossy().into_owned();
    let mut st = jni_state().lock().unwrap();
    st.heap.new_string(s) as *mut c_void
}

unsafe extern "C" fn jni_get_string_utf_length(_env: *mut c_void, s: *mut c_void) -> c_int {
    let st = jni_state().lock().unwrap();
    st.heap.as_str(s as ObjId).map(|x| x.len() as c_int).unwrap_or(0)
}

unsafe extern "C" fn jni_get_string_utf_chars(
    _env: *mut c_void,
    s: *mut c_void,
    is_copy: *mut u8,
) -> *const c_char {
    if !is_copy.is_null() {
        *is_copy = 1;
    }
    let mut st = jni_state().lock().unwrap();
    let text = st.heap.as_str(s as ObjId).unwrap_or("").to_string();
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetStringUTFChars(id={}) -> {:?}", s as usize, text);
    }
    let cs = CString::new(text).unwrap_or_default();
    let ptr = cs.as_ptr();
    st.registry.utf_pins.insert(ptr as usize, cs);
    ptr
}

unsafe extern "C" fn jni_release_string_utf_chars(
    _env: *mut c_void,
    _s: *mut c_void,
    chars: *const c_char,
) {
    if !chars.is_null() {
        jni_state()
            .lock()
            .unwrap()
            .registry
            .utf_pins
            .remove(&(chars as usize));
    }
}

unsafe extern "C" fn jni_get_array_length(_env: *mut c_void, arr: *mut c_void) -> c_int {
    let st = jni_state().lock().unwrap();
    match st.heap.get(arr as ObjId) {
        Some(HeapObject::Array { data, .. }) => data.len() as c_int,
        _ => 0,
    }
}

unsafe extern "C" fn jni_new_object_array(
    _env: *mut c_void,
    len: c_int,
    element_cls: *mut c_void,
    init: *mut c_void,
) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    let elem_desc = match st.heap.get(element_cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => format!("[{c}"),
        _ => "[Ljava/lang/Object;".to_string(),
    };
    let n = len.max(0) as usize;
    let init_val = if init.is_null() {
        None
    } else {
        Some(init as ObjId)
    };
    st.heap.alloc(HeapObject::Array {
        type_desc: elem_desc,
        data: ArrayData::Object(vec![init_val; n]),
    }) as *mut c_void
}

unsafe extern "C" fn jni_get_object_array_element(
    _env: *mut c_void,
    arr: *mut c_void,
    idx: c_int,
) -> *mut c_void {
    if arr.is_null() || idx < 0 {
        return std::ptr::null_mut();
    }
    let st = jni_state().lock().unwrap();
    match st.heap.get(arr as ObjId) {
        Some(HeapObject::Array {
            data: ArrayData::Object(v),
            ..
        }) => v
            .get(idx as usize)
            .and_then(|o| *o)
            .map(|id| id as *mut c_void)
            .unwrap_or(std::ptr::null_mut()),
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn jni_set_object_array_element(
    _env: *mut c_void,
    arr: *mut c_void,
    idx: c_int,
    val: *mut c_void,
) {
    if arr.is_null() || idx < 0 {
        return;
    }
    let mut st = jni_state().lock().unwrap();
    if let Some(HeapObject::Array {
        data: ArrayData::Object(v),
        ..
    }) = st.heap.get_mut(arr as ObjId)
    {
        if let Some(slot) = v.get_mut(idx as usize) {
            *slot = if val.is_null() {
                None
            } else {
                Some(val as ObjId)
            };
        }
    }
}

unsafe extern "C" fn jni_new_int_array(_env: *mut c_void, len: c_int) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    st.heap
        .new_array("[I", len.max(0) as usize)
        .unwrap_or(0) as *mut c_void
}

unsafe extern "C" fn jni_new_byte_array(_env: *mut c_void, len: c_int) -> *mut c_void {
    let mut st = jni_state().lock().unwrap();
    st.heap
        .new_array("[B", len.max(0) as usize)
        .unwrap_or(0) as *mut c_void
}

unsafe extern "C" fn jni_get_byte_array_elements(
    _env: *mut c_void,
    arr: *mut c_void,
    is_copy: *mut u8,
) -> *mut i8 {
    if !is_copy.is_null() {
        *is_copy = 0;
    }
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    let mut st = jni_state().lock().unwrap();
    match st.heap.get_mut(arr as ObjId) {
        Some(HeapObject::Array {
            data: ArrayData::Byte(v),
            ..
        }) => v.as_mut_ptr(),
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn jni_release_byte_array_elements(
    _env: *mut c_void,
    _arr: *mut c_void,
    _elems: *mut i8,
    _mode: c_int,
) {
}

unsafe extern "C" fn jni_register_natives(
    _env: *mut c_void,
    cls: *mut c_void,
    methods: *const JNINativeMethod,
    n_methods: c_int,
) -> c_int {
    if methods.is_null() || n_methods < 0 {
        return JNI_ERR;
    }
    let mut st = jni_state().lock().unwrap();
    let cname = match st.heap.get(cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => c.clone(),
        _ => return JNI_ERR,
    };
    for i in 0..(n_methods as usize) {
        let m = &*methods.add(i);
        if m.name.is_null() || m.signature.is_null() {
            continue;
        }
        let mname = CStr::from_ptr(m.name).to_string_lossy().into_owned();
        let msig = CStr::from_ptr(m.signature).to_string_lossy().into_owned();
        st.registry
            .natives
            .insert((cname.clone(), mname, msig), m.fn_ptr as usize);
    }
    JNI_OK
}

unsafe extern "C" fn jni_unregister_natives(_env: *mut c_void, _cls: *mut c_void) -> c_int {
    JNI_OK
}

unsafe extern "C" fn jni_get_java_vm(_env: *mut c_void, vm: *mut *mut c_void) -> c_int {
    if !vm.is_null() {
        *vm = java_vm_ptr();
    }
    JNI_OK
}

pub fn alloc_vm_value_from_array_data(data: ArrayData, type_desc: &str) -> Value {
    let mut st = jni_state().lock().unwrap();
    let id = st.heap.alloc(HeapObject::Array {
        type_desc: type_desc.to_string(),
        data,
    });
    Value::Ref(Some(id))
}

pub unsafe extern "C" fn bridge_get_method_shorty(
    _env: *mut c_void,
    mid: *mut c_void,
) -> *const c_char {
    let idx = mid as usize;
    let mut st = jni_state().lock().unwrap();
    let sig = st
        .method_ids
        .get(idx)
        .map(|(_, _, s)| s.clone())
        .unwrap_or_else(|| "()V".to_string());
    let mut shorty = String::new();
    let mut chars = sig.chars().peekable();
    let mut in_params = false;
    let mut ret_str = String::new();
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
            shorty.push(ch);
        } else {
            ret_str.push(ch);
        }
    }
    let full = format!("{}{}", if ret_str.is_empty() { "V" } else { &ret_str }, shorty);
    let cs = CString::new(full).unwrap_or_default();
    let ptr = cs.as_ptr();
    st.registry.utf_pins.insert(ptr as usize, cs);
    ptr
}

pub unsafe extern "C" fn bridge_get_native_method_count(
    _env: *mut c_void,
    cls: *mut c_void,
) -> u32 {
    let st = jni_state().lock().unwrap();
    let cname = match st.heap.get(cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => c.as_str(),
        _ => return 0,
    };
    st.registry
        .natives
        .keys()
        .filter(|(c, _, _)| c == cname)
        .count() as u32
}

pub unsafe extern "C" fn bridge_get_native_methods(
    _env: *mut c_void,
    cls: *mut c_void,
    methods: *mut c_void,
    max_count: u32,
) -> u32 {
    if methods.is_null() || max_count == 0 {
        return 0;
    }
    let mut st = jni_state().lock().unwrap();
    let cname = match st.heap.get(cls as ObjId) {
        Some(HeapObject::ClassObj(c)) => c.clone(),
        _ => return 0,
    };
    let entries: Vec<(String, String, usize)> = st
        .registry
        .natives
        .iter()
        .filter(|((c, _, _), _)| c == &cname)
        .take(max_count as usize)
        .map(|((_, n, s), &p)| (n.clone(), s.clone(), p))
        .collect();
    let out = methods as *mut JNINativeMethod;
    for (i, (name, sig, ptr)) in entries.iter().enumerate() {
        let cn = CString::new(name.as_str()).unwrap_or_default();
        let cs = CString::new(sig.as_str()).unwrap_or_default();
        let np = cn.as_ptr();
        let sp = cs.as_ptr();
        st.registry.utf_pins.insert(np as usize, cn);
        st.registry.utf_pins.insert(sp as usize, cs);
        *out.add(i) = JNINativeMethod {
            name: np,
            signature: sp,
            fn_ptr: *ptr as *mut c_void,
        };
    }
    entries.len() as u32
}
