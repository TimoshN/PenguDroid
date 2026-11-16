//! PenguDroid's independent Dalvik DEX VM, JNI host, and core Java/Android runtime.
//!
//! Designed to execute Android `.dex` bytecode and JNI native libraries
//! without requiring Android ART (`libart.so`) or framework `.jar` modules.

pub mod dex;
pub mod jni_host;
pub mod stdlib;
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
