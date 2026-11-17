//! Dalvik Executable (`.dex`) file parser.
//!
//! Supports DEX formats `035`..`041` with bounds-checked decoding of
//! string/type/proto/field/method tables, class definitions, `class_data_item`,
//! and `code_item` (registers, insns, try/catch handlers).

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DexFile {
    pub strings: Vec<String>,
    pub types: Vec<String>,
    pub protos: Vec<ProtoId>,
    pub fields: Vec<FieldId>,
    pub methods: Vec<MethodId>,
    pub classes: Vec<ClassDef>,
    pub class_by_name: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct ProtoId {
    pub shorty: String,
    pub return_type: String,
    pub params: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FieldId {
    pub class_name: String,
    pub type_name: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct MethodId {
    pub class_name: String,
    pub proto_idx: usize,
    pub name: String,
    pub descriptor: String,
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub name: String,
    pub access_flags: u32,
    pub superclass: Option<String>,
    pub interfaces: Vec<String>,
    pub signature: Option<String>,
    pub field_annotations: HashMap<usize, Vec<DexAnnotation>>,
    pub static_fields: Vec<EncodedField>,
    pub instance_fields: Vec<EncodedField>,
    pub direct_methods: Vec<EncodedMethod>,
    pub virtual_methods: Vec<EncodedMethod>,
}

#[derive(Debug, Clone)]
pub enum AnnotationValue {
    String(String),
    StringArray(Vec<String>),
    Int(i32),
    Boolean(bool),
    Other,
}

#[derive(Debug, Clone)]
pub struct DexAnnotation {
    pub type_name: String,
    pub elements: Vec<(String, AnnotationValue)>,
}

#[derive(Debug, Clone)]
pub struct EncodedField {
    pub field_idx: usize,
    pub access_flags: u32,
}

#[derive(Debug, Clone)]
pub struct EncodedMethod {
    pub method_idx: usize,
    pub access_flags: u32,
    pub code: Option<Arc<CodeItem>>,
}

#[derive(Debug, Clone)]
pub struct CodeItem {
    pub registers_size: u16,
    pub ins_size: u16,
    pub outs_size: u16,
    pub insns: Vec<u16>,
    pub tries: Vec<TryBlock>,
}

#[derive(Debug, Clone)]
pub struct TryBlock {
    pub start_addr: u32,
    pub insn_count: u16,
    pub handlers: Vec<CatchHandler>,
    pub catch_all_addr: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CatchHandler {
    pub type_name: String,
    pub addr: u32,
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    fn u8(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("unexpected EOF")?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let s = self.data.get(self.pos..self.pos + 2).ok_or("unexpected EOF")?;
        self.pos += 2;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let s = self.data.get(self.pos..self.pos + 4).ok_or("unexpected EOF")?;
        self.pos += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn uleb128(&mut self) -> Result<u32, String> {
        let mut res: u32 = 0;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            res |= ((b & 0x7f) as u32) << shift;
            if (b & 0x80) == 0 {
                return Ok(res);
            }
        }
        Err("invalid uleb128".into())
    }

    fn sleb128(&mut self) -> Result<i32, String> {
        let mut res: i32 = 0;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            res |= ((b & 0x7f) as i32) << shift;
            shift += 7;
            if (b & 0x80) == 0 {
                if shift < 32 && (b & 0x40) != 0 {
                    res |= !0i32 << shift;
                }
                return Ok(res);
            }
            if shift >= 35 {
                return Err("invalid sleb128".into());
            }
        }
    }
}

fn read_u16_at(data: &[u8], off: usize) -> Result<u16, String> {
    let s = data.get(off..off + 2).ok_or("out-of-bounds u16")?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32_at(data: &[u8], off: usize) -> Result<u32, String> {
    let s = data.get(off..off + 4).ok_or("out-of-bounds u32")?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn decode_mutf8(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let a = bytes[i];
        if a == 0 {
            break;
        }
        if a < 0x80 {
            out.push(a as char);
            i += 1;
        } else if (a & 0xe0) == 0xc0 && i + 1 < bytes.len() {
            let b = bytes[i + 1];
            let cp = (((a & 0x1f) as u32) << 6) | ((b & 0x3f) as u32);
            if let Some(c) = char::from_u32(cp) {
                out.push(c);
            }
            i += 2;
        } else if (a & 0xf0) == 0xe0 && i + 2 < bytes.len() {
            let b = bytes[i + 1];
            let c = bytes[i + 2];
            let cp = (((a & 0x0f) as u32) << 12)
                | (((b & 0x3f) as u32) << 6)
                | ((c & 0x3f) as u32);
            if let Some(ch) = char::from_u32(cp) {
                out.push(ch);
            }
            i += 3;
        } else {
            out.push('\u{FFFD}');
            i += 1;
        }
    }
    out
}

fn read_type_list(data: &[u8], off: u32, types: &[String]) -> Result<Vec<String>, String> {
    if off == 0 {
        return Ok(Vec::new());
    }
    let off = off as usize;
    let size = read_u32_at(data, off)? as usize;
    let mut out = Vec::with_capacity(size);
    for i in 0..size {
        let idx = read_u16_at(data, off + 4 + i * 2)? as usize;
        out.push(types.get(idx).ok_or("bad type_list idx")?.clone());
    }
    Ok(out)
}

fn parse_code_item(data: &[u8], off: u32, types: &[String]) -> Result<CodeItem, String> {
    let mut c = Cursor::new(data, off as usize);
    let registers_size = c.u16()?;
    let ins_size = c.u16()?;
    let outs_size = c.u16()?;
    let tries_size = c.u16()?;
    let _debug_info_off = c.u32()?;
    let insns_size = c.u32()? as usize;

    let mut insns = Vec::with_capacity(insns_size);
    for _ in 0..insns_size {
        insns.push(c.u16()?);
    }

    let mut tries = Vec::new();
    if tries_size > 0 {
        if insns_size % 2 == 1 {
            let _padding = c.u16()?;
        }
        let tries_off = c.pos;
        let handlers_base = tries_off + (tries_size as usize) * 8;
        for i in 0..(tries_size as usize) {
            let entry_off = tries_off + i * 8;
            let start_addr = read_u32_at(data, entry_off)?;
            let insn_count = read_u16_at(data, entry_off + 4)?;
            let handler_off = read_u16_at(data, entry_off + 6)? as usize;

            let mut hc = Cursor::new(data, handlers_base + handler_off);
            let count = hc.sleb128()?;
            let abs_count = count.unsigned_abs() as usize;
            let mut handlers = Vec::with_capacity(abs_count);
            for _ in 0..abs_count {
                let type_idx = hc.uleb128()? as usize;
                let addr = hc.uleb128()?;
                let type_name = types.get(type_idx).ok_or("bad catch type")?.clone();
                handlers.push(CatchHandler { type_name, addr });
            }
            let catch_all_addr = if count <= 0 { Some(hc.uleb128()?) } else { None };
            tries.push(TryBlock {
                start_addr,
                insn_count,
                handlers,
                catch_all_addr,
            });
        }
    }

    Ok(CodeItem {
        registers_size,
        ins_size,
        outs_size,
        insns,
        tries,
    })
}

impl DexFile {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 0x70 || &data[0..4] != b"dex\n" {
            return Err("not a valid DEX file".into());
        }
        let string_ids_size = read_u32_at(data, 0x38)? as usize;
        let string_ids_off = read_u32_at(data, 0x3c)? as usize;
        let type_ids_size = read_u32_at(data, 0x40)? as usize;
        let type_ids_off = read_u32_at(data, 0x44)? as usize;
        let proto_ids_size = read_u32_at(data, 0x48)? as usize;
        let proto_ids_off = read_u32_at(data, 0x4c)? as usize;
        let field_ids_size = read_u32_at(data, 0x50)? as usize;
        let field_ids_off = read_u32_at(data, 0x54)? as usize;
        let method_ids_size = read_u32_at(data, 0x58)? as usize;
        let method_ids_off = read_u32_at(data, 0x5c)? as usize;
        let class_defs_size = read_u32_at(data, 0x60)? as usize;
        let class_defs_off = read_u32_at(data, 0x64)? as usize;

        let mut strings = Vec::with_capacity(string_ids_size);
        for i in 0..string_ids_size {
            let str_off = read_u32_at(data, string_ids_off + i * 4)? as usize;
            let mut c = Cursor::new(data, str_off);
            let _utf16_len = c.uleb128()?;
            let start = c.pos;
            let end = data[start..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| start + p)
                .unwrap_or(data.len());
            strings.push(decode_mutf8(&data[start..end]));
        }

        let mut types = Vec::with_capacity(type_ids_size);
        for i in 0..type_ids_size {
            let desc_idx = read_u32_at(data, type_ids_off + i * 4)? as usize;
            types.push(strings.get(desc_idx).ok_or("bad type descriptor")?.clone());
        }

        let mut protos = Vec::with_capacity(proto_ids_size);
        for i in 0..proto_ids_size {
            let off = proto_ids_off + i * 12;
            let shorty_idx = read_u32_at(data, off)? as usize;
            let ret_idx = read_u32_at(data, off + 4)? as usize;
            let params_off = read_u32_at(data, off + 8)?;
            protos.push(ProtoId {
                shorty: strings.get(shorty_idx).ok_or("bad proto shorty")?.clone(),
                return_type: types.get(ret_idx).ok_or("bad proto ret")?.clone(),
                params: read_type_list(data, params_off, &types)?,
            });
        }

        let mut fields = Vec::with_capacity(field_ids_size);
        for i in 0..field_ids_size {
            let off = field_ids_off + i * 8;
            let class_idx = read_u16_at(data, off)? as usize;
            let type_idx = read_u16_at(data, off + 2)? as usize;
            let name_idx = read_u32_at(data, off + 4)? as usize;
            fields.push(FieldId {
                class_name: types.get(class_idx).ok_or("bad field class")?.clone(),
                type_name: types.get(type_idx).ok_or("bad field type")?.clone(),
                name: strings.get(name_idx).ok_or("bad field name")?.clone(),
            });
        }

        let mut methods = Vec::with_capacity(method_ids_size);
        for i in 0..method_ids_size {
            let off = method_ids_off + i * 8;
            let class_idx = read_u16_at(data, off)? as usize;
            let proto_idx = read_u16_at(data, off + 2)? as usize;
            let name_idx = read_u32_at(data, off + 4)? as usize;
            let proto = protos.get(proto_idx).ok_or("bad method proto")?;
            let mut desc = String::from("(");
            for p in &proto.params {
                desc.push_str(p);
            }
            desc.push(')');
            desc.push_str(&proto.return_type);
            methods.push(MethodId {
                class_name: types.get(class_idx).ok_or("bad method class")?.clone(),
                proto_idx,
                name: strings.get(name_idx).ok_or("bad method name")?.clone(),
                descriptor: desc,
            });
        }

        let mut classes = Vec::with_capacity(class_defs_size);
        let mut class_by_name = HashMap::with_capacity(class_defs_size);
        for i in 0..class_defs_size {
            let off = class_defs_off + i * 32;
            let class_idx = read_u32_at(data, off)? as usize;
            let access_flags = read_u32_at(data, off + 4)?;
            let superclass_idx = read_u32_at(data, off + 8)?;
            let interfaces_off = read_u32_at(data, off + 12)?;
            let _source_file_idx = read_u32_at(data, off + 16)?;
            let annotations_off = read_u32_at(data, off + 20)?;
            let class_data_off = read_u32_at(data, off + 24)?;
            let _static_values_off = read_u32_at(data, off + 28)?;

            let name = types.get(class_idx).ok_or("bad class_def type")?.clone();
            let superclass = if superclass_idx == 0xffff_ffff {
                None
            } else {
                Some(types.get(superclass_idx as usize).ok_or("bad superclass")?.clone())
            };
            let interfaces = read_type_list(data, interfaces_off, &types)?;

            let mut static_fields = Vec::new();
            let mut instance_fields = Vec::new();
            let mut direct_methods = Vec::new();
            let mut virtual_methods = Vec::new();

            if class_data_off != 0 {
                let mut c = Cursor::new(data, class_data_off as usize);
                let sf_len = c.uleb128()? as usize;
                let if_len = c.uleb128()? as usize;
                let dm_len = c.uleb128()? as usize;
                let vm_len = c.uleb128()? as usize;

                let mut prev = 0usize;
                for _ in 0..sf_len {
                    prev += c.uleb128()? as usize;
                    let flags = c.uleb128()?;
                    static_fields.push(EncodedField { field_idx: prev, access_flags: flags });
                }
                prev = 0;
                for _ in 0..if_len {
                    prev += c.uleb128()? as usize;
                    let flags = c.uleb128()?;
                    instance_fields.push(EncodedField { field_idx: prev, access_flags: flags });
                }
                prev = 0;
                for _ in 0..dm_len {
                    prev += c.uleb128()? as usize;
                    let flags = c.uleb128()?;
                    let code_off = c.uleb128()?;
                    let code = if code_off != 0 {
                        Some(Arc::new(parse_code_item(data, code_off, &types)?))
                    } else {
                        None
                    };
                    direct_methods.push(EncodedMethod { method_idx: prev, access_flags: flags, code });
                }
                prev = 0;
                for _ in 0..vm_len {
                    prev += c.uleb128()? as usize;
                    let flags = c.uleb128()?;
                    let code_off = c.uleb128()?;
                    let code = if code_off != 0 {
                        Some(Arc::new(parse_code_item(data, code_off, &types)?))
                    } else {
                        None
                    };
                    virtual_methods.push(EncodedMethod { method_idx: prev, access_flags: flags, code });
                }
            }

            class_by_name.insert(name.clone(), classes.len());
            let signature = read_signature_annotation(data, annotations_off, &strings, &types);
            let field_annotations = read_field_annotations(data, annotations_off, &strings, &types);
            classes.push(ClassDef {
                name,
                access_flags,
                superclass,
                interfaces,
                signature,
                field_annotations,
                static_fields,
                instance_fields,
                direct_methods,
                virtual_methods,
            });
        }

        Ok(Self {
            strings,
            types,
            protos,
            fields,
            methods,
            classes,
            class_by_name,
        })
    }
}

fn read_uleb_slice(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut res = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let b = *data.get(*pos)?;
        *pos += 1;
        res |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(res);
        }
        shift += 7;
    }
    None
}

fn read_uint_le(data: &[u8], pos: &mut usize, nbytes: usize) -> Option<u32> {
    let slice = data.get(*pos..*pos + nbytes)?;
    *pos += nbytes;
    let mut out = 0u32;
    for (i, &b) in slice.iter().enumerate() {
        out |= u32::from(b) << (i * 8);
    }
    Some(out)
}

fn skip_encoded_value(data: &[u8], pos: &mut usize) -> Option<()> {
    let header = *data.get(*pos)?;
    *pos += 1;
    let vtype = header & 0x1f;
    let varg = usize::from(header >> 5);
    match vtype {
        0x00 | 0x02..=0x04 | 0x06 | 0x10..=0x11 | 0x15..=0x1b => {
            let n = varg + 1;
            data.get(*pos..*pos + n)?;
            *pos += n;
        }
        0x1c => {
            let count = read_uleb_slice(data, pos)?;
            for _ in 0..count {
                skip_encoded_value(data, pos)?;
            }
        }
        0x1d => {
            let _ty = read_uleb_slice(data, pos)?;
            let count = read_uleb_slice(data, pos)?;
            for _ in 0..count {
                let _name = read_uleb_slice(data, pos)?;
                skip_encoded_value(data, pos)?;
            }
        }
        0x1e | 0x1f => {}
        _ => return None,
    }
    Some(())
}

fn read_signature_annotation(
    data: &[u8],
    annotations_off: u32,
    strings: &[String],
    types: &[String],
) -> Option<String> {
    if annotations_off == 0 {
        return None;
    }
    let class_ann_off = read_u32_at(data, annotations_off as usize).ok()? as usize;
    if class_ann_off == 0 {
        return None;
    }
    let set_size = read_u32_at(data, class_ann_off).ok()? as usize;
    for i in 0..set_size {
        let ann_off = read_u32_at(data, class_ann_off + 4 + i * 4).ok()? as usize;
        let mut pos = ann_off + 1; // skip visibility byte
        let type_idx = read_uleb_slice(data, &mut pos)? as usize;
        let count = read_uleb_slice(data, &mut pos)?;
        let is_sig = types.get(type_idx).map(String::as_str) == Some("Ldalvik/annotation/Signature;");
        for _ in 0..count {
            let name_idx = read_uleb_slice(data, &mut pos)? as usize;
            let is_val = strings.get(name_idx).map(String::as_str) == Some("value");
            if is_sig && is_val {
                let header = *data.get(pos)?;
                pos += 1;
                if (header & 0x1f) != 0x1c {
                    return None;
                }
                let arr_len = read_uleb_slice(data, &mut pos)?;
                let mut out = String::new();
                for _ in 0..arr_len {
                    let eh = *data.get(pos)?;
                    pos += 1;
                    if (eh & 0x1f) != 0x17 {
                        return None;
                    }
                    let sidx = read_uint_le(data, &mut pos, usize::from(eh >> 5) + 1)? as usize;
                    out.push_str(strings.get(sidx)?);
                }
                return Some(out);
            }
            skip_encoded_value(data, &mut pos)?;
        }
    }
    None
}

fn read_encoded_annotation_value(
    data: &[u8],
    pos: &mut usize,
    strings: &[String],
) -> Option<AnnotationValue> {
    let header = *data.get(*pos)?;
    *pos += 1;
    let vtype = header & 0x1f;
    let varg = usize::from(header >> 5);
    match vtype {
        0x17 => {
            let sidx = read_uint_le(data, pos, varg + 1)? as usize;
            Some(AnnotationValue::String(strings.get(sidx)?.clone()))
        }
        0x1c => {
            let count = read_uleb_slice(data, pos)?;
            let mut items = Vec::with_capacity(count as usize);
            let mut all_strings = true;
            for _ in 0..count {
                match read_encoded_annotation_value(data, pos, strings)? {
                    AnnotationValue::String(s) => items.push(s),
                    _ => all_strings = false,
                }
            }
            if all_strings {
                Some(AnnotationValue::StringArray(items))
            } else {
                Some(AnnotationValue::Other)
            }
        }
        0x04 => {
            let val = read_uint_le(data, pos, varg + 1)? as i32;
            Some(AnnotationValue::Int(val))
        }
        0x1f => Some(AnnotationValue::Boolean(varg != 0)),
        0x00 | 0x02..=0x03 | 0x06 | 0x10..=0x11 | 0x15..=0x16 | 0x18..=0x1b => {
            let n = varg + 1;
            data.get(*pos..*pos + n)?;
            *pos += n;
            Some(AnnotationValue::Other)
        }
        0x1d => {
            let _ty = read_uleb_slice(data, pos)?;
            let count = read_uleb_slice(data, pos)?;
            for _ in 0..count {
                let _name = read_uleb_slice(data, pos)?;
                skip_encoded_value(data, pos)?;
            }
            Some(AnnotationValue::Other)
        }
        0x1e => Some(AnnotationValue::Other),
        _ => None,
    }
}

fn read_field_annotations(
    data: &[u8],
    annotations_off: u32,
    strings: &[String],
    types: &[String],
) -> HashMap<usize, Vec<DexAnnotation>> {
    let mut out = HashMap::new();
    if annotations_off == 0 {
        return out;
    }
    let dir = annotations_off as usize;
    let Some(fields_size) = read_u32_at(data, dir + 4).ok().map(|v| v as usize) else {
        return out;
    };
    for i in 0..fields_size {
        let entry_off = dir + 16 + i * 8;
        let (Ok(field_idx), Ok(set_off)) = (read_u32_at(data, entry_off), read_u32_at(data, entry_off + 4)) else {
            break;
        };
        if set_off == 0 {
            continue;
        }
        let Ok(set_size) = read_u32_at(data, set_off as usize) else {
            continue;
        };
        let mut anns = Vec::new();
        for j in 0..set_size as usize {
            let Ok(ann_off) = read_u32_at(data, set_off as usize + 4 + j * 4) else {
                break;
            };
            let ann_off = ann_off as usize;
            let Some(&visibility) = data.get(ann_off) else {
                continue;
            };
            if visibility != 1 {
                continue;
            }
            let mut pos = ann_off + 1;
            let Some(type_idx) = read_uleb_slice(data, &mut pos).map(|v| v as usize) else {
                continue;
            };
            let Some(type_name) = types.get(type_idx).cloned() else {
                continue;
            };
            let Some(count) = read_uleb_slice(data, &mut pos) else {
                continue;
            };
            let mut elements = Vec::new();
            for _ in 0..count {
                let Some(name_idx) = read_uleb_slice(data, &mut pos).map(|v| v as usize) else {
                    break;
                };
                let Some(elem_name) = strings.get(name_idx).cloned() else {
                    break;
                };
                let Some(val) = read_encoded_annotation_value(data, &mut pos, strings) else {
                    break;
                };
                elements.push((elem_name, val));
            }
            anns.push(DexAnnotation { type_name, elements });
        }
        if !anns.is_empty() {
            out.insert(field_idx as usize, anns);
        }
    }
    out
}
