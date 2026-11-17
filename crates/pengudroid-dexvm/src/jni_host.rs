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
    pub utf16_pins: HashMap<usize, Box<[u16]>>,
}

static JNI_STATE: OnceLock<Mutex<JniState>> = OnceLock::new();
static WIRED_JNI_SLOTS: OnceLock<Vec<usize>> = OnceLock::new();
static MISSING_JNI: [std::sync::atomic::AtomicU64; 233] = [const { std::sync::atomic::AtomicU64::new(0) }; 233];

pub fn wired_jni_slots() -> &'static [usize] {
    let _ = jni_env_ptr();
    WIRED_JNI_SLOTS.get().unwrap()
}
pub fn missing_jni_calls() -> Vec<(usize, u64)> {
    MISSING_JNI.iter().enumerate().filter_map(|(slot, count)| {
        let count = count.load(std::sync::atomic::Ordering::Relaxed);
        (count != 0).then_some((slot, count))
    }).collect()
}


pub struct JniState {
    pub heap: Heap,
    pub registry: JniRegistry,
    pub method_ids: Vec<(String, String, String)>,
    method_index: HashMap<(String, String, String), usize>,
    pub field_ids: Vec<(String, String, String)>,
    field_index: HashMap<(String, String, String), usize>,
}

impl JniState {
    fn new() -> Self {
        Self {
            heap: Heap::new(),
            registry: JniRegistry::default(),
            method_ids: vec![(String::new(), String::new(), String::new())],
            method_index: HashMap::new(),
            field_ids: vec![(String::new(), String::new(), String::new())],
            field_index: HashMap::new(),
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
std::thread_local! {
    static JNI_ATTACHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static JNI_ENV_OBJ: std::cell::UnsafeCell<JNIEnvObj> = const {
        std::cell::UnsafeCell::new(JNIEnvObj { functions: std::ptr::null() })
    };
}

macro_rules! fill_unimpl {
    ($t:expr, $($i:expr),* $(,)?) => {
        $(
            $t[$i] = jni_unimplemented::<$i> as *const () as usize;
        )*
    };
}

pub fn jni_env_ptr() -> *mut c_void {
    JNI_ATTACHED.with(|attached| attached.set(true));
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
        let placeholders = t.clone();
        t[4] = jni_get_version as *const () as usize;
        t[6] = jni_find_class as *const () as usize;
        t[7] = jni_from_reflected_method as *const () as usize;
        t[8] = jni_from_reflected_field as *const () as usize;
        t[9] = jni_to_reflected_method as *const () as usize;
        t[11] = jni_is_assignable_from as *const () as usize;
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
        t[94] = jni_get_field_id as *const () as usize;
        t[95] = jni_get_object_field as *const () as usize;
        t[96] = jni_get_boolean_field as *const () as usize;
        t[97] = jni_get_byte_field as *const () as usize;
        t[98] = jni_get_char_field as *const () as usize;
        t[99] = jni_get_short_field as *const () as usize;
        t[100] = jni_get_int_field as *const () as usize;
        t[101] = jni_get_long_field as *const () as usize;
        t[102] = jni_get_float_field as *const () as usize;
        t[103] = jni_get_double_field as *const () as usize;
        t[104] = jni_set_object_field as *const () as usize;
        t[105] = jni_set_boolean_field as *const () as usize;
        t[109] = jni_set_int_field as *const () as usize;
        t[110] = jni_set_long_field as *const () as usize;
        t[113] = jni_get_static_method_id as *const () as usize;
        t[144] = jni_get_static_field_id as *const () as usize;
        t[145] = jni_get_static_object_field as *const () as usize;
        t[150] = jni_get_static_int_field as *const () as usize;
        t[163] = crate::jni_strings::new_string as *const () as usize;
        t[164] = crate::jni_strings::get_string_length as *const () as usize;
        t[165] = crate::jni_strings::get_string_chars as *const () as usize;
        t[166] = crate::jni_strings::release_string_chars as *const () as usize;
        t[169] = crate::jni_strings::get_string_utf_chars as *const () as usize;
        t[220] = crate::jni_strings::get_string_region as *const () as usize;
        t[221] = crate::jni_strings::get_string_utf_region as *const () as usize;
        t[224] = crate::jni_strings::get_string_chars as *const () as usize;
        t[225] = crate::jni_strings::release_string_chars as *const () as usize;
        t[167] = crate::jni_strings::new_string_utf as *const () as usize;
        t[168] = crate::jni_strings::get_string_utf_length as *const () as usize;
        t[169] = crate::jni_strings::get_string_utf_chars as *const () as usize;
        t[170] = crate::jni_strings::release_string_utf_chars as *const () as usize;
        t[171] = jni_get_array_length as *const () as usize;
        t[172] = jni_new_object_array as *const () as usize;
        t[173] = jni_get_object_array_element as *const () as usize;
        t[174] = jni_set_object_array_element as *const () as usize;
        t[176] = jni_new_byte_array as *const () as usize;
        t[179] = jni_new_int_array as *const () as usize;
        t[184] = jni_get_byte_array_elements as *const () as usize;
        t[187] = jni_get_int_array_elements as *const () as usize;
        t[192] = jni_release_byte_array_elements as *const () as usize;
        t[195] = jni_release_int_array_elements as *const () as usize;
        t[215] = jni_register_natives as *const () as usize;
        t[216] = jni_unregister_natives as *const () as usize;
        t[217] = jni_monitor_enter as *const () as usize;
        t[218] = jni_monitor_exit as *const () as usize;
        t[219] = jni_get_java_vm as *const () as usize;
        t[226] = jni_new_global_ref as *const () as usize;
        t[227] = jni_delete_global_ref as *const () as usize;
        t[228] = jni_exception_check as *const () as usize;
        t[229] = jni_new_direct_byte_buffer as *const () as usize;
        t[230] = jni_get_direct_buffer_address as *const () as usize;
        t[231] = jni_get_direct_buffer_capacity as *const () as usize;
        unsafe extern "C" { fn pengu_install_jni_calls(table: *mut usize); }
        unsafe { pengu_install_jni_calls(t.as_mut_ptr()); }
        let _ = WIRED_JNI_SLOTS.set((4..t.len()).filter(|&i| t[i] != placeholders[i]).collect());
        t
    });
    JNI_ENV_OBJ.with(|object| unsafe {
        (*object.get()).functions = table.as_ptr() as *const *const c_void;
        object.get().cast()
    })
}

pub fn java_vm_ptr() -> *mut c_void {
    let _ = jni_env_ptr();
    &raw mut JAVA_VM_OBJ as *mut c_void
}

unsafe extern "C" fn vm_destroy(_vm: *mut c_void) -> c_int {
    JNI_OK
}

unsafe extern "C" fn vm_attach(_vm: *mut c_void, p_env: *mut *mut c_void, _args: *mut c_void) -> c_int {
    if p_env.is_null() { return JNI_ERR; }
    crate::threads::attach_native_thread();
    if !p_env.is_null() {
        *p_env = jni_env_ptr();
    }
    JNI_OK
}

unsafe extern "C" fn vm_detach(_vm: *mut c_void) -> c_int {
    if !crate::threads::detach_native_thread() { return JNI_ERR; }
    let _ = JNI_ATTACHED.try_with(|attached| attached.set(false));
    JNI_OK
}

unsafe extern "C" fn vm_get_env(_vm: *mut c_void, p_env: *mut *mut c_void, version: c_int) -> c_int {
    if p_env.is_null() { return JNI_ERR; }
    *p_env = std::ptr::null_mut();
    if !matches!(version, 0x00010001 | 0x00010002 | 0x00010004 | JNI_VERSION_1_6) { return -3; }
    if !JNI_ATTACHED.try_with(|attached| attached.get()).unwrap_or(false) { return -2; }
    crate::threads::attach_native_thread();
    if !p_env.is_null() {
        *p_env = jni_env_ptr();
    }
    JNI_OK
}

unsafe extern "C" fn jni_unimplemented<const SLOT: usize>() -> usize {
    let first = MISSING_JNI.get(SLOT).map(|count| count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0).unwrap_or(true);
    if first { eprintln!("[pengu-jni] UNIMPLEMENTED JNI slot {SLOT} called"); }
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

unsafe extern "C" fn jni_get_superclass(_env:*mut c_void,class:*mut c_void)->*mut c_void {
    invoke_via_vm("Ljava/lang/Class;","getSuperclass","()Ljava/lang/Class;",&[Value::Ref((!class.is_null()).then_some(class as usize))]).as_ref().map_or(std::ptr::null_mut(),|id|id as *mut c_void)
}

std::thread_local! {
    static PENDING_JNI_EXCEPTION: std::cell::Cell<Option<ObjId>> = const { std::cell::Cell::new(None) };
}
pub(crate) fn set_pending_exception_object(object: ObjId) {
    PENDING_JNI_EXCEPTION.with(|slot| slot.set(Some(object)));
}
pub(crate) fn take_pending_exception() -> Option<ObjId> {
    PENDING_JNI_EXCEPTION.with(|slot| slot.take())
}
pub(crate) fn raise_exception(class: &str, message: &str) {
    let mut state = jni_state().lock().unwrap();
    let text = state.heap.new_string(message.into());
    let object = state.heap.new_instance(class);
    if let Some(HeapObject::Instance { fields, .. }) = state.heap.get_mut(object) {
        fields.insert("detailMessage".into(), Value::Ref(Some(text)));
    }
    drop(state);
    set_pending_exception_object(object);
}
unsafe extern "C" fn jni_throw(_env: *mut c_void, object: *mut c_void) -> c_int {
    if object.is_null() { return JNI_ERR; }
    set_pending_exception_object(object as usize);
    JNI_OK
}
unsafe extern "C" fn jni_throw_new(_env: *mut c_void, class: *mut c_void, message: *const c_char) -> c_int {
    let name = {
        let state = jni_state().lock().unwrap();
        match state.heap.get(class as usize) {
            Some(HeapObject::ClassObj(name)) => name.clone(),
            _ => return JNI_ERR,
        }
    };
    let message = if message.is_null() { String::new() } else { CStr::from_ptr(message).to_string_lossy().into_owned() };
    raise_exception(&name, &message);
    JNI_OK
}
unsafe extern "C" fn jni_exception_occurred(_env: *mut c_void) -> *mut c_void {
    PENDING_JNI_EXCEPTION.with(|slot| slot.get().unwrap_or(0) as *mut c_void)
}
unsafe extern "C" fn jni_exception_describe(_env: *mut c_void) {
    if let Some(object) = PENDING_JNI_EXCEPTION.with(|slot| slot.get()) {
        let state = jni_state().lock().unwrap();
        if let Some(value) = state.heap.get(object) { eprintln!("[pengu-jni] pending {}", value.class_name()); }
    }
}
unsafe extern "C" fn jni_exception_clear(_env: *mut c_void) { take_pending_exception(); }
static DIRECT_BUFFERS: OnceLock<Mutex<HashMap<ObjId, Box<[u8]>>>> = OnceLock::new();

fn direct_buffers() -> &'static Mutex<HashMap<ObjId, Box<[u8]>>> {
    DIRECT_BUFFERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn alloc_direct_buffer(id: ObjId, cap: usize) {
    let buf = vec![0u8; cap].into_boxed_slice();
    direct_buffers().lock().unwrap().insert(id, buf);
}

pub(crate) fn direct_buffer_slice_mut<F, R>(id: ObjId, f: F) -> Option<R>
where
    F: FnOnce(&mut [u8]) -> R,
{
    let mut map = direct_buffers().lock().unwrap();
    map.get_mut(&id).map(|b| f(b))
}

unsafe extern "C" fn jni_new_direct_byte_buffer(
    _env: *mut c_void,
    address: *mut c_void,
    capacity: i64,
) -> *mut c_void {
    if address.is_null() || capacity < 0 {
        return std::ptr::null_mut();
    }
    let mut st = jni_state().lock().unwrap();
    let id = st.heap.new_instance("Ljava/nio/ByteBuffer;");
    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(id) {
        fields.insert("capacity".into(), Value::Int(capacity as i32));
        fields.insert("limit".into(), Value::Int(capacity as i32));
        fields.insert("position".into(), Value::Int(0));
        fields.insert("ext_addr".into(), Value::Long(address as usize as i64));
    }
    id as *mut c_void
}

unsafe extern "C" fn jni_get_direct_buffer_address(_env: *mut c_void, buf: *mut c_void) -> *mut c_void {
    if buf.is_null() {
        return std::ptr::null_mut();
    }
    let id = buf as ObjId;
    if let Some(b) = direct_buffers().lock().unwrap().get_mut(&id) {
        return b.as_mut_ptr() as *mut c_void;
    }
    let st = jni_state().lock().unwrap();
    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get(id) {
        if let Some(Ok(addr)) = fields.get("ext_addr").map(|v| v.as_long()) {
            return addr as usize as *mut c_void;
        }
    }
    std::ptr::null_mut()
}

unsafe extern "C" fn jni_get_direct_buffer_capacity(_env: *mut c_void, buf: *mut c_void) -> i64 {
    if buf.is_null() {
        return -1;
    }
    let id = buf as ObjId;
    if let Some(b) = direct_buffers().lock().unwrap().get(&id) {
        return b.len() as i64;
    }
    let st = jni_state().lock().unwrap();
    match st.heap.get(id) {
        Some(HeapObject::Instance { fields, .. }) => fields
            .get("capacity")
            .and_then(|v| v.as_int().ok())
            .map(|c| c as i64)
            .unwrap_or(-1),
        _ => -1,
    }
}

unsafe extern "C" fn jni_exception_check(_env: *mut c_void) -> u8 {
    PENDING_JNI_EXCEPTION.with(|slot| slot.get().is_some() as u8)
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

pub(crate) fn has_vm_invoker() -> bool { VM_INVOKER.try_with(|slot| slot.get().is_some()).unwrap_or(false) }
pub(crate) fn clear_vm_invoker() { let _ = VM_INVOKER.try_with(|slot| slot.set(None)); }

pub fn set_vm_invoker(ctx: usize, f: VmInvokeFn) {
    let _ = VM_INVOKER.try_with(|c| c.set(Some((ctx, f))));
}

pub struct VmInvokerGuard(Option<(usize, VmInvokeFn)>);
impl Drop for VmInvokerGuard {
    fn drop(&mut self) { let _ = VM_INVOKER.try_with(|slot| slot.set(self.0)); }
}
pub fn scoped_vm_invoker(ctx: usize, callback: VmInvokeFn) -> VmInvokerGuard {
    VmInvokerGuard(VM_INVOKER.with(|slot| slot.replace(Some((ctx, callback)))))
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
        .class_name(obj as ObjId).map(str::to_owned)
        .unwrap_or_else(|| "Ljava/lang/Object;".to_string());
    st.heap.class_object(&cname) as *mut c_void
}

unsafe extern "C" fn jni_is_instance_of(_env:*mut c_void,object:*mut c_void,class:*mut c_void)->u8 {
    if object.is_null(){return 1;}
    invoke_via_vm("Ljava/lang/Class;","isInstance","(Ljava/lang/Object;)Z",&[Value::Ref((!class.is_null()).then_some(class as usize)),Value::Ref(Some(object as usize))]).as_int().unwrap_or(0) as u8
}
unsafe extern "C" fn jni_is_assignable_from(_env:*mut c_void,source:*mut c_void,target:*mut c_void)->u8 {
    invoke_via_vm("Ljava/lang/Class;","isAssignableFrom","(Ljava/lang/Class;)Z",&[Value::Ref((!target.is_null()).then_some(target as usize)),Value::Ref((!source.is_null()).then_some(source as usize))]).as_int().unwrap_or(0) as u8
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

unsafe extern "C" fn jni_set_object_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void, val: *mut c_void) {
    if obj.is_null() { return; }
    let mut st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize).cloned() else { return; };
    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(obj as ObjId) {
        fields.insert(fname, Value::Ref((!val.is_null()).then_some(val as ObjId)));
    }
}

unsafe extern "C" fn jni_set_boolean_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void, val: u8) {
    if obj.is_null() { return; }
    let mut st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize).cloned() else { return; };
    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(obj as ObjId) {
        fields.insert(fname, Value::Int((val != 0) as i32));
    }
}

unsafe extern "C" fn jni_set_int_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void, val: c_int) {
    if obj.is_null() { return; }
    let mut st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize).cloned() else { return; };
    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(obj as ObjId) {
        fields.insert(fname, Value::Int(val));
    }
}

unsafe extern "C" fn jni_set_long_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void, val: i64) {
    if obj.is_null() { return; }
    let mut st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize).cloned() else { return; };
    if let Some(HeapObject::Instance { fields, .. }) = st.heap.get_mut(obj as ObjId) {
        fields.insert(fname, Value::Long(val));
    }
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

unsafe extern "C" fn jni_get_boolean_field(env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> u8 {
    (jni_get_int_field(env, obj, fid) != 0) as u8
}
unsafe extern "C" fn jni_get_byte_field(env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> i8 {
    jni_get_int_field(env, obj, fid) as i8
}
unsafe extern "C" fn jni_get_char_field(env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> u16 {
    jni_get_int_field(env, obj, fid) as u16
}
unsafe extern "C" fn jni_get_short_field(env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> i16 {
    jni_get_int_field(env, obj, fid) as i16
}
unsafe extern "C" fn jni_get_long_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> i64 {
    if obj.is_null() { return 0; }
    let st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize) else { return 0; };
    match st.heap.get(obj as ObjId) {
        Some(HeapObject::Instance { fields, .. }) => fields.get(fname).and_then(|v| v.as_long().ok()).unwrap_or(0),
        _ => 0,
    }
}
unsafe extern "C" fn jni_get_float_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> f32 {
    if obj.is_null() { return 0.0; }
    let st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize) else { return 0.0; };
    match st.heap.get(obj as ObjId) {
        Some(HeapObject::Instance { fields, .. }) => match fields.get(fname) {
            Some(Value::Float(f)) => *f,
            _ => 0.0,
        },
        _ => 0.0,
    }
}
unsafe extern "C" fn jni_get_double_field(_env: *mut c_void, obj: *mut c_void, fid: *mut c_void) -> f64 {
    if obj.is_null() { return 0.0; }
    let st = jni_state().lock().unwrap();
    let Some((_, fname, _)) = st.field_ids.get(fid as usize) else { return 0.0; };
    match st.heap.get(obj as ObjId) {
        Some(HeapObject::Instance { fields, .. }) => match fields.get(fname) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

static GLOBAL_STATICS: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();

fn global_statics() -> &'static Mutex<HashMap<String, Value>> {
    GLOBAL_STATICS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn set_static_field(key: &str, value: Value) {
    global_statics().lock().unwrap().insert(key.to_string(), value);
}

unsafe extern "C" fn jni_get_static_object_field(
    _env: *mut c_void,
    _cls: *mut c_void,
    fid: *mut c_void,
) -> *mut c_void {
    let (cname, fname) = {
        let st = jni_state().lock().unwrap();
        let Some((cname, fname, _fsig)) = st.field_ids.get(fid as usize) else {
            return std::ptr::null_mut();
        };
        (cname.clone(), fname.clone())
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetStaticObjectField {cname}.{fname}");
    }
    let key = format!("{cname}.{fname}");
    if let Some(val) = global_statics().lock().unwrap().get(&key).cloned() {
        return val.as_ref().map_or(std::ptr::null_mut(), |id| id as *mut c_void);
    }
    if cname == "Lcom/unity3d/player/UnityPlayer;" && fname == "currentActivity" {
        if let Some(val) = global_statics().lock().unwrap().get("PENGUDROID_ACTIVITY").cloned() {
            return val.as_ref().map_or(std::ptr::null_mut(), |id| id as *mut c_void);
        }
    }
    let mut st = jni_state().lock().unwrap();
    match (cname.as_str(), fname.as_str()) {
        ("Landroid/os/Build;", "MODEL" | "MANUFACTURER" | "BRAND" | "DEVICE" | "PRODUCT" | "HARDWARE" | "BOARD" | "ID" | "FINGERPRINT") => {
            st.heap.new_string("PenguDroid".into()) as *mut c_void
        }
        ("Landroid/os/Build$VERSION;", "RELEASE" | "INCREMENTAL") => {
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
    let (cname, fname) = {
        let st = jni_state().lock().unwrap();
        let Some((cname, fname, _fsig)) = st.field_ids.get(fid as usize) else {
            return 0;
        };
        (cname.clone(), fname.clone())
    };
    if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
        eprintln!("[pengu-jni] GetStaticIntField {cname}.{fname}");
    }
    let key = format!("{cname}.{fname}");
    if let Some(val) = global_statics().lock().unwrap().get(&key).cloned() {
        if let Ok(i) = val.as_int() {
            return i;
        }
    }
    match (cname.as_str(), fname.as_str()) {
        ("Landroid/os/Build$VERSION;", "SDK_INT") => 37,
        _ => 0,
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
    let key = (cname, mname, msig);
    if let Some(&index) = st.method_index.get(&key) { return index as *mut c_void; }
    let index = st.method_ids.len();
    st.method_ids.push(key.clone());
    st.method_index.insert(key, index);
    index as *mut c_void
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
    let key = (cname, fname, fsig);
    if let Some(&index) = st.field_index.get(&key) { return index as *mut c_void; }
    let index = st.field_ids.len();
    st.field_ids.push(key.clone());
    st.field_index.insert(key, index);
    index as *mut c_void
}

unsafe extern "C" fn jni_get_static_field_id(
    env: *mut c_void,
    cls: *mut c_void,
    name: *const c_char,
    sig: *const c_char,
) -> *mut c_void {
    jni_get_field_id(env, cls, name, sig)
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

unsafe extern "C" fn jni_get_int_array_elements(
    _env: *mut c_void,
    arr: *mut c_void,
    is_copy: *mut u8,
) -> *mut c_int {
    if !is_copy.is_null() {
        *is_copy = 0;
    }
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    let mut st = jni_state().lock().unwrap();
    match st.heap.get_mut(arr as ObjId) {
        Some(HeapObject::Array {
            data: ArrayData::Int(v),
            ..
        }) => v.as_mut_ptr(),
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn jni_release_int_array_elements(
    _env: *mut c_void,
    _arr: *mut c_void,
    _elems: *mut c_int,
    _mode: c_int,
) {}

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

#[no_mangle]
unsafe extern "C" fn pengu_jni_shorty(method: *mut c_void, out: *mut c_char, size: usize) -> c_int {
    if out.is_null() { return -1; }
    let state = jni_state().lock().unwrap();
    let Some((_, _, descriptor)) = state.method_ids.get(method as usize) else { return -1; };
    let shorty = crate::vm::descriptor_to_shorty(descriptor);
    if shorty.len() >= size { return -1; }
    std::ptr::copy_nonoverlapping(shorty.as_ptr(), out.cast(), shorty.len());
    *out.add(shorty.len()) = 0;
    shorty.len() as c_int
}

#[no_mangle]
unsafe extern "C" fn pengu_jni_dispatch(env: *mut c_void, object: *mut c_void, method: *mut c_void,
    mode: c_int, values: *const u64) -> u64 {
    if mode == 3 { return jni_new_object_a(env, object, method, values) as u64; }
    let (mut class, name, signature) = {
        let state = jni_state().lock().unwrap();
        let Some(entry) = state.method_ids.get(method as usize) else { return 0; };
        entry.clone()
    };
    let mut args = Vec::new();
    if mode != 1 {
        if mode == 0 && !object.is_null() {
            let state = jni_state().lock().unwrap();
            if let Some(value) = state.heap.class_name(object as usize) { class = value.into(); }
        }
        args.push(Value::Ref(if object.is_null() { None } else { Some(object as usize) }));
    }
    args.extend(unpack_jvalues(&signature, values));
    match invoke_via_vm(&class, &name, &signature, &args) {
        Value::Void => 0,
        Value::Int(value) => value as u64,
        Value::Long(value) => value as u64,
        Value::Float(value) => value.to_bits() as u64,
        Value::Double(value) => value.to_bits(),
        Value::Ref(value) => value.unwrap_or(0) as u64,
    }
}

unsafe extern "C" fn jni_from_reflected_method(_env:*mut c_void,object:*mut c_void)->*mut c_void {
    let Ok((class,name,sig,_))=crate::reflection::method_info(object as usize) else { return std::ptr::null_mut(); };
    let mut state=jni_state().lock().unwrap();let key=(class,name,sig);
    if let Some(&id)=state.method_index.get(&key){return id as *mut c_void}
    let id=state.method_ids.len();state.method_ids.push(key.clone());state.method_index.insert(key,id);id as *mut c_void
}
unsafe extern "C" fn jni_from_reflected_field(_env:*mut c_void,object:*mut c_void)->*mut c_void {
    let Ok((class,name,sig,_))=crate::reflection::method_info(object as usize) else { return std::ptr::null_mut(); };
    let mut state=jni_state().lock().unwrap();let key=(class,name,sig);
    if let Some(&id)=state.field_index.get(&key){return id as *mut c_void}
    let id=state.field_ids.len();state.field_ids.push(key.clone());state.field_index.insert(key,id);id as *mut c_void
}
unsafe extern "C" fn jni_to_reflected_method(_env:*mut c_void,_class:*mut c_void,id:*mut c_void,is_static:u8)->*mut c_void {
    let entry=jni_state().lock().unwrap().method_ids.get(id as usize).cloned();
    entry.map(|(class,name,sig)|crate::reflection::method(&class,&name,&sig,if is_static!=0{9}else{1}) as *mut c_void).unwrap_or(std::ptr::null_mut())
}

unsafe extern "C" fn jni_monitor_enter(_env:*mut c_void,object:*mut c_void)->c_int {
    if object.is_null() { return JNI_ERR; } crate::monitors::enter(object as usize);JNI_OK
}
unsafe extern "C" fn jni_monitor_exit(_env:*mut c_void,object:*mut c_void)->c_int {
    if object.is_null() { return JNI_ERR; }
    match crate::monitors::exit(object as usize) {Ok(())=>JNI_OK,Err(_)=>{let id=jni_state().lock().unwrap().heap.new_instance("Ljava/lang/IllegalMonitorStateException;");set_pending_exception_object(id);JNI_ERR}}
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    #[test]
    fn native_threads_must_attach_and_get_env_rejects_bad_versions_and_detached_threads() {
        let vm=java_vm_ptr() as usize;
        std::thread::spawn(move||unsafe {
            let table=*(vm as *const *const usize);
            let get:unsafe extern "C" fn(*mut c_void,*mut *mut c_void,c_int)->c_int=std::mem::transmute(*table.add(6));
            let attach:unsafe extern "C" fn(*mut c_void,*mut *mut c_void,*mut c_void)->c_int=std::mem::transmute(*table.add(4));
            let detach:unsafe extern "C" fn(*mut c_void)->c_int=std::mem::transmute(*table.add(5));
            let mut env=std::ptr::null_mut();
            assert_eq!(get(vm as *mut c_void,&mut env,JNI_VERSION_1_6),-2);assert!(env.is_null());
            assert_eq!(attach(vm as *mut c_void,&mut env,std::ptr::null_mut()),JNI_OK);assert!(!env.is_null());
            let previous=env;assert_eq!(get(vm as *mut c_void,&mut env,JNI_VERSION_1_6),JNI_OK);assert_eq!(previous,env);
            assert_eq!(get(vm as *mut c_void,&mut env,0x7fff_ffff),-3);assert!(env.is_null());
            assert_eq!(detach(vm as *mut c_void),JNI_OK);assert_eq!(get(vm as *mut c_void,&mut env,JNI_VERSION_1_6),-2);assert!(env.is_null());
        }).join().unwrap();
    }
}
