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
    if class == "Landroid/content/res/AssetManager;" {
        return handle_asset_manager(heap, statics, method, args);
    }
    if matches!(
        class,
        "Ljava/io/InputStream;"
            | "Ljava/io/ByteArrayInputStream;"
            | "Landroid/content/res/AssetManager$AssetInputStream;"
            | "Ljava/io/InputStreamReader;"
            | "Ljava/io/BufferedReader;"
            | "Ljava/io/StringReader;"
            | "Ljava/io/Reader;"
            | "Ljava/io/StringWriter;"
            | "Ljava/io/Writer;"
    ) {
        return handle_streams(heap, class, method, descriptor, args);
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
        "getAbsoluteFile" => {
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(path)
            };
            handled(file(heap, &path))
        }
        "createNewFile" => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .is_ok();
            handled(Value::Int(i32::from(created)))
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

pub(crate) fn apk_paths(heap: &Heap, statics: &HashMap<String, Value>) -> Vec<String> {
    let mut paths = Vec::new();
    let main = text(heap, statics.get("PENGUDROID_APK_PATH"));
    if !main.is_empty() {
        paths.push(main);
    }
    if let Some(arr_id) = statics.get("PENGUDROID_SPLIT_APKS").and_then(Value::as_ref) {
        if let Some(HeapObject::Array {
            data: ArrayData::Object(items),
            ..
        }) = heap.get(arr_id)
        {
            for item in items.iter().flatten() {
                if let Some(s) = heap.as_str(*item) {
                    if !s.is_empty() && !paths.iter().any(|p| p == s) {
                        paths.push(s.to_string());
                    }
                }
            }
        }
    }
    paths
}

fn le16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

pub(crate) fn list_zip_dir(apk_path: &str, prefix: &str, out: &mut std::collections::BTreeSet<String>) {
    let Ok(data) = std::fs::read(apk_path) else { return; };
    let min_eocd = data.len().saturating_sub(65_557);
    let Some(eocd_off) = (min_eocd..=data.len().saturating_sub(22))
        .rev()
        .find(|&i| data.get(i..i + 4) == Some(b"PK\x05\x06"))
    else { return; };
    let Some(cd_size) = le32(&data, eocd_off + 12).map(|v| v as usize) else { return; };
    let Some(cd_off) = le32(&data, eocd_off + 16).map(|v| v as usize) else { return; };
    let Some(cd_end) = cd_off.checked_add(cd_size).map(|v| v.min(data.len())) else { return; };
    let mut pos = cd_off;
    while pos + 46 <= cd_end {
        if data.get(pos..pos + 4) != Some(b"PK\x01\x02") { break; }
        let Some(name_len) = le16(&data, pos + 28).map(|v| v as usize) else { break; };
        let Some(extra_len) = le16(&data, pos + 30).map(|v| v as usize) else { break; };
        let Some(comment_len) = le16(&data, pos + 32).map(|v| v as usize) else { break; };
        let Some(name_bytes) = data.get(pos + 46..pos + 46 + name_len) else { break; };
        pos = pos + 46 + name_len + extra_len + comment_len;
        if let Ok(name) = std::str::from_utf8(name_bytes) {
            if let Some(rest) = name.strip_prefix(prefix) {
                let first = rest.split('/').next().unwrap_or(rest);
                if !first.is_empty() {
                    out.insert(first.to_string());
                }
            }
        }
    }
}

pub(crate) fn read_zip_entry(apk_path: &str, entry_name: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(apk_path).ok()?;
    let min_eocd = data.len().saturating_sub(65_557);
    let eocd_off = (min_eocd..=data.len().saturating_sub(22))
        .rev()
        .find(|&i| data.get(i..i + 4) == Some(b"PK\x05\x06"))?;
    let cd_size = le32(&data, eocd_off + 12)? as usize;
    let cd_off = le32(&data, eocd_off + 16)? as usize;
    let cd_end = cd_off.checked_add(cd_size)?.min(data.len());
    let mut pos = cd_off;
    while pos + 46 <= cd_end {
        if data.get(pos..pos + 4) != Some(b"PK\x01\x02") {
            break;
        }
        let method = le16(&data, pos + 10)?;
        let comp_size = le32(&data, pos + 20)? as usize;
        let name_len = le16(&data, pos + 28)? as usize;
        let extra_len = le16(&data, pos + 30)? as usize;
        let comment_len = le16(&data, pos + 32)? as usize;
        let lfh_off = le32(&data, pos + 42)? as usize;
        let name_bytes = data.get(pos + 46..pos + 46 + name_len)?;
        pos = pos + 46 + name_len + extra_len + comment_len;
        if name_bytes == entry_name.as_bytes() {
            if data.get(lfh_off..lfh_off + 4) != Some(b"PK\x03\x04") {
                return None;
            }
            let lfh_name_len = le16(&data, lfh_off + 26)? as usize;
            let lfh_extra_len = le16(&data, lfh_off + 28)? as usize;
            let start = lfh_off + 30 + lfh_name_len + lfh_extra_len;
            let payload = data.get(start..start.checked_add(comp_size)?)?;
            return match method {
                0 => Some(payload.to_vec()),
                8 => miniz_oxide::inflate::decompress_to_vec(payload).ok(),
                _ => None,
            };
        }
    }
    None
}

fn handle_asset_manager(
    heap: &mut Heap,
    statics: &HashMap<String, Value>,
    method: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    match method {
        "open" => {
            let rel = text(heap, args.get(1));
            let trimmed = rel.trim_start_matches('/');
            let entry = format!("assets/{trimmed}");
            let mut found = None;
            for apk in apk_paths(heap, statics) {
                if let Some(bytes) = read_zip_entry(&apk, &entry) {
                    found = Some(bytes);
                    break;
                }
            }
            if std::env::var_os("PENGUDROID_TRACE_ASSETS").is_some() {
                eprintln!("[dexvm] AssetManager.open({rel}) -> {}", found.as_ref().map(|b| b.len()).unwrap_or(0));
            }
            let Some(bytes) = found else {
                return Err(format!("java.io.FileNotFoundException: {rel}"));
            };
            let arr_id = heap.alloc(HeapObject::Array {
                type_desc: "[B".into(),
                data: ArrayData::Byte(bytes.into_iter().map(|b| b as i8).collect()),
            });
            let stream_id = heap.new_instance("Ljava/io/ByteArrayInputStream;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(stream_id) {
                fields.insert("data".into(), Value::Ref(Some(arr_id)));
                fields.insert("pos".into(), Value::Int(0));
            }
            Ok(BuiltinResult::Handled(Value::Ref(Some(stream_id))))
        }
        "list" => {
            let rel = text(heap, args.get(1));
            let trimmed = rel.trim_matches('/');
            let prefix = if trimmed.is_empty() {
                "assets/".to_string()
            } else {
                format!("assets/{trimmed}/")
            };
            let mut entries = std::collections::BTreeSet::new();
            for apk in apk_paths(heap, statics) {
                list_zip_dir(&apk, &prefix, &mut entries);
            }
            if std::env::var_os("PENGUDROID_TRACE_ASSETS").is_some() {
                eprintln!("[dexvm] AssetManager.list({rel}) -> {} entries", entries.len());
            }
            let vals: Vec<Option<usize>> = entries.into_iter().map(|s| Some(heap.new_string(s))).collect();
            let arr = heap.alloc(HeapObject::Array {
                type_desc: "[Ljava/lang/String;".into(),
                data: ArrayData::Object(vals),
            });
            Ok(BuiltinResult::Handled(Value::Ref(Some(arr))))
        }
        _ => Ok(BuiltinResult::NotBuiltin),
    }
}

fn stream_bytes_and_pos(heap: &Heap, stream_id: usize) -> Option<(usize, usize)> {
    match heap.get(stream_id) {
        Some(HeapObject::Instance { fields, .. }) => {
            let arr_id = fields.get("data").and_then(|v| v.as_ref())?;
            let pos = fields
                .get("pos")
                .and_then(|v| v.as_int().ok())
                .unwrap_or(0)
                .max(0) as usize;
            Some((arr_id, pos))
        }
        _ => None,
    }
}

fn set_stream_pos(heap: &mut Heap, stream_id: usize, pos: usize) {
    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(stream_id) {
        fields.insert("pos".into(), Value::Int(pos as i32));
    }
}

fn handle_streams(
    heap: &mut Heap,
    class: &str,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    if class == "Ljava/io/InputStreamReader;" {
        return match method {
            "<init>" => {
                if let Some(id) = args.first().and_then(Value::as_ref) {
                    let stream = args.get(1).cloned().unwrap_or(Value::Ref(None));
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("stream".into(), stream);
                    }
                }
                Ok(BuiltinResult::Handled(Value::Void))
            }
            "close" => Ok(BuiltinResult::Handled(Value::Void)),
            _ => Ok(BuiltinResult::NotBuiltin),
        };
    }
    if class == "Ljava/io/StringReader;" {
        let Some(id) = args.first().and_then(Value::as_ref) else {
            return Ok(BuiltinResult::NotBuiltin);
        };
        return match method {
            "<init>" => {
                let s = args.get(1).cloned().unwrap_or(Value::Ref(None));
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("str".into(), s);
                    fields.insert("pos".into(), Value::Int(0));
                }
                Ok(BuiltinResult::Handled(Value::Void))
            }
            "close" => Ok(BuiltinResult::Handled(Value::Void)),
            "read" => {
                let (s_opt, pos) = match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => (
                        fields
                            .get("str")
                            .and_then(|v| v.as_ref())
                            .and_then(|sid| heap.as_str(sid))
                            .map(str::to_string),
                        fields
                            .get("pos")
                            .and_then(|v| v.as_int().ok())
                            .unwrap_or(0)
                            .max(0) as usize,
                    ),
                    _ => (None, 0),
                };
                let Some(s) = s_opt else {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                };
                let chars: Vec<u16> = s.encode_utf16().collect();
                if pos >= chars.len() {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                }
                if descriptor == "()I" {
                    let ch = i32::from(chars[pos]);
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("pos".into(), Value::Int((pos + 1) as i32));
                    }
                    return Ok(BuiltinResult::Handled(Value::Int(ch)));
                }
                let Some(dst_id) = args.get(1).and_then(Value::as_ref) else {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                };
                let (off, req_len) = if descriptor == "([CII)I" {
                    (
                        args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                        args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    )
                } else {
                    (0, usize::MAX)
                };
                let copied = match heap.get_mut(dst_id) {
                    Some(HeapObject::Array { data: ArrayData::Char(dst), .. }) => {
                        let avail = dst.len().saturating_sub(off);
                        let n = avail.min(req_len).min(chars.len() - pos);
                        dst[off..off + n].copy_from_slice(&chars[pos..pos + n]);
                        n
                    }
                    _ => 0,
                };
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                    fields.insert("pos".into(), Value::Int((pos + copied) as i32));
                }
                Ok(BuiltinResult::Handled(Value::Int(if copied == 0 { -1 } else { copied as i32 })))
            }
            _ => Ok(BuiltinResult::NotBuiltin),
        };
    }
    if class == "Ljava/io/StringWriter;" || class == "Ljava/io/Writer;" {
        return match method {
            "<init>" => {
                if let Some(id) = args.first().and_then(Value::as_ref) {
                    let s = heap.new_string(String::new());
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("buf".into(), Value::Ref(Some(s)));
                    }
                }
                Ok(BuiltinResult::Handled(Value::Void))
            }
            "write" => {
                let added = match descriptor {
                    "([CII)V" => {
                        let off = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                        let len = args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize;
                        args.get(1)
                            .and_then(Value::as_ref)
                            .and_then(|aid| match heap.get(aid) {
                                Some(HeapObject::Array { data: ArrayData::Char(chars), .. }) => {
                                    let end = (off + len).min(chars.len());
                                    Some(chars[off..end].iter().filter_map(|&c| char::from_u32(c as u32)).collect::<String>())
                                }
                                _ => None,
                            })
                            .unwrap_or_default()
                    }
                    "(Ljava/lang/String;)V" => text(heap, args.get(1)),
                    _ => String::new(),
                };
                if let Some(id) = args.first().and_then(Value::as_ref) {
                    let cur = match heap.get(id) {
                        Some(HeapObject::Instance { fields, .. }) => text(heap, fields.get("buf")),
                        _ => String::new(),
                    };
                    let next = heap.new_string(format!("{cur}{added}"));
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("buf".into(), Value::Ref(Some(next)));
                    }
                }
                Ok(BuiltinResult::Handled(Value::Void))
            }
            "toString" => {
                let s = args
                    .first()
                    .and_then(Value::as_ref)
                    .and_then(|id| match heap.get(id) {
                        Some(HeapObject::Instance { fields, .. }) => Some(text(heap, fields.get("buf"))),
                        _ => None,
                    })
                    .unwrap_or_default();
                Ok(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(s)))))
            }
            "close" | "flush" => Ok(BuiltinResult::Handled(Value::Void)),
            _ => Ok(BuiltinResult::NotBuiltin),
        };
    }
    if class == "Ljava/io/BufferedReader;" || class == "Ljava/io/Reader;" {
        return match method {
            "<init>" => {
                if let Some(id) = args.first().and_then(Value::as_ref) {
                    let reader = args.get(1).cloned().unwrap_or(Value::Ref(None));
                    if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                        fields.insert("reader".into(), reader);
                    }
                }
                Ok(BuiltinResult::Handled(Value::Void))
            }
            "close" => Ok(BuiltinResult::Handled(Value::Void)),
            "read" => {
                let reader_id = args
                    .first()
                    .and_then(Value::as_ref)
                    .and_then(|id| match heap.get(id) {
                        Some(HeapObject::Instance { fields, .. }) => {
                            fields.get("reader").and_then(|v| v.as_ref())
                        }
                        _ => None,
                    });
                let stream_id = reader_id.and_then(|rid| match heap.get(rid) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        fields.get("stream").and_then(|v| v.as_ref())
                    }
                    _ => None,
                });
                let Some(sid) = stream_id else {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                };
                let Some((arr_id, pos)) = stream_bytes_and_pos(heap, sid) else {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                };
                let (chars_vec, next_pos): (Vec<u16>, usize) = match heap.get(arr_id) {
                    Some(HeapObject::Array { data: ArrayData::Byte(bytes), .. }) if pos < bytes.len() => {
                        let raw: Vec<u8> = bytes[pos..].iter().map(|&b| b as u8).collect();
                        let s = String::from_utf8_lossy(&raw);
                        (s.encode_utf16().collect(), bytes.len())
                    }
                    _ => return Ok(BuiltinResult::Handled(Value::Int(-1))),
                };
                if descriptor == "()I" {
                    if let Some(&ch) = chars_vec.first() {
                        set_stream_pos(heap, sid, pos + 1);
                        return Ok(BuiltinResult::Handled(Value::Int(i32::from(ch))));
                    }
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                }
                let Some(dst_id) = args.get(1).and_then(Value::as_ref) else {
                    return Ok(BuiltinResult::Handled(Value::Int(-1)));
                };
                let (off, req_len) = if descriptor == "([CII)I" {
                    (
                        args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                        args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    )
                } else {
                    (0, usize::MAX)
                };
                let copied = match heap.get_mut(dst_id) {
                    Some(HeapObject::Array { data: ArrayData::Char(dst), .. }) => {
                        let avail = dst.len().saturating_sub(off);
                        let n = avail.min(req_len).min(chars_vec.len());
                        dst[off..off + n].copy_from_slice(&chars_vec[..n]);
                        n
                    }
                    _ => 0,
                };
                if copied == chars_vec.len() {
                    set_stream_pos(heap, sid, next_pos);
                } else {
                    let prefix_bytes = String::from_utf16_lossy(&chars_vec[..copied]).len();
                    set_stream_pos(heap, sid, pos + prefix_bytes);
                }
                Ok(BuiltinResult::Handled(Value::Int(if copied == 0 { -1 } else { copied as i32 })))
            }
            "readLine" => {
                let reader_id = args
                    .first()
                    .and_then(Value::as_ref)
                    .and_then(|id| match heap.get(id) {
                        Some(HeapObject::Instance { fields, .. }) => {
                            fields.get("reader").and_then(|v| v.as_ref())
                        }
                        _ => None,
                    });
                let stream_id = reader_id.and_then(|rid| match heap.get(rid) {
                    Some(HeapObject::Instance { fields, .. }) => {
                        fields.get("stream").and_then(|v| v.as_ref())
                    }
                    _ => None,
                });
                let Some(sid) = stream_id else {
                    return Ok(BuiltinResult::Handled(Value::Ref(None)));
                };
                let Some((arr_id, pos)) = stream_bytes_and_pos(heap, sid) else {
                    return Ok(BuiltinResult::Handled(Value::Ref(None)));
                };
                let (line_opt, next_pos) = match heap.get(arr_id) {
                    Some(HeapObject::Array {
                        data: ArrayData::Byte(bytes),
                        ..
                    }) => {
                        if pos >= bytes.len() {
                            (None, pos)
                        } else {
                            let mut end = pos;
                            while end < bytes.len() && bytes[end] as u8 != b'\n' {
                                end += 1;
                            }
                            let slice_end =
                                if end > pos && bytes[end - 1] as u8 == b'\r' {
                                    end - 1
                                } else {
                                    end
                                };
                            let raw: Vec<u8> = bytes[pos..slice_end]
                                .iter()
                                .map(|&b| b as u8)
                                .collect();
                            let next = if end < bytes.len() { end + 1 } else { end };
                            (Some(String::from_utf8_lossy(&raw).into_owned()), next)
                        }
                    }
                    _ => (None, pos),
                };
                set_stream_pos(heap, sid, next_pos);
                let out = line_opt.map(|s| heap.new_string(s));
                Ok(BuiltinResult::Handled(Value::Ref(out)))
            }
            _ => Ok(BuiltinResult::NotBuiltin),
        };
    }

    // InputStream / ByteArrayInputStream
    let Some(sid) = args.first().and_then(Value::as_ref) else {
        return Ok(BuiltinResult::NotBuiltin);
    };
    match method {
        "<init>" => {
            if let Some(arr) = args.get(1).cloned() {
                let offset = args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(sid) {
                    fields.insert("data".into(), arr);
                    fields.insert("pos".into(), Value::Int(offset));
                }
            }
            Ok(BuiltinResult::Handled(Value::Void))
        }
        "available" => {
            let avail = stream_bytes_and_pos(heap, sid)
                .and_then(|(arr_id, pos)| match heap.get(arr_id) {
                    Some(HeapObject::Array {
                        data: ArrayData::Byte(bytes),
                        ..
                    }) => Some(bytes.len().saturating_sub(pos) as i32),
                    _ => None,
                })
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(avail)))
        }
        "read" if descriptor == "()I" => {
            let Some((arr_id, pos)) = stream_bytes_and_pos(heap, sid) else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let byte = match heap.get(arr_id) {
                Some(HeapObject::Array {
                    data: ArrayData::Byte(bytes),
                    ..
                }) => bytes.get(pos).map(|&b| i32::from(b as u8)),
                _ => None,
            };
            if let Some(b) = byte {
                set_stream_pos(heap, sid, pos + 1);
                Ok(BuiltinResult::Handled(Value::Int(b)))
            } else {
                Ok(BuiltinResult::Handled(Value::Int(-1)))
            }
        }
        "read" => {
            let Some(dst_id) = args.get(1).and_then(Value::as_ref) else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let Some((arr_id, pos)) = stream_bytes_and_pos(heap, sid) else {
                return Ok(BuiltinResult::Handled(Value::Int(-1)));
            };
            let src_slice: Vec<i8> = match heap.get(arr_id) {
                Some(HeapObject::Array {
                    data: ArrayData::Byte(bytes),
                    ..
                }) if pos < bytes.len() => bytes[pos..].to_vec(),
                _ => return Ok(BuiltinResult::Handled(Value::Int(-1))),
            };
            let (off, req_len) = if descriptor == "([BII)I" {
                (
                    args.get(2).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                    args.get(3).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                )
            } else {
                (0, usize::MAX)
            };
            let copied = match heap.get_mut(dst_id) {
                Some(HeapObject::Array {
                    data: ArrayData::Byte(dst),
                    ..
                }) => {
                    let avail_dst = dst.len().saturating_sub(off);
                    let n = avail_dst.min(req_len).min(src_slice.len());
                    dst[off..off + n].copy_from_slice(&src_slice[..n]);
                    n
                }
                _ => 0,
            };
            set_stream_pos(heap, sid, pos + copied);
            Ok(BuiltinResult::Handled(Value::Int(if copied == 0 && req_len > 0 {
                -1
            } else {
                copied as i32
            })))
        }
        "skip" => {
            let n = args.get(1).and_then(|v| v.as_long().ok()).unwrap_or(0).max(0) as usize;
            let Some((arr_id, pos)) = stream_bytes_and_pos(heap, sid) else {
                return Ok(BuiltinResult::Handled(Value::Long(0)));
            };
            let len = match heap.get(arr_id) {
                Some(HeapObject::Array {
                    data: ArrayData::Byte(bytes),
                    ..
                }) => bytes.len(),
                _ => 0,
            };
            let skipped = n.min(len.saturating_sub(pos));
            set_stream_pos(heap, sid, pos + skipped);
            Ok(BuiltinResult::Handled(Value::Long(skipped as i64)))
        }
        "close" => Ok(BuiltinResult::Handled(Value::Void)),
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
