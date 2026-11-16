//! Atomic operations are serialized by the VM heap mutex.
use crate::stdlib::BuiltinResult;
use crate::{Heap, HeapObject, Value};
pub fn try_invoke(
    heap: &mut Heap,
    class: &str,
    method: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    let kind = match class {
        "Ljava/util/concurrent/atomic/AtomicInteger;" => 0,
        "Ljava/util/concurrent/atomic/AtomicLong;" => 1,
        "Ljava/util/concurrent/atomic/AtomicReference;" => 2,
        _ => return Ok(BuiltinResult::NotBuiltin),
    };
    let default = match kind {
        0 => Value::Int(0),
        1 => Value::Long(0),
        _ => Value::Ref(None),
    };
    let object = args
        .first()
        .and_then(Value::as_ref)
        .ok_or("null atomic receiver")?;
    let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(object) else {
        return Err("invalid atomic receiver".into());
    };
    let old = fields.get("value").cloned().unwrap_or(default.clone());
    let next = args.get(1).cloned().unwrap_or(default);
    let mut set = |value| {
        fields.insert("value".into(), value);
    };
    let result = match method {
        "<init>" | "set" | "lazySet" => {
            set(next);
            Value::Void
        }
        "get" => old,
        "getAndSet" => {
            set(next);
            old
        }
        "compareAndSet" => {
            let same = if kind == 2 {
                old.as_ref() == next.as_ref()
            } else if kind == 1 {
                old.as_long()? == next.as_long()?
            } else {
                old.as_int()? == next.as_int()?
            };
            if same {
                set(args.get(2).cloned().ok_or("missing atomic update value")?);
            }
            Value::Int(same as i32)
        }
        "getAndIncrement" | "incrementAndGet" | "getAndDecrement" | "decrementAndGet"
        | "getAndAdd" | "addAndGet"
            if kind != 2 =>
        {
            let delta = if method.contains("Increment") || method.starts_with("increment") {
                1
            } else if method.contains("Decrement") || method.starts_with("decrement") {
                -1
            } else if kind == 1 {
                next.as_long()?
            } else {
                next.as_int()? as i64
            };
            let updated = if kind == 1 {
                Value::Long(old.as_long()?.wrapping_add(delta))
            } else {
                Value::Int(old.as_int()?.wrapping_add(delta as i32))
            };
            set(updated.clone());
            if method.starts_with("getAnd") {
                old
            } else {
                updated
            }
        }
        _ => return Ok(BuiltinResult::NotBuiltin),
    };
    Ok(BuiltinResult::Handled(result))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_values_obey_cas_return_values_and_java_overflow() {
        let mut heap = Heap::new();
        for (class, initial) in [
            (
                "Ljava/util/concurrent/atomic/AtomicInteger;",
                Value::Int(i32::MAX),
            ),
            (
                "Ljava/util/concurrent/atomic/AtomicLong;",
                Value::Long(i64::MAX),
            ),
        ] {
            let id = heap.new_instance(class);
            let receiver = Value::Ref(Some(id));
            try_invoke(
                &mut heap,
                class,
                "<init>",
                &[receiver.clone(), initial.clone()],
            )
            .unwrap();
            let BuiltinResult::Handled(old) =
                try_invoke(&mut heap, class, "getAndIncrement", &[receiver.clone()]).unwrap()
            else {
                panic!()
            };
            assert_eq!(format!("{old:?}"), format!("{initial:?}"));
            let BuiltinResult::Handled(value) =
                try_invoke(&mut heap, class, "get", &[receiver]).unwrap()
            else {
                panic!()
            };
            assert!(match value {
                Value::Int(v) => v == i32::MIN,
                Value::Long(v) => v == i64::MIN,
                _ => false,
            });
        }
        let class = "Ljava/util/concurrent/atomic/AtomicReference;";
        let id = heap.new_instance(class);
        let receiver = Value::Ref(Some(id));
        let a = Value::Ref(Some(heap.new_string("same".into())));
        let b = Value::Ref(Some(heap.new_string("same".into())));
        try_invoke(&mut heap, class, "<init>", &[receiver.clone(), a.clone()]).unwrap();
        let BuiltinResult::Handled(failed) = try_invoke(
            &mut heap,
            class,
            "compareAndSet",
            &[receiver.clone(), b.clone(), Value::Ref(None)],
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(failed.as_int().unwrap(), 0); // identity, not equal string content
        let BuiltinResult::Handled(ok) =
            try_invoke(&mut heap, class, "compareAndSet", &[receiver, a, b]).unwrap()
        else {
            panic!()
        };
        assert_eq!(ok.as_int().unwrap(), 1);
    }
}
