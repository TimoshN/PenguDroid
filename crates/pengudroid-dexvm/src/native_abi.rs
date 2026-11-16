use crate::Value;
use std::ffi::{c_void, CString};
unsafe extern "C" {
    fn pengu_prepare_native(shorty: *const libc::c_char) -> *mut c_void;
    fn pengu_release_native(call: *mut c_void);
    fn pengu_call_native(
        call: *mut c_void,
        function: *mut c_void,
        cells: *mut u64,
        values: *mut *mut c_void,
        result: *mut u64,
    );
}
pub(crate) struct NativeCall {
    pointer: *mut c_void,
    shorty: Vec<u8>,
}
impl NativeCall {
    pub fn new(shorty: &str) -> Result<Self, String> {
        let text = CString::new(shorty).map_err(|_| "NUL in JNI shorty")?;
        let pointer = unsafe { pengu_prepare_native(text.as_ptr()) };
        if pointer.is_null() {
            return Err(format!("invalid or unsupported JNI shorty {shorty}"));
        }
        Ok(Self {
            pointer,
            shorty: shorty.as_bytes().to_vec(),
        })
    }
    pub unsafe fn call(
        &self,
        function: usize,
        env: *mut c_void,
        receiver: *mut c_void,
        args: &[Value],
    ) -> Result<Value, String> {
        if args.len() + 1 != self.shorty.len() {
            return Err(format!(
                "JNI argument count mismatch: expected {}, received {}",
                self.shorty.len() - 1,
                args.len()
            ));
        }
        let mut cells = Vec::with_capacity(args.len() + 2);
        cells.extend([env as u64, receiver as u64]);
        for (ty, value) in self.shorty[1..].iter().zip(args) {
            cells.push(match ty {
                b'F' => value.as_float()?.to_bits() as u64,
                b'D' => value.as_double()?.to_bits(),
                b'J' => value.as_long()? as u64,
                b'L' => value.as_ref().unwrap_or(0) as u64,
                _ => value.as_int()? as u64,
            });
        }
        let mut pointers = vec![std::ptr::null_mut(); cells.len()];
        let mut result = 0u64;
        pengu_call_native(
            self.pointer,
            function as *mut c_void,
            cells.as_mut_ptr(),
            pointers.as_mut_ptr(),
            &mut result,
        );
        Ok(match self.shorty[0] {
            b'V' => Value::Void,
            b'Z' => Value::Int((result as u8 != 0) as i32),
            b'B' => Value::Int(result as i8 as i32),
            b'C' => Value::Int(result as u16 as i32),
            b'S' => Value::Int(result as i16 as i32),
            b'I' => Value::Int(result as i32),
            b'J' => Value::Long(result as i64),
            b'F' => Value::Float(f32::from_bits(result as u32)),
            b'D' => Value::Double(f64::from_bits(result)),
            b'L' => Value::Ref(if result == 0 {
                None
            } else {
                Some(result as usize)
            }),
            _ => unreachable!(),
        })
    }
}
impl Drop for NativeCall {
    fn drop(&mut self) {
        unsafe {
            pengu_release_native(self.pointer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn mixed(
        env: *mut c_void,
        receiver: *mut c_void,
        a: i32,
        b: f32,
        c: f64,
        d: i64,
        e: i64,
        f: i32,
        object: *mut c_void,
    ) -> f64 {
        if env as usize != 0x11 || receiver as usize != 0x22 || object as usize != 0x1234 {
            return -1.0;
        }
        a as f64 + b as f64 + c + d as f64 + e as f64 + f as f64
    }
    unsafe extern "C" fn negative_byte(_: *mut c_void, _: *mut c_void) -> i8 {
        -7
    }
    #[test]
    fn native_calls_preserve_mixed_register_banks_stack_arguments_and_float_return() {
        let call = NativeCall::new("DIFDJJIL").unwrap();
        let args = [
            Value::Int(5),
            Value::Float(1.5),
            Value::Double(2.25),
            Value::Long(100),
            Value::Long(200),
            Value::Int(6),
            Value::Ref(Some(0x1234)),
        ];
        let value = unsafe {
            call.call(
                mixed as *const () as usize,
                0x11 as *mut c_void,
                0x22 as *mut c_void,
                &args,
            )
        }
        .unwrap();
        assert_eq!(value.as_double().unwrap(), 314.75);
        assert!(unsafe {
            call.call(
                mixed as *const () as usize,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &args[..3],
            )
        }
        .is_err());
    }
    #[test]
    fn signed_narrow_return_and_invalid_signatures() {
        let call = NativeCall::new("B").unwrap();
        let value = unsafe {
            call.call(
                negative_byte as *const () as usize,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &[],
            )
        }
        .unwrap();
        assert_eq!(value.as_int().unwrap(), -7);
        for bad in ["", "Q", "IV"] {
            assert!(NativeCall::new(bad).is_err());
        }
    }
}
