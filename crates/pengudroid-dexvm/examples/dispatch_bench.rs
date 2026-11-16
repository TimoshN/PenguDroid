//! Focused dispatch benchmark; not a game/FPS benchmark.
use pengudroid_dexvm::dex::{ClassDef, CodeItem, EncodedMethod, MethodId, ProtoId};
use pengudroid_dexvm::{DexFile, Value, Vm};
use std::{collections::HashMap, hint::black_box, sync::Arc, time::Instant};
fn fixture(d: usize) -> DexFile {
    let name = format!("LBench{d};");
    let methods: Vec<_> = (0..1024)
        .map(|i| MethodId {
            class_name: name.clone(),
            proto_idx: 0,
            name: format!("method{i}"),
            descriptor: "()I".into(),
        })
        .collect();
    let code = Arc::new(CodeItem {
        registers_size: 1,
        ins_size: 0,
        outs_size: 0,
        insns: vec![0x1012, 0x000f],
        tries: vec![],
    }); // const/4 v0,1; return v0
    let class = ClassDef {
        name: name.clone(),
        access_flags: 1,
        superclass: None,
        interfaces: vec![],
        static_fields: vec![],
        instance_fields: vec![],
        direct_methods: (0..methods.len())
            .map(|method_idx| EncodedMethod {
                method_idx,
                access_flags: 9,
                code: Some(Arc::clone(&code)),
            })
            .collect(),
        virtual_methods: vec![],
    };
    DexFile {
        strings: vec![],
        types: vec![name.clone()],
        protos: vec![ProtoId {
            shorty: "I".into(),
            return_type: "I".into(),
            params: vec![],
        }],
        fields: vec![],
        methods,
        classes: vec![class],
        class_by_name: HashMap::from([(name, 0)]),
    }
}
fn main() {
    let mut vm = Vm::new();
    for d in 0..16 {
        vm.load_parsed_dex(fixture(d)).unwrap();
    }
    let repetitions = 100_000;
    let start = Instant::now();
    for _ in 0..repetitions {
        let result = vm
            .invoke_method(
                black_box("LBench15;"),
                black_box("method1023"),
                "()I",
                &[],
                false,
            )
            .unwrap();
        assert_eq!(result.as_int().unwrap(), 1);
        black_box(result);
    }
    println!(
        "calls={repetitions} elapsed_ns={} ns_per_call={:.1}",
        start.elapsed().as_nanos(),
        start.elapsed().as_nanos() as f64 / repetitions as f64
    );
    black_box(Value::Void);
}
