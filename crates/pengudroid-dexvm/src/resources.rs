//! Minimal `resources.arsc` reader for `android.content.res.Resources` in DexVM.
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use crate::{stdlib::BuiltinResult, Heap, Value};

#[derive(Default)]
struct ArscTable {
    /// `(type_name, entry_name) -> res_id` (and `pkg:type/name -> res_id`).
    by_name: HashMap<String, u32>,
    /// `res_id -> string` for string/reference entries.
    strings_by_id: HashMap<u32, String>,
    /// `res_id -> int` for int/bool entries.
    ints_by_id: HashMap<u32, i32>,
}

static CACHE: OnceLock<Mutex<Option<ArscTable>>> = OnceLock::new();

fn le16(d: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(off..off + 2)?.try_into().ok()?))
}

fn le32(d: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(off..off + 4)?.try_into().ok()?))
}

fn parse_string_pool(d: &[u8], chunk: usize) -> Vec<String> {
    let Some(header) = le16(d, chunk + 2).map(usize::from) else {
        return Vec::new();
    };
    let Some(count) = le32(d, chunk + 8).map(|v| v as usize) else {
        return Vec::new();
    };
    let Some(flags) = le32(d, chunk + 16) else {
        return Vec::new();
    };
    let Some(strings_start) = le32(d, chunk + 20).map(|v| chunk + v as usize) else {
        return Vec::new();
    };
    let is_utf8 = (flags & (1 << 8)) != 0;
    let offsets_base = chunk + header;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let Some(rel) = le32(d, offsets_base + i * 4).map(|v| v as usize) else {
            out.push(String::new());
            continue;
        };
        let mut p = strings_start + rel;
        let s = if is_utf8 {
            let mut skip_len = || {
                let b = *d.get(p)?;
                p += 1;
                if b & 0x80 != 0 {
                    p += 1;
                }
                Some(())
            };
            let _ = skip_len();
            let blen = (|| -> Option<usize> {
                let b0 = *d.get(p)?;
                p += 1;
                if b0 & 0x80 != 0 {
                    let b1 = *d.get(p)?;
                    p += 1;
                    Some((((b0 & 0x7f) as usize) << 8) | (b1 as usize))
                } else {
                    Some(b0 as usize)
                }
            })()
            .unwrap_or(0);
            d.get(p..p + blen)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default()
        } else {
            let u16_len = (|| -> Option<usize> {
                let w0 = le16(d, p)? as usize;
                p += 2;
                if w0 & 0x8000 != 0 {
                    let w1 = le16(d, p)? as usize;
                    p += 2;
                    Some(((w0 & 0x7fff) << 16) | w1)
                } else {
                    Some(w0)
                }
            })()
            .unwrap_or(0);
            let units: Vec<u16> = (0..u16_len)
                .filter_map(|j| le16(d, p + j * 2))
                .collect();
            String::from_utf16_lossy(&units)
        };
        out.push(s);
    }
    out
}

fn parse_arsc(d: &[u8]) -> Option<ArscTable> {
    if le16(d, 0)? != 0x0002 {
        return None;
    }
    let header = le16(d, 2)? as usize;
    let mut global_strings = Vec::new();
    let mut table = ArscTable::default();
    let mut raw_refs: Vec<(u32, u32)> = Vec::new();
    let mut offset = header;
    while offset + 8 <= d.len() {
        let kind = le16(d, offset)?;
        let size = le32(d, offset + 4)? as usize;
        if size < 8 || offset + size > d.len() {
            break;
        }
        if kind == 0x0001 && global_strings.is_empty() {
            global_strings = parse_string_pool(d, offset);
        } else if kind == 0x0200 {
            let pkg_id = le32(d, offset + 8)? as u8;
            let pkg_name_units: Vec<u16> = (0..128)
                .filter_map(|i| le16(d, offset + 12 + i * 2))
                .take_while(|&u| u != 0)
                .collect();
            let pkg_name = String::from_utf16_lossy(&pkg_name_units);
            let type_pool_off = le32(d, offset + 268).unwrap_or(0) as usize;
            let key_pool_off = le32(d, offset + 276).unwrap_or(0) as usize;
            let type_strings = if type_pool_off != 0 {
                parse_string_pool(d, offset + type_pool_off)
            } else {
                Vec::new()
            };
            let key_strings = if key_pool_off != 0 {
                parse_string_pool(d, offset + key_pool_off)
            } else {
                Vec::new()
            };

            let mut sub = offset + le16(d, offset + 2)? as usize;
            let pkg_end = offset + size;
            while sub + 8 <= pkg_end {
                let ck = le16(d, sub)?;
                let cs = le32(d, sub + 4)? as usize;
                if cs < 8 || sub + cs > pkg_end {
                    break;
                }
                if ck == 0x0201 {
                    let type_id = *d.get(sub + 8)?;
                    let flags = *d.get(sub + 9)?;
                    let count = le32(d, sub + 12)? as usize;
                    let entries_base = sub + le32(d, sub + 16)? as usize;
                    let offsets_base = sub + le16(d, sub + 2)? as usize;
                    let type_name = type_id
                        .checked_sub(1)
                        .and_then(|i| type_strings.get(i as usize))
                        .map(String::as_str)
                        .unwrap_or("");
                    for idx in 0..count {
                        let (entry_index, rel_off) = if flags & 0x01 != 0 {
                            let at = offsets_base + idx * 4;
                            let Some(ei) = le16(d, at).map(usize::from) else {
                                continue;
                            };
                            let Some(ro) = le16(d, at + 2).map(|o| o as usize * 4) else {
                                continue;
                            };
                            (ei, ro)
                        } else if flags & 0x02 != 0 {
                            let Some(raw) = le16(d, offsets_base + idx * 2) else {
                                continue;
                            };
                            if raw == 0xffff {
                                continue;
                            }
                            (idx, raw as usize * 4)
                        } else {
                            let Some(raw) = le32(d, offsets_base + idx * 4) else {
                                continue;
                            };
                            if raw == 0xffff_ffff {
                                continue;
                            }
                            (idx, raw as usize)
                        };
                        let e_off = entries_base + rel_off;
                        let Some(word0) = le16(d, e_off) else {
                            continue;
                        };
                        let Some(e_flags) = le16(d, e_off + 2) else {
                            continue;
                        };
                        let is_compact = (e_flags & 0x0008) != 0;
                        let key_idx = if is_compact {
                            usize::from(word0)
                        } else {
                            let Some(k) = le32(d, e_off + 4).map(|v| v as usize) else {
                                continue;
                            };
                            k
                        };
                        let res_id = (u32::from(pkg_id) << 24)
                            | (u32::from(type_id) << 16)
                            | (entry_index as u32 & 0xffff);
                        if let Some(key_name) = key_strings.get(key_idx) {
                            if !type_name.is_empty() {
                                table
                                    .by_name
                                    .entry(format!("{pkg_name}:{type_name}/{key_name}"))
                                    .or_insert(res_id);
                                table
                                    .by_name
                                    .entry(format!("{type_name}/{key_name}"))
                                    .or_insert(res_id);
                            }
                        }
                        let val_pair = if is_compact {
                            le32(d, e_off + 4).map(|vd| ((e_flags >> 8) as u8, vd))
                        } else if e_flags & 0x0001 == 0 {
                            let v_off = e_off + usize::from(word0);
                            d.get(v_off + 3).copied().zip(le32(d, v_off + 4))
                        } else {
                            None
                        };
                        if let Some((v_type, v_data)) = val_pair {
                            match v_type {
                                0x01 => raw_refs.push((res_id, v_data)),
                                0x03 => {
                                    if let Some(s) = global_strings.get(v_data as usize) {
                                        table.strings_by_id.entry(res_id).or_insert_with(|| s.clone());
                                    }
                                }
                                0x10 | 0x11 | 0x12 | 0x1c..=0x1f => {
                                    table.ints_by_id.entry(res_id).or_insert(v_data as i32);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                sub += cs;
            }
        }
        offset += size;
    }
    for (id, target) in raw_refs {
        if !table.strings_by_id.contains_key(&id) {
            if let Some(s) = table.strings_by_id.get(&target).cloned() {
                table.strings_by_id.insert(id, s);
            }
        }
        if !table.ints_by_id.contains_key(&id) {
            if let Some(v) = table.ints_by_id.get(&target).copied() {
                table.ints_by_id.insert(id, v);
            }
        }
    }
    Some(table)
}

fn with_table<R>(
    heap: &Heap,
    statics: &HashMap<String, Value>,
    f: impl FnOnce(&ArscTable) -> R,
) -> Option<R> {
    let lock = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap();
    if guard.is_none() {
        let mut combined = ArscTable::default();
        for apk in crate::stdlib_fs::apk_paths(heap, statics) {
            if let Some(bytes) = crate::stdlib_fs::read_zip_entry(&apk, "resources.arsc") {
                if let Some(t) = parse_arsc(&bytes) {
                    for (k, v) in t.by_name {
                        combined.by_name.entry(k).or_insert(v);
                    }
                    for (k, v) in t.strings_by_id {
                        combined.strings_by_id.entry(k).or_insert(v);
                    }
                    for (k, v) in t.ints_by_id {
                        combined.ints_by_id.entry(k).or_insert(v);
                    }
                }
            }
        }
        *guard = Some(combined);
    }
    guard.as_ref().map(f)
}

fn arg_str(heap: &Heap, v: Option<&Value>) -> String {
    v.and_then(Value::as_ref)
        .and_then(|id| heap.as_str(id))
        .unwrap_or("")
        .to_string()
}

pub fn try_invoke(
    heap: &mut Heap,
    statics: &HashMap<String, Value>,
    class: &str,
    method: &str,
    args: &[Value],
) -> Result<BuiltinResult, String> {
    if class != "Landroid/content/res/Resources;" {
        return Ok(BuiltinResult::NotBuiltin);
    }
    match method {
        "getIdentifier" => {
            let name = arg_str(heap, args.get(1));
            let def_type = arg_str(heap, args.get(2));
            let def_pkg = arg_str(heap, args.get(3));
            let key = if name.contains('/') {
                name
            } else if !def_pkg.is_empty() && !def_type.is_empty() {
                format!("{def_pkg}:{def_type}/{name}")
            } else if !def_type.is_empty() {
                format!("{def_type}/{name}")
            } else {
                name
            };
            let id = with_table(heap, statics, |t| t.by_name.get(&key).copied())
                .flatten()
                .unwrap_or(0);
            if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
                eprintln!("[pengu-vm] Resources.getIdentifier({key:?}) -> {id:#x}");
            }
            Ok(BuiltinResult::Handled(Value::Int(id as i32)))
        }
        "getString" | "getText" => {
            let id = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
            let s = with_table(heap, statics, |t| t.strings_by_id.get(&id).cloned()).flatten();
            if std::env::var_os("PENGUDROID_TRACE_VM").is_some() {
                eprintln!("[pengu-vm] Resources.getString({id:#x}) -> {s:?}");
            }
            let val = s.map(|txt| heap.new_string(txt));
            Ok(BuiltinResult::Handled(Value::Ref(val)))
        }
        "getInteger" => {
            let id = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
            let v = with_table(heap, statics, |t| t.ints_by_id.get(&id).copied())
                .flatten()
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(v)))
        }
        "getBoolean" => {
            let id = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0) as u32;
            let v = with_table(heap, statics, |t| t.ints_by_id.get(&id).copied())
                .flatten()
                .unwrap_or(0);
            Ok(BuiltinResult::Handled(Value::Int(i32::from(v != 0))))
        }
        "getResourcePackageName" => {
            let pkg = statics.get("PENGUDROID_PACKAGE_NAME").cloned().unwrap_or(Value::Ref(None));
            Ok(BuiltinResult::Handled(pkg))
        }
        _ => Ok(BuiltinResult::NotBuiltin),
    }
}
