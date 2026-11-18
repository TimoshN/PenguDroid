//! Plain-text Android IClipboard <-> daemon/Wayland clipboard support.

use pengudroid_proto::{Message, PROTO_VERSION, Role};
use pengudroid_socket::Connection;

fn surface_id() -> Result<u32, String> {
    ["PENGUDROID_LAST_SURFACE_ID", "PENGUDROID_SURFACE_ID"]
        .iter()
        .find_map(|name| std::env::var(name).ok()?.trim().parse::<u32>().ok())
        .filter(|id| *id != 0)
        .ok_or_else(|| "clipboard request has no surface id".to_string())
}

fn request(message: Message) -> Result<Message, String> {
    let conn = Connection::connect_endpoint(&pengudroid_proto::default_socket_path())
        .map_err(|e| e.to_string())?;
    conn.send_message(&Message::Hello {
        version: PROTO_VERSION,
        role: Role::Shim,
        pid: std::process::id() as i32,
    }, &[]).map_err(|e| e.to_string())?;
    let (_, fds) = conn.recv_message().map_err(|e| e.to_string())?;
    if !fds.is_empty() { return Err("unexpected clipboard handshake fds".into()); }
    conn.send_message(&message, &[]).map_err(|e| e.to_string())?;
    let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
    if !fds.is_empty() { return Err("unexpected clipboard reply fds".into()); }
    match reply {
        Message::Error { message, .. } => Err(message),
        other => Ok(other),
    }
}

pub fn set(text: String) -> Result<(), String> {
    let reply = request(Message::ClipboardSet { surface_id: surface_id()?, text })?;
    if reply == Message::Ack { Ok(()) } else { Err("unexpected clipboard set reply".into()) }
}

pub fn clear() -> Result<(), String> {
    let reply = request(Message::ClipboardClear { surface_id: surface_id()? })?;
    if reply == Message::Ack { Ok(()) } else { Err("unexpected clipboard clear reply".into()) }
}

pub fn get() -> Result<Option<String>, String> {
    match request(Message::ClipboardGet { surface_id: surface_id()? }) {
        Ok(Message::ClipboardText { text }) => Ok(Some(text)),
        Ok(_) => Err("unexpected clipboard get reply".into()),
        Err(error) if error.contains("empty") => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
struct ParcelReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ParcelReader<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, position: 0 } }
    fn align(&mut self, alignment: usize) -> Result<(), String> {
        let aligned = self.position.checked_add(alignment - 1).unwrap_or(usize::MAX)
            / alignment * alignment;
        if aligned > self.data.len() { return Err("Parcel alignment outside data".into()); }
        self.position = aligned;
        Ok(())
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self.position.checked_add(count).ok_or("Parcel overflow")?;
        let bytes = self.data.get(self.position..end).ok_or("truncated Parcel")?;
        self.position = end;
        Ok(bytes)
    }
    fn i32(&mut self) -> Result<i32, String> {
        self.align(4)?;
        Ok(i32::from_ne_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, String> {
        self.align(8)?;
        Ok(i64::from_ne_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string8(&mut self) -> Result<Option<String>, String> {
        let length = self.i32()?;
        if length == -1 { return Ok(None); }
        if length < 0 { return Err("negative String8 length".into()); }
        let bytes = self.take(length as usize + 1)?;
        if bytes.last() != Some(&0) { return Err("String8 has no NUL".into()); }
        self.align(4)?;
        Ok(Some(String::from_utf8(bytes[..length as usize].to_vec())
            .map_err(|_| "String8 is not UTF-8".to_string())?))
    }
    fn string16(&mut self) -> Result<Option<String>, String> {
        let length = self.i32()?;
        if length == -1 { return Ok(None); }
        if length < 0 || length > 16 * 1024 * 1024 { return Err("invalid String16 length".into()); }
        let bytes = self.take(length as usize * 2 + 2)?;
        if bytes.last() != Some(&0) { return Err("String16 has no NUL".into()); }
        self.align(4)?;
        let units: Vec<u16> = bytes[..length as usize * 2]
            .chunks_exact(2).map(|b| u16::from_ne_bytes([b[0], b[1]])).collect();
        Ok(Some(String::from_utf16(&units).map_err(|_| "invalid UTF-16".to_string())?))
    }
}

pub struct ParsedClipData {
    pub text: Option<String>,
    /// Exact bytes accepted by ClipData(Parcel), without the interface token
    /// or its outer readTypedObject presence word.
    pub raw: Vec<u8>,
}

/// Parse Android 17 ClipData and retain its exact byte representation. Only
/// plain CharSequence items with null optional payloads are accepted.
pub fn parse_clip_data(data: &[u8]) -> Result<ParsedClipData, String> {
    let mut p = ParcelReader::new(data);
    // libbinder prefixes the interface token with a native header (16 bytes in
    // the observed Android 17 request). Find it at an aligned boundary rather
    // than assuming that application ClipData starts at byte zero.
    let mut descriptor_end = None;
    for start in (0..data.len()).step_by(4) {
        let mut candidate = ParcelReader { data, position: start };
        if candidate.string16().ok().flatten().as_deref() == Some("android.content.IClipboard") {
            descriptor_end = Some(candidate.position);
            break;
        }
    }
    let descriptor_end = descriptor_end.ok_or("IClipboard Parcel descriptor not found")?;
    p.position = descriptor_end;
    if p.i32()? != 1 {
        return Err("ClipData is null".into());
    }
    let label_kind = p.i32()?;
    let _label = p.string8()?;
    if label_kind != 1 { return Err("spanned ClipDescription label is unsupported".into()); }
    let mime_count = p.i32()?;
    if !(0..=64).contains(&mime_count) { return Err("invalid clipboard MIME count".into()); }
    for _ in 0..mime_count { let _ = p.string16()?; }
    if p.i32()? != -1 { return Err("clipboard extras bundle is unsupported".into()); }
    let _timestamp = p.i64()?;
    let _styled = p.i32()?;
    let _classification = p.i32()?;
    let confidences = p.i32()?;
    if confidences != -1 && confidences != 0 {
        return Err("clipboard confidence bundle is unsupported".into());
    }
    if p.i32()? != 0 { return Err("clipboard icon is unsupported".into()); }
    let items = p.i32()?;
    if items < 1 || items > 1024 { return Err("invalid clipboard item count".into()); }
    let raw_start = descriptor_end + 4;
    let text_kind = p.i32()?;
    if text_kind != 1 {
        return Err("spanned clipboard items are unsupported".into());
    }
    let text = p.string8()?;
    let _html = p.string8()?;
    for _ in 0..5 {
        if p.i32()? != 0 {
            return Err("non-null optional clipboard item payload is unsupported".into());
        }
    }
    let raw = data.get(raw_start..p.position).ok_or("invalid ClipData bounds")?.to_vec();
    Ok(ParsedClipData { text, raw })
}

pub fn clip_data_text(data: &[u8]) -> Result<Option<String>, String> {
    Ok(parse_clip_data(data)?.text)
}

fn write_i32(out: &mut Vec<u8>, value: i32) { out.extend_from_slice(&value.to_ne_bytes()); }
fn write_i64(out: &mut Vec<u8>, value: i64) {
    while out.len() % 8 != 0 { out.push(0); }
    out.extend_from_slice(&value.to_ne_bytes());
}
fn write_string8(out: &mut Vec<u8>, value: Option<&str>) {
    let Some(value) = value else { write_i32(out, -1); return };
    write_i32(out, value.len() as i32);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 { out.push(0); }
}
fn write_string16(out: &mut Vec<u8>, value: &str) {
    let units: Vec<u16> = value.encode_utf16().collect();
    write_i32(out, units.len() as i32);
    for unit in units { out.extend_from_slice(&unit.to_ne_bytes()); }
    out.extend_from_slice(&0u16.to_ne_bytes());
    while out.len() % 4 != 0 { out.push(0); }
}

pub fn description_parcel() -> Vec<u8> {
    let mut out = Vec::new();
    write_i32(&mut out, 1); // null label CharSequence
    write_string8(&mut out, None);
    write_i32(&mut out, 1); // one MIME
    write_string16(&mut out, "text/plain");
    write_i32(&mut out, -1); // no PersistableBundle extras
    write_i64(&mut out, 0); // timestamp
    write_i32(&mut out, 0); // not styled
    write_i32(&mut out, 0); // classification status
    write_i32(&mut out, 0); // empty confidence Bundle, matching Android 17
    out
}

pub fn clip_data_parcel(text: &str) -> Vec<u8> {
    let mut out = description_parcel();
    write_i32(&mut out, 0); // no icon
    write_i32(&mut out, 1); // one item
    write_i32(&mut out, 1); // plain CharSequence
    write_string8(&mut out, Some(text));
    write_string8(&mut out, None); // no HTML
    for _ in 0..5 { write_i32(&mut out, 0); } // Intent/IntentSender/Uri/ActivityInfo/TextLinks
    out
}

#[no_mangle]
pub unsafe extern "C" fn pengudroid_clipboard_set_text(text: *const u8, length: usize) -> i32 {
    if text.is_null() { return -1; }
    let bytes = unsafe { std::slice::from_raw_parts(text, length) };
    let Ok(text) = std::str::from_utf8(bytes) else { return -1 };
    i32::from(set(text.to_string()).is_ok())
}

/// Return the required byte length, even when `out` is null. A zero-length
/// clipboard string is valid; errors return -1.
#[no_mangle]
pub unsafe extern "C" fn pengudroid_clipboard_get_text(out: *mut u8, capacity: usize) -> i32 {
    let text = match get() {
        Ok(Some(text)) => text,
        Ok(None) => return 0,
        Err(_) => return -1,
    };
    if out.is_null() || capacity < text.len() { return text.len() as i32; }
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    text.len() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_clip_data_codec_roundtrip() {
        let parcel = clip_data_parcel("hello");
        let request = {
            let mut out = Vec::new();
            write_string16(&mut out, "android.content.IClipboard");
            write_i32(&mut out, 1); // ClipData readTypedObject presence
            out.extend_from_slice(&parcel);
            out
        };
        assert_eq!(clip_data_text(&request).unwrap().as_deref(), Some("hello"));
        let mut prefixed = vec![0u8; 16];
        prefixed.extend_from_slice(&request);
        assert_eq!(clip_data_text(&prefixed).unwrap().as_deref(), Some("hello"));
    }

    #[test]
    fn malformed_clip_data_is_rejected() {
        assert!(clip_data_text(&[]).is_err());
        let parcel = clip_data_parcel("secret");
        for length in 0..parcel.len() {
            assert!(clip_data_text(&parcel[..length]).is_err());
        }
    }
}
