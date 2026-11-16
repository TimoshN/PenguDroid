//! Host filesystem implementation for the independent runtime.
use crate::stdlib::BuiltinResult;
use crate::{ArrayData, Heap, HeapObject, Value};
use std::{
    collections::HashMap,
    ffi::CString,
    path::{Path, PathBuf},
};

fn text(heap: &Heap, value: Option<&Value>) -> String {
    value
        .and_then(Value::as_ref)
        .and_then(|id| heap.as_str(id))
        .unwrap_or("")
        .into()
}
fn file_path(heap: &Heap, value: Option<&Value>) -> Option<String> {
    let id = value?.as_ref()?;
    match heap.get(id)? {
        HeapObject::Instance { fields, .. } => Some(text(heap, fields.get("path"))),
        _ => None,
    }
}
fn file(heap: &mut Heap, path: &Path) -> Value {
    let path_id = heap.new_string(path.to_string_lossy().into_owned());
    let id = heap.new_instance("Ljava/io/File;");
    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
        fields.insert("path".into(), Value::Ref(Some(path_id)));
    }
    Value::Ref(Some(id))
}

pub fn try_invoke(
    heap: &mut Heap,
    statics: &HashMap<String, Value>,
    class: &str,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    let handled = |v| Ok(BuiltinResult::Handled(v));
    if class == "Landroid/os/Environment;" && method == "getExternalStorageState" {
        return handled(Value::Ref(Some(heap.new_string("mounted".into()))));
    }
    if class == "Landroid/content/Context;"
        && matches!(
            method,
            "getFilesDir"
                | "getCacheDir"
                | "getCodeCacheDir"
                | "getExternalFilesDir"
                | "getExternalCacheDir"
                | "getObbDir"
                | "getObbDirs"
        )
    {
        let data = PathBuf::from(text(heap, statics.get("PENGUDROID_DATA_DIR")));
        if data.as_os_str().is_empty() {
            return Err("no per-application data directory configured".into());
        }
        let prefix = data.parent().ok_or("invalid data directory")?;
        let path = match method {
            "getFilesDir" => data.clone(),
            "getCacheDir" => prefix.join("cache"),
            "getCodeCacheDir" => prefix.join("code_cache"),
            "getExternalCacheDir" => prefix.join("external/cache"),
            "getObbDir" | "getObbDirs" => prefix
                .join("bundle/Android/obb")
                .join(text(heap, statics.get("PENGUDROID_PACKAGE_NAME"))),
            _ => {
                let kind = text(heap, args.get(1));
                if Path::new(&kind).components().any(|c| {
                    !matches!(
                        c,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                }) {
                    return Err("external files type must be relative to the app directory".into());
                }
                prefix.join("external/files").join(kind)
            }
        };
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("cannot create app directory {}: {e}", path.display()))?;
        let result = file(heap, &path);
        if method == "getObbDirs" {
            return handled(Value::Ref(Some(heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/io/File;".into(),
                data: ArrayData::Object(vec![result.as_ref()]),
            }))));
        }
        return handled(result);
    }
    if class != "Ljava/io/File;" {
        return Ok(BuiltinResult::NotBuiltin);
    }
    if method == "<init>" {
        let path = match descriptor {
            "(Ljava/lang/String;)V" => PathBuf::from(text(heap, args.get(1))),
            "(Ljava/lang/String;Ljava/lang/String;)V" => {
                PathBuf::from(text(heap, args.get(1))).join(text(heap, args.get(2)))
            }
            "(Ljava/io/File;Ljava/lang/String;)V" => {
                PathBuf::from(file_path(heap, args.get(1)).unwrap_or_default())
                    .join(text(heap, args.get(2)))
            }
            _ => return Ok(BuiltinResult::NotBuiltin),
        };
        let path = heap.new_string(path.to_string_lossy().into_owned());
        if let Some(HeapObject::Instance { fields, .. }) = args
            .first()
            .and_then(Value::as_ref)
            .and_then(|id| heap.get_mut(id))
        {
            fields.insert("path".into(), Value::Ref(Some(path)));
        }
        return handled(Value::Void);
    }
    let Some(path) = file_path(heap, args.first()) else {
        return Ok(BuiltinResult::NotBuiltin);
    };
    let path = PathBuf::from(path);
    match method {
        "exists" => handled(Value::Int(path.exists() as i32)),
        "isDirectory" => handled(Value::Int(path.is_dir() as i32)),
        "isFile" => handled(Value::Int(path.is_file() as i32)),
        "mkdirs" => handled(Value::Int(
            (!path.exists() && std::fs::create_dir_all(&path).is_ok()) as i32,
        )),
        "length" => handled(Value::Long(
            std::fs::metadata(&path)
                .map(|m| m.len() as i64)
                .unwrap_or(0),
        )),
        "getAbsolutePath" => {
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(path)
            };
            handled(Value::Ref(Some(
                heap.new_string(path.to_string_lossy().into_owned()),
            )))
        }
        "getParentFile" => handled(
            path.parent()
                .map(|p| file(heap, p))
                .unwrap_or(Value::Ref(None)),
        ),
        "getName" => handled(Value::Ref(Some(
            heap.new_string(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        ))),
        "getUsableSpace" | "getFreeSpace" | "getTotalSpace" => {
            let cpath = CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|_| "file path contains NUL")?;
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
                return handled(Value::Long(0));
            }
            let blocks = match method {
                "getUsableSpace" => stat.f_bavail,
                "getFreeSpace" => stat.f_bfree,
                _ => stat.f_blocks,
            };
            handled(Value::Long(
                (blocks as u64)
                    .saturating_mul(stat.f_frsize as u64)
                    .min(i64::MAX as u64) as i64,
            ))
        }
        _ => Ok(BuiltinResult::NotBuiltin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_directories_obb_arrays_and_real_storage_capacity() {
        let root = std::env::temp_dir().join(format!("pengu-fs-probe-{}", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let mut heap = Heap::new();
        let data = heap.new_string(root.join("files").to_string_lossy().into_owned());
        let package = heap.new_string("org.pengudroid.fsprobe".into());
        let statics = HashMap::from([
            ("PENGUDROID_DATA_DIR".into(), Value::Ref(Some(data))),
            ("PENGUDROID_PACKAGE_NAME".into(), Value::Ref(Some(package))),
        ]);
        let get = |heap: &mut Heap, method| match try_invoke(
            heap,
            &statics,
            "Landroid/content/Context;",
            method,
            "",
            &[],
        )
        .unwrap()
        {
            BuiltinResult::Handled(value) => value,
            _ => panic!("directory API unhandled"),
        };
        let files = get(&mut heap, "getFilesDir");
        let cache = get(&mut heap, "getCacheDir");
        assert_eq!(
            file_path(&heap, Some(&files)),
            Some(root.join("files").to_string_lossy().into_owned())
        );
        assert_eq!(
            file_path(&heap, Some(&cache)),
            Some(root.join("cache").to_string_lossy().into_owned())
        );
        let obb = get(&mut heap, "getObbDir");
        let obbs = get(&mut heap, "getObbDirs");
        let HeapObject::Array {
            data: ArrayData::Object(array),
            ..
        } = heap.get(obbs.as_ref().unwrap()).unwrap()
        else {
            panic!("OBB list must be a File array")
        };
        assert_eq!(array.len(), 1);
        assert_eq!(
            file_path(&heap, Some(&obb)),
            file_path(&heap, Some(&Value::Ref(array[0])))
        );
        for value in [files, cache, obb] {
            let path = file_path(&heap, Some(&value)).unwrap();
            assert!(Path::new(&path).is_dir());
            let BuiltinResult::Handled(space) = try_invoke(
                &mut heap,
                &statics,
                "Ljava/io/File;",
                "getTotalSpace",
                "()J",
                &[value],
            )
            .unwrap() else {
                panic!("storage query unhandled")
            };
            assert!(space.as_long().unwrap() > 0);
        }
    }
}
