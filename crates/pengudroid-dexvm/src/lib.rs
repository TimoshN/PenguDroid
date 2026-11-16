//! PenguDroid's independent Dalvik DEX VM, JNI host, and core Java/Android runtime.
//!
//! Designed to execute Android `.dex` bytecode and JNI native libraries
//! without requiring Android ART (`libart.so`) or framework `.jar` modules.

pub mod dex;
pub mod jni_host;
pub mod stdlib;
mod stdlib_fs;
mod stdlib_sync;
mod native_abi;
mod jni_strings;
pub mod value;
pub mod vm;

pub use dex::DexFile;
pub use value::{ArrayData, Heap, HeapObject, ObjId, Value};
pub use vm::Vm;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_char, c_void, CStr, CString};

    fn push_uleb128(out: &mut Vec<u8>, mut val: u32) {
        loop {
            let mut b = (val & 0x7f) as u8;
            val >>= 7;
            if val != 0 {
                b |= 0x80;
            }
            out.push(b);
            if val == 0 {
                break;
            }
        }
    }

    fn push_mutf8(out: &mut Vec<u8>, s: &str) {
        push_uleb128(out, s.encode_utf16().count() as u32);
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }

    /// Build a minimal valid DEX `035` file with one class `LTest;` containing:
    /// - `public static int sumTo(int n)` (loop with `const/4`, `if-gt`, `add-int`, `return`)
    fn build_sum_dex() -> Vec<u8> {
        let strings = ["I", "LTest;", "Ljava/lang/Object;", "V", "sumTo"];
        let types = ["I", "LTest;", "Ljava/lang/Object;", "V"];
        // proto 0: (I)I -> shorty "II", ret=0(I), params=[0(I)]
        // method 0: LTest;.sumTo(I)I -> class=1, proto=0, name=4("sumTo")

        // Dalvik bytecode for sumTo(int n) [registers=3, ins=1 -> v2=n, v0=acc, v1=i]:
        // 0: const/4 v0, 0      -> 0x0012
        // 1: const/4 v1, 1      -> 0x1112
        // 2: if-gt v1, v2, +4   -> 0x2136, 0x0004 (jump to pc=6)
        // 4: add-int/2addr v0, v1 -> 0x10b0
        // 5: add-int/lit8 v1, v1, 1 -> 0x01d8, 0x0101
        // 6: goto -4            -> 0xfc28 (pc=6 -> pc=2)
        // 7: return v0          -> 0x000f
        // Wait: let's fix branch targets carefully:
        // pc=0: 0x0012 (const/4 v0, 0)
        // pc=1: 0x1112 (const/4 v1, 1)
        // pc=2: 0x2136, 0x0005 (if-gt v1, v2, +5 -> target pc=7)
        // pc=4: 0x10b0 (add-int/2addr v0, v1)
        // pc=5: 0x01d8, 0x0101 (add-int/lit8 v1, v1, 1)
        // pc=7: 0xfb28 -> wait, let's put goto at pc=7? No, if-gt target is return at pc=8:
        // pc=2: if-gt v1, v2, +6 -> target = 2 + 6 = 8
        // pc=4: add-int/2addr v0, v1
        // pc=5: add-int/lit8 v1, v1, 1 (2 units: 5, 6)
        // pc=7: goto -5 (2 - 7 = -5 = 0xfb) -> 0xfb28
        // pc=8: return v0 -> 0x000f
        let insns: [u16; 9] = [0x0012, 0x1112, 0x2136, 0x0006, 0x10b0, 0x01d8, 0x0101, 0xfb28, 0x000f];

        let mut buf = vec![0u8; 0x70];
        buf[0..8].copy_from_slice(b"dex\n035\0");

        // String data
        let mut str_offs = Vec::new();
        for s in &strings {
            str_offs.push(buf.len() as u32);
            push_mutf8(&mut buf, s);
        }
        while buf.len() % 4 != 0 {
            buf.push(0);
        }

        // string_ids
        let string_ids_off = buf.len() as u32;
        for o in &str_offs {
            buf.extend_from_slice(&o.to_le_bytes());
        }

        // type_ids (indices into strings: 0, 1, 2, 3)
        let type_ids_off = buf.len() as u32;
        for idx in 0..types.len() as u32 {
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        // proto params type_list: [I]
        let params_off = buf.len() as u32;
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        while buf.len() % 4 != 0 {
            buf.push(0);
        }

        // proto_ids
        let proto_ids_off = buf.len() as u32;
        buf.extend_from_slice(&0u32.to_le_bytes()); // shorty="I" (placeholder)
        buf.extend_from_slice(&0u32.to_le_bytes()); // ret=I
        buf.extend_from_slice(&params_off.to_le_bytes());

        // method_ids
        let method_ids_off = buf.len() as u32;
        buf.extend_from_slice(&1u16.to_le_bytes()); // class=LTest;
        buf.extend_from_slice(&0u16.to_le_bytes()); // proto=0
        buf.extend_from_slice(&4u32.to_le_bytes()); // name="sumTo"

        // code_item
        let code_off = buf.len() as u32;
        buf.extend_from_slice(&3u16.to_le_bytes()); // registers_size
        buf.extend_from_slice(&1u16.to_le_bytes()); // ins_size
        buf.extend_from_slice(&0u16.to_le_bytes()); // outs_size
        buf.extend_from_slice(&0u16.to_le_bytes()); // tries_size
        buf.extend_from_slice(&0u32.to_le_bytes()); // debug_info_off
        buf.extend_from_slice(&(insns.len() as u32).to_le_bytes());
        for w in &insns {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        while buf.len() % 4 != 0 {
            buf.push(0);
        }

        // class_data_item
        let class_data_off = buf.len() as u32;
        push_uleb128(&mut buf, 0); // static_fields
        push_uleb128(&mut buf, 0); // instance_fields
        push_uleb128(&mut buf, 1); // direct_methods
        push_uleb128(&mut buf, 0); // virtual_methods
        push_uleb128(&mut buf, 0); // method_idx_diff = 0
        push_uleb128(&mut buf, 0x0009); // public static
        push_uleb128(&mut buf, code_off);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }

        // class_defs
        let class_defs_off = buf.len() as u32;
        buf.extend_from_slice(&1u32.to_le_bytes()); // class_idx=LTest;
        buf.extend_from_slice(&0x0001u32.to_le_bytes()); // public
        buf.extend_from_slice(&2u32.to_le_bytes()); // superclass=Object
        buf.extend_from_slice(&0u32.to_le_bytes()); // interfaces
        buf.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // source_file
        buf.extend_from_slice(&0u32.to_le_bytes()); // annotations
        buf.extend_from_slice(&class_data_off.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // static_values

        // Patch header
        let file_size = buf.len() as u32;
        buf[0x20..0x24].copy_from_slice(&file_size.to_le_bytes());
        buf[0x24..0x28].copy_from_slice(&0x70u32.to_le_bytes());
        buf[0x38..0x3c].copy_from_slice(&(strings.len() as u32).to_le_bytes());
        buf[0x3c..0x40].copy_from_slice(&string_ids_off.to_le_bytes());
        buf[0x40..0x44].copy_from_slice(&(types.len() as u32).to_le_bytes());
        buf[0x44..0x48].copy_from_slice(&type_ids_off.to_le_bytes());
        buf[0x48..0x4c].copy_from_slice(&1u32.to_le_bytes());
        buf[0x4c..0x50].copy_from_slice(&proto_ids_off.to_le_bytes());
        buf[0x50..0x54].copy_from_slice(&0u32.to_le_bytes());
        buf[0x54..0x58].copy_from_slice(&0u32.to_le_bytes());
        buf[0x58..0x5c].copy_from_slice(&1u32.to_le_bytes());
        buf[0x5c..0x60].copy_from_slice(&method_ids_off.to_le_bytes());
        buf[0x60..0x64].copy_from_slice(&1u32.to_le_bytes());
        buf[0x64..0x68].copy_from_slice(&class_defs_off.to_le_bytes());
        buf
    }

    #[test]
    fn executes_dalvik_loop_without_art() {
        let dex_bytes = build_sum_dex();
        let mut vm = Vm::new();
        vm.load_dex(&dex_bytes).expect("parse synthetic dex");
        let res = vm
            .invoke_method("LTest;", "sumTo", "(I)I", &[Value::Int(10)], false)
            .expect("execute sumTo(10)");
        assert_eq!(res.as_int().unwrap(), 55);
    }

    #[test]
    fn indexed_multidex_preserves_first_class_and_rejects_invalid_method_transactionally() {
        let bytes = build_sum_dex();
        let mut vm = Vm::new();
        vm.load_dex(&bytes).unwrap();
        let mut duplicate = DexFile::parse(&bytes).unwrap();
        duplicate.classes[0].direct_methods.clear();
        vm.load_parsed_dex(duplicate).unwrap();
        assert_eq!(vm.invoke_method("LTest;", "sumTo", "(I)I", &[Value::Int(10)], false).unwrap().as_int().unwrap(), 55);
        let mut invalid = DexFile::parse(&bytes).unwrap();
        invalid.classes[0].name = "LInvalid;".into();
        invalid.classes[0].direct_methods[0].method_idx = usize::MAX;
        assert!(vm.load_parsed_dex(invalid).is_err());
        assert!(vm.find_class("LInvalid;").is_none());
        assert_eq!(vm.invoke_method("LTest;", "sumTo", "(I)I", &[Value::Int(3)], false).unwrap().as_int().unwrap(), 6);
    }

    #[test]
    fn direct_private_native_uses_object_receiver_instead_of_class() {
        unsafe extern "C" fn native(_: *mut c_void, receiver: *mut c_void, input: i32) -> i32 {
            let state = jni_host::jni_state().lock().unwrap();
            match state.heap.get(receiver as usize) {
                Some(HeapObject::Instance { class_name, .. }) if class_name == "LPrivateNativeCase;" => input + 10,
                _ => -1,
            }
        }
        let mut dex = DexFile::parse(&build_sum_dex()).unwrap();
        dex.classes[0].name = "LPrivateNativeCase;".into();
        dex.methods[0].class_name = "LPrivateNativeCase;".into();
        dex.classes[0].direct_methods[0].access_flags = 0x0102; // private native, not static
        dex.classes[0].direct_methods[0].code = None;
        let mut vm = Vm::new(); vm.load_parsed_dex(dex).unwrap();
        jni_host::register_native("LPrivateNativeCase;", "sumTo", "(I)I", native as *const () as usize);
        let object = vm.new_instance("LPrivateNativeCase;").unwrap();
        let result = vm.invoke_method("LPrivateNativeCase;", "sumTo", "(I)I", &[Value::Ref(Some(object)), Value::Int(7)], false).unwrap();
        assert_eq!(result.as_int().unwrap(), 17);
    }

    #[test]
    fn builtin_stdlib_stringbuilder_and_math_work() {
        let mut vm = Vm::new();
        let sb = vm.new_instance("Ljava/lang/StringBuilder;").unwrap();
        vm.invoke_method("Ljava/lang/StringBuilder;", "<init>", "()V", &[Value::Ref(Some(sb))], false).unwrap();
        let hello_id = {
            let mut st = jni_host::jni_state().lock().unwrap();
            st.heap.new_string("Pengu".into())
        };
        vm.invoke_method(
            "Ljava/lang/StringBuilder;",
            "append",
            "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
            &[Value::Ref(Some(sb)), Value::Ref(Some(hello_id))],
            true,
        ).unwrap();
        vm.invoke_method(
            "Ljava/lang/StringBuilder;",
            "append",
            "(I)Ljava/lang/StringBuilder;",
            &[Value::Ref(Some(sb)), Value::Int(17)],
            true,
        ).unwrap();
        let s_val = vm
            .invoke_method("Ljava/lang/StringBuilder;", "toString", "()Ljava/lang/String;", &[Value::Ref(Some(sb))], true)
            .unwrap();
        let sid = s_val.as_ref().unwrap();
        let st = jni_host::jni_state().lock().unwrap();
        assert_eq!(st.heap.as_str(sid), Some("Pengu17"));
    }

    #[test]
    fn c_jni_variadic_v_and_a_calls_preserve_arguments_and_virtual_receiver() {
        unsafe extern "C" { fn pengu_test_jni_calls(env: *mut c_void, object: *mut c_void,
            method: *mut c_void, results: *mut f64) -> i32; }
        unsafe fn record(_: usize, class: &str, name: &str, descriptor: &str, args: &[Value]) -> Value {
            let valid = class == "LJniBridgeCase;" && name == "mixed" && descriptor == "(IFDJIL)D"
                && args.len() == 7 && args[0].as_ref().is_some()
                && args[1].as_int().ok() == Some(-7) && args[2].as_float().ok() == Some(1.25)
                && args[3].as_double().ok() == Some(2.5) && args[4].as_long().ok() == Some(10_000_000_000)
                && args[5].as_int().ok() == Some(9) && args[6].as_ref() == Some(0x1234);
            Value::Double(if valid { 19.75 } else { -1.0 })
        }
        unsafe {
            let _guard = jni_host::scoped_vm_invoker(0, record);
            let env = jni_host::jni_env_ptr();
            let (object, method) = {
                let mut state = jni_host::jni_state().lock().unwrap();
                let object = state.heap.new_instance("LJniBridgeCase;");
                let method = state.method_ids.len();
                state.method_ids.push(("LSuperCase;".into(), "mixed".into(), "(IFDJIL)D".into()));
                (object, method)
            };
            let mut results = [0.0; 3];
            assert_eq!(pengu_test_jni_calls(env, object as *mut c_void, method as *mut c_void, results.as_mut_ptr()), 0);
            assert_eq!(results, [19.75; 3]);
        }
    }

    #[test]
    fn repeated_jni_id_lookups_are_stable_and_env_is_thread_local() {
        unsafe {
            let env = jni_host::jni_env_ptr();
            let table = *(env as *const *const *const c_void);
            type GetId = unsafe extern "C" fn(*mut c_void,*mut c_void,*const c_char,*const c_char) -> *mut c_void;
            let method: GetId = std::mem::transmute(*table.add(33));
            let field: GetId = std::mem::transmute(*table.add(94));
            let class = jni_host::jni_state().lock().unwrap().heap.class_object("LStableIdCase;") as *mut c_void;
            let name = CString::new("value").unwrap();
            let signature = CString::new("()I").unwrap();
            let first = method(env,class,name.as_ptr(),signature.as_ptr());
            assert_eq!(first,method(env,class,name.as_ptr(),signature.as_ptr()));
            let signature = CString::new("I").unwrap();
            let first = field(env,class,name.as_ptr(),signature.as_ptr());
            assert_eq!(first,field(env,class,name.as_ptr(),signature.as_ptr()));
            let main_address = env as usize;
            let child_address = std::thread::spawn(|| jni_host::jni_env_ptr() as usize).join().unwrap();
            assert_ne!(main_address,child_address);
            assert_eq!(jni_host::jni_env_ptr(),env);
        }
    }

    #[test]
    fn handler_obtain_message_keeps_target_and_payload() {
        let mut vm = Vm::new();
        let handler = vm.new_instance("Landroid/os/Handler;").unwrap();
        let message = vm.invoke_method("Landroid/os/Handler;", "obtainMessage", "(III)Landroid/os/Message;",
            &[Value::Ref(Some(handler)),Value::Int(7),Value::Int(11),Value::Int(13)],true).unwrap().as_ref().unwrap();
        let state = jni_host::jni_state().lock().unwrap();
        let Some(HeapObject::Instance { fields, .. }) = state.heap.get(message) else { panic!("not a Message") };
        assert_eq!(fields["target"].as_ref(),Some(handler));
        assert_eq!(fields["what"].as_int().unwrap(),7);
        assert_eq!(fields["arg1"].as_int().unwrap(),11);
        assert_eq!(fields["arg2"].as_int().unwrap(),13);
    }

    #[test]
    fn jni_host_c_abi_roundtrip_works() {
        unsafe {
            let env = jni_host::jni_env_ptr();
            let vtable = *(env as *const *const *const c_void);
            let get_ver: unsafe extern "C" fn(*mut c_void) -> i32 =
                std::mem::transmute(*vtable.add(4));
            assert_eq!(get_ver(env), jni_host::JNI_VERSION_1_6);

            let new_str: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void =
                std::mem::transmute(*vtable.add(167));
            let get_chars: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut u8) -> *const c_char =
                std::mem::transmute(*vtable.add(169));
            let rel_chars: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) =
                std::mem::transmute(*vtable.add(170));

            let input = CString::new("independent-runtime").unwrap();
            let jstr = new_str(env, input.as_ptr());
            assert!(!jstr.is_null());
            let mut is_copy = 0u8;
            let chars = get_chars(env, jstr, &mut is_copy);
            assert_eq!(CStr::from_ptr(chars).to_str().unwrap(), "independent-runtime");
            rel_chars(env, jstr, chars);
        }
    }
}
