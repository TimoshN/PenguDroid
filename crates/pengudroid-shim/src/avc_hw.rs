//! Hardware H.264 behind the stock software Codec2 component.
//!
//! `c2.android.avc.decoder` (`libcodec2_soft_avcdec.so`) is a complete, correct
//! Codec2 component: work queue, flush, EOS, formats, block pools. All of its
//! actual decoding goes through one C entry point of the statically linked
//! libavc, `ih264d_api_function(handle, in, out)`. This module hooks that entry
//! and nothing else, so the framework contract stays the real component's and
//! only the pixels come from somewhere else:
//!
//! * header decodes, controls, create and delete still run in libavc, which
//!   therefore keeps reporting the real picture size and VUI;
//! * a frame decode sends the access unit to the daemon's held hardware decoder
//!   (`VideoHeldDecode`) and, when that returns a picture, writes a small tag
//!   naming it into the output block instead of pixels;
//! * the component finishes its work as usual, the tagged block travels through
//!   the real BufferQueue, and `blast.rs` recognises the tag when the app
//!   renders that buffer and asks the daemon to show the named frame
//!   (`VideoHeldPresent`). Timing, drops and A/V sync remain the app's.
//!
//! If the daemon has no hardware decoder, or refuses the very first picture,
//! the instance stays on libavc: nothing has been withheld from it by then.
//!
//! The hook is pinned to one binary (Build ID below) and re-checked against the
//! bytes it replaces; any mismatch leaves the component untouched. Structure
//! layouts are libavc's public `decoder/ivd.h` (external/libavc), cross-checked
//! on that binary with `PENGUDROID_HW_VIDEO=observe`.
use pengudroid_proto::{default_socket_path, Message, Role, PROTO_VERSION};
use pengudroid_socket::Connection;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// `libcodec2_soft_avcdec.so` from the API-37 `com.android.media.swcodec` APEX.
const BUILD_ID: [u8; 16] = [
    0x09, 0x4d, 0xf4, 0xe6, 0xcf, 0x76, 0xa4, 0x48, 0x17, 0xe5, 0xfd, 0xa1, 0x64, 0x68, 0x4d, 0x6b,
];
/// `ih264d_api_function`, from the library's `.gnu_debugdata` symbol table.
const API_OFFSET: usize = 0x40b50;
/// `ih264d_api_function.cfi_jt`: a `jmp` to the function, used as a cross-check.
const CFI_JUMP_OFFSET: usize = 0x907a0;
/// The position-independent prologue the patch overwrites: push rbp; mov
/// rbp,rsp; push r15; push r14; push r12; push rbx; sub rsp,0x10.
const PROLOGUE: [u8; 15] = [
    0x55, 0x48, 0x89, 0xe5, 0x41, 0x57, 0x41, 0x56, 0x41, 0x54, 0x53, 0x48, 0x83, 0xec, 0x10,
];

// `IVD_API_COMMAND_TYPE_T` (`IV_CMD_DUMMY_ELEMENT` is 4, `IVD_CMD_CREATE` 5).
const CMD_DELETE: u32 = 6;
const CMD_CTL: u32 = 7;
const CMD_DECODE: u32 = 8;
// `IVD_CONTROL_API_COMMAND_TYPE_T`.
const CTL_SETPARAMS: u32 = 1;
const CTL_RESET: u32 = 2;
const CTL_FLUSH: u32 = 4;
/// `IVD_DECODE_HEADER`.
const MODE_HEADER: u32 = 1;
/// `IVD_RES_CHANGED`, and bit `IVD_FATALERROR` of `u4_error_code`.
const ERROR_RES_CHANGED: u32 = 0x2a;
const ERROR_FATAL: u32 = 1 << 0xe;

// `ivd_video_decode_ip_t`.
const IP_CMD: usize = 4;
const IP_TS: usize = 8;
const IP_NUM_BYTES: usize = 12;
const IP_STREAM: usize = 16;
const IP_NUM_BUFS: usize = 24;
const IP_BUF0: usize = 32;
const IP_MIN_SIZE0: usize = 32 + 64 * 8;
// `ivd_ctl_set_config_ip_t`.
const IP_SUB_CMD: usize = 8;
const IP_DEC_MODE: usize = 12;
// `ivd_video_decode_op_t` as this binary lays it out (184 bytes). It is older
// than today's header: `iv_yuv_buf_t` holds its three pointers before the
// dimensions (72 bytes from offset 48), so everything after it sits 8 bytes
// lower than in current `ivd.h`. Read off real decodes: `u4_ts` echoes the
// input timestamp two calls later, `i4_reorder_depth` is constant per stream.
const OP_ERROR: usize = 4;
const OP_CONSUMED: usize = 8;
const OP_PIC_WD: usize = 12;
const OP_PIC_HT: usize = 16;
const OP_FRAME_DECODED: usize = 24;
const OP_OUTPUT_PRESENT: usize = 32;
const OP_TS: usize = 132;
const OP_REORDER_DEPTH: usize = 140;
const OP_MIN_SIZE: usize = 148;

/// What a tagged output block starts with, followed by the session token and
/// the frame id (little-endian `u64` each).
const TAG_MAGIC: [u8; 16] = *b"PenguDroidHwFrm\x01";
pub const TAG_SIZE: usize = 32;

type ApiFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> c_int;
/// The relocated prologue followed by a jump back into libavc; 0 = not hooked.
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq)]
enum Policy { Off, Observe, On }
fn policy() -> Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    *POLICY.get_or_init(|| match std::env::var("PENGUDROID_HW_VIDEO").as_deref() {
        // Opt-in for now: a tagged block holds no pixels, so only a consumer
        // that understands the tag (the BLAST `SurfaceView` path) shows the
        // picture. `ImageReader`, `TextureView` and ByteBuffer output would
        // read the tag as pixels.
        Ok("1") | Ok("on") => Policy::On,
        Ok("observe") => Policy::Observe,
        _ => Policy::Off,
    })
}
fn trace() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("PENGUDROID_TRACE_VIDEO").is_some())
}

/// The daemon side of one decoder instance. `blast.rs` reaches it by token,
/// from another thread, to show a frame on the same connection that decoded it.
struct Session { conn: Mutex<Connection>, token: u64 }
fn sessions() -> &'static Mutex<HashMap<u64, Arc<Session>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, Arc<Session>>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}
impl Session {
    fn open() -> Result<Arc<Session>, String> {
        let conn = Connection::connect_endpoint(&default_socket_path()).map_err(|e| e.to_string())?;
        conn.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
        let hello = Message::Hello { version: PROTO_VERSION, role: Role::Guest, pid: std::process::id() as i32 };
        match exchange(&conn, &hello)? {
            Message::Welcome { version, .. } if version == PROTO_VERSION => {}
            reply => return Err(format!("daemon handshake: {reply:?}")),
        }
        let device = std::env::var("PENGUDROID_VIDEO_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        match exchange(&conn, &Message::VideoHeldOpen { codec: "h264".into(), device, extra: Vec::new() })? {
            Message::VideoDecoderReady => {}
            reply => return Err(format!("open: {reply:?}")),
        }
        // Unpredictable enough that stale bytes in a recycled buffer cannot
        // name a live session by accident.
        let token = (std::process::id() as u64) << 40
            ^ NEXT_TOKEN.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (&conn as *const Connection as u64).rotate_left(17);
        let session = Arc::new(Session { conn: Mutex::new(conn), token });
        sessions().lock().unwrap().insert(token, session.clone());
        Ok(session)
    }
    fn request(&self, message: &Message) -> Result<Message, String> {
        exchange(&self.conn.lock().unwrap(), message)
    }
    /// Send one access unit (or, with no data, EOS / a plain poll) and take the
    /// next decoded frame, if any: `(id, pts)`, then whether the stream drained.
    fn decode(&self, pts: i64, data: Vec<u8>, end_of_stream: bool) -> Result<(Option<(u64, i64)>, bool), String> {
        let sent = !data.is_empty();
        match self.request(&Message::VideoHeldDecode { pts_us: pts, data, end_of_stream })? {
            Message::VideoHeldFrame { accepted, frame, drained } => {
                if sent && !accepted { return Err("daemon did not accept the access unit".into()); }
                Ok((frame.map(|(id, pts, _, _)| (id, pts)), drained))
            }
            reply => Err(format!("decode: {reply:?}")),
        }
    }
    fn flush(&self) -> Result<(), String> {
        match self.request(&Message::VideoDecoderFlush)? {
            Message::Ack => Ok(()),
            reply => Err(format!("flush: {reply:?}")),
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        // Closing the connection closes the daemon's decoder with it.
        if trace() { eprintln!("[avc-hw] session {:#x} closed", self.token); }
    }
}
fn exchange(conn: &Connection, message: &Message) -> Result<Message, String> {
    conn.send_message(message, &[]).map_err(|e| e.to_string())?;
    let (reply, fds) = conn.recv_message().map_err(|e| e.to_string())?;
    for fd in fds { unsafe { libc::close(fd) }; }
    match reply {
        Message::Error { message, .. } => Err(message),
        reply => Ok(reply),
    }
}

/// Per `iv_obj_t` state. libavc itself always has the truth about headers.
#[derive(Default)]
struct Instance {
    header_mode: bool,
    /// Decided for good: this instance decodes in libavc.
    software: bool,
    session: Option<Arc<Session>>,
    /// Parameter sets libavc consumed (header mode, or a parameter-set-only
    /// buffer) that the hardware decoder has not been given yet.
    prefix: Vec<u8>,
    last_sps: Vec<u8>,
    width: u32,
    height: u32,
    /// Access units sent since the decoder was opened or flushed.
    sent: u64,
    pictures: u64,
    observed: u32,
}
fn instances() -> &'static Mutex<HashMap<usize, Arc<Mutex<Instance>>>> {
    static INSTANCES: OnceLock<Mutex<HashMap<usize, Arc<Mutex<Instance>>>>> = OnceLock::new();
    INSTANCES.get_or_init(Default::default)
}
fn instance(handle: *mut c_void) -> Arc<Mutex<Instance>> {
    instances().lock().unwrap().entry(handle as usize).or_default().clone()
}

unsafe fn read_u32(base: *const c_void, offset: usize) -> u32 {
    (base as *const u8).add(offset).cast::<u32>().read_unaligned()
}
unsafe fn write_u32(base: *mut c_void, offset: usize, value: u32) {
    (base as *mut u8).add(offset).cast::<u32>().write_unaligned(value)
}
unsafe fn original(handle: *mut c_void, ip: *mut c_void, op: *mut c_void) -> c_int {
    let entry: ApiFn = std::mem::transmute(ORIGINAL.load(Ordering::Acquire));
    entry(handle, ip, op)
}

/// Annex-B NAL units of `data` as `(nal_unit_type, bytes including start code)`.
fn nal_units(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // A fourth zero in front belongs to the start code.
            let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (index, (begin, payload)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(data.len(), |next| next.0);
        if *payload < end { units.push((data[*payload] & 0x1f, &data[*begin..end])); }
    }
    units
}
fn sequence_parameter_set(data: &[u8]) -> Option<&[u8]> {
    nal_units(data).into_iter().find(|(kind, _)| *kind == 7).map(|(_, bytes)| bytes)
}
fn has_picture(data: &[u8]) -> bool {
    nal_units(data).iter().any(|(kind, _)| matches!(kind, 1 | 5))
}

/// Start an answer libavc did not write: everything zero but the caller's size.
unsafe fn clear_output(op: *mut c_void) -> bool {
    let size = read_u32(op, 0) as usize;
    if !(OP_MIN_SIZE..=4096).contains(&size) { return false; }
    std::ptr::write_bytes((op as *mut u8).add(4), 0, size - 4);
    write_u32(op, OP_REORDER_DEPTH, u32::MAX); // -1: no new output delay
    true
}
/// Name a held frame in the luma plane of the caller's output block.
unsafe fn tag_output(ip: *const c_void, token: u64, frame: u64) -> bool {
    let luma = (ip as *const u8).add(IP_BUF0).cast::<*mut u8>().read_unaligned();
    if read_u32(ip, IP_NUM_BUFS) == 0 || luma.is_null() || (read_u32(ip, IP_MIN_SIZE0) as usize) < TAG_SIZE { return false; }
    let mut tag = [0u8; TAG_SIZE];
    tag[..16].copy_from_slice(&TAG_MAGIC);
    tag[16..24].copy_from_slice(&token.to_le_bytes());
    tag[24..32].copy_from_slice(&frame.to_le_bytes());
    std::ptr::copy_nonoverlapping(tag.as_ptr(), luma, TAG_SIZE);
    true
}

unsafe extern "C" fn hook(handle: *mut c_void, ip: *mut c_void, op: *mut c_void) -> c_int {
    if handle.is_null() || ip.is_null() || op.is_null() { return original(handle, ip, op); }
    match read_u32(ip, IP_CMD) {
        CMD_DECODE => decode(handle, ip, op),
        CMD_CTL => {
            let state = instance(handle);
            let sub_command = read_u32(ip, IP_SUB_CMD);
            {
                let mut state = state.lock().unwrap();
                match sub_command {
                    CTL_SETPARAMS => state.header_mode = read_u32(ip, IP_DEC_MODE) == MODE_HEADER,
                    CTL_RESET => {
                        // libavc forgets its parameter sets here; so must we.
                        state.prefix.clear();
                        state.last_sps.clear();
                        state.sent = 0;
                        if let Some(session) = &state.session {
                            if let Err(error) = session.flush() { eprintln!("[avc-hw] reset: {error}"); }
                        }
                    }
                    // Flush mode itself needs no bookkeeping: its decode calls
                    // carry no input, which is how `drain` recognises them.
                    CTL_FLUSH => {}
                    _ => {}
                }
            }
            original(handle, ip, op)
        }
        CMD_DELETE => {
            if let Some(state) = instances().lock().unwrap().remove(&(handle as usize)) {
                if let Some(session) = state.lock().unwrap().session.take() {
                    sessions().lock().unwrap().remove(&session.token);
                }
            }
            original(handle, ip, op)
        }
        _ => original(handle, ip, op),
    }
}

unsafe fn decode(handle: *mut c_void, ip: *mut c_void, op: *mut c_void) -> c_int {
    let state = instance(handle);
    let mut state = state.lock().unwrap();
    let length = read_u32(ip, IP_NUM_BYTES) as usize;
    let stream = (ip as *const u8).add(IP_STREAM).cast::<*const u8>().read_unaligned();
    let data: &[u8] = if stream.is_null() || length == 0 { &[] } else { std::slice::from_raw_parts(stream, length) };

    if state.software || state.header_mode || policy() != Policy::On {
        let status = original(handle, ip, op);
        if policy() == Policy::Observe && state.observed < 24 {
            state.observed += 1;
            let words: Vec<String> = (0..read_u32(op, 0).min(192) as usize / 4)
                .map(|index| format!("{:x}", read_u32(op, index * 4))).collect();
            eprintln!("[avc-hw] observe: header={} ts={} bytes={length} -> status={status} op=[{}]",
                state.header_mode, read_u32(ip, IP_TS), words.join(" "));
        }
        if state.header_mode && !state.software {
            // Remember what libavc took as headers: the hardware decoder needs
            // the same parameter sets in front of the first slice.
            let consumed = (read_u32(op, OP_CONSUMED) as usize).min(data.len());
            state.remember_parameter_sets(&data[..consumed]);
            let (width, height) = (read_u32(op, OP_PIC_WD), read_u32(op, OP_PIC_HT));
            if width > 0 && height > 0 { state.width = width; state.height = height; }
        }
        return status;
    }
    if data.is_empty() { return drain(&mut state, handle, ip, op); }

    if !has_picture(data) {
        // Parameter sets or SEI on their own: libavc parses them (it still
        // answers VUI queries), and the hardware decoder gets them with the
        // next slice.
        let status = original(handle, ip, op);
        state.remember_parameter_sets(data);
        if read_u32(op, OP_CONSUMED) == 0 { write_u32(op, OP_CONSUMED, length as u32); }
        return status;
    }
    // A different SPS in a running stream: have the component drain, reset and
    // decode the header again, exactly as it does for libavc's own report.
    if let Some(sps) = sequence_parameter_set(data) {
        if state.sent > 0 && !state.last_sps.is_empty() && state.last_sps != sps {
            if trace() { eprintln!("[avc-hw] sequence parameter set changed; reporting IVD_RES_CHANGED"); }
            if !clear_output(op) { return original(handle, ip, op); }
            write_u32(op, OP_ERROR, ERROR_RES_CHANGED);
            return 0;
        }
    }
    if state.session.is_none() {
        match Session::open() {
            Ok(session) => {
                if trace() { eprintln!("[avc-hw] hardware decoder session {:#x} opened", session.token); }
                state.session = Some(session);
            }
            Err(error) => {
                eprintln!("[avc-hw] no hardware decoder ({error}); decoding in software");
                state.software = true;
                return original(handle, ip, op);
            }
        }
    }
    let session = state.session.clone().unwrap();
    let mut packet = std::mem::take(&mut state.prefix);
    packet.extend_from_slice(data);
    let timestamp = read_u32(ip, IP_TS);
    let frame = match session.decode(timestamp as i64, packet, false) {
        Ok((frame, _)) => frame,
        Err(error) if state.pictures == 0 => {
            // Nothing has been withheld from libavc yet, so it can take over.
            eprintln!("[avc-hw] hardware decoder refused the first picture ({error}); decoding in software");
            sessions().lock().unwrap().remove(&session.token);
            state.session = None;
            state.software = true;
            return original(handle, ip, op);
        }
        Err(error) => {
            eprintln!("[avc-hw] hardware decode failed: {error}");
            if clear_output(op) { write_u32(op, OP_ERROR, ERROR_FATAL); }
            return 1; // IV_FAIL
        }
    };
    if let Some(sps) = sequence_parameter_set(data) { state.last_sps = sps.to_vec(); }
    state.sent += 1;
    state.pictures += 1;
    if !clear_output(op) {
        eprintln!("[avc-hw] unexpected decode output structure size {}", read_u32(op, 0));
        return 1;
    }
    write_u32(op, OP_CONSUMED, length as u32);
    write_u32(op, OP_PIC_WD, state.width);
    write_u32(op, OP_PIC_HT, state.height);
    write_u32(op, OP_FRAME_DECODED, 1);
    if let Some((id, pts)) = frame {
        if tag_output(ip, session.token, id) {
            write_u32(op, OP_OUTPUT_PRESENT, 1);
            write_u32(op, OP_TS, pts as u32);
        }
    }
    if trace() && (state.pictures <= 3 || state.pictures % 120 == 0) {
        eprintln!("[avc-hw] {} access units decoded on the GPU ({}x{})", state.pictures, state.width, state.height);
    }
    0
}

/// A decode call without input: flush mode. Hand back what the hardware decoder
/// still holds, one frame per call; when it has nothing left, let libavc answer
/// so that it leaves its own flush mode too.
unsafe fn drain(state: &mut Instance, handle: *mut c_void, ip: *mut c_void, op: *mut c_void) -> c_int {
    let Some(session) = state.session.clone() else { return original(handle, ip, op) };
    if state.sent > 0 {
        match session.decode(0, Vec::new(), true) {
            Ok((Some((id, pts)), _)) => {
                if clear_output(op) && tag_output(ip, session.token, id) {
                    write_u32(op, OP_PIC_WD, state.width);
                    write_u32(op, OP_PIC_HT, state.height);
                    write_u32(op, OP_OUTPUT_PRESENT, 1);
                    write_u32(op, OP_TS, pts as u32);
                    return 0;
                }
            }
            Ok((None, _)) => {}
            Err(error) => eprintln!("[avc-hw] drain: {error}"),
        }
        // Drained: make the decoder accept input again.
        if let Err(error) = session.flush() { eprintln!("[avc-hw] flush: {error}"); }
        state.sent = 0;
    }
    original(handle, ip, op)
}

impl Instance {
    fn remember_parameter_sets(&mut self, data: &[u8]) {
        for (kind, bytes) in nal_units(data) {
            if !matches!(kind, 7 | 8) { continue; }
            if kind == 7 { self.last_sps = bytes.to_vec(); }
            // A stream that repeats its parameter sets must not grow this.
            if self.prefix.len() + bytes.len() <= 64 * 1024 { self.prefix.extend_from_slice(bytes); }
        }
    }
}

/// If `fd` is an output block that names a held hardware frame, show that frame
/// on `surface_id` and return how it went. `None`: an ordinary pixel buffer.
pub fn present_tagged(fd: c_int, surface_id: u32) -> Option<Result<(), String>> {
    if ORIGINAL.load(Ordering::Acquire) == 0 { return None; }
    let mut tag = [0u8; TAG_SIZE];
    let read = unsafe { libc::pread(fd, tag.as_mut_ptr().cast(), TAG_SIZE, 0) };
    if read != TAG_SIZE as isize || tag[..16] != TAG_MAGIC { return None; }
    let token = u64::from_le_bytes(tag[16..24].try_into().unwrap());
    let frame_id = u64::from_le_bytes(tag[24..32].try_into().unwrap());
    let Some(session) = sessions().lock().unwrap().get(&token).cloned() else {
        return Some(Err("the decoder that produced this frame is closed".into()));
    };
    Some(match session.request(&Message::VideoHeldPresent { frame_id, surface_id }) {
        Ok(Message::Ack) => Ok(()),
        Ok(reply) => Err(format!("present: {reply:?}")),
        Err(error) => Err(error),
    })
}

struct Located { base: usize, build_id: Option<[u8; 16]> }
unsafe extern "C" fn find_build_id(info: *mut libc::dl_phdr_info, _size: usize, data: *mut c_void) -> c_int {
    let located = &mut *(data as *mut Located);
    let info = &*info;
    if info.dlpi_addr as usize != located.base { return 0; }
    for index in 0..info.dlpi_phnum as usize {
        let header = &*info.dlpi_phdr.add(index);
        if header.p_type != libc::PT_NOTE { continue; }
        let mut cursor = (located.base + header.p_vaddr as usize) as *const u8;
        let end = cursor.add(header.p_memsz as usize);
        // Elf64_Nhdr: namesz, descsz, type; name and desc padded to 4 bytes.
        while cursor.add(12) <= end {
            let name_size = cursor.cast::<u32>().read_unaligned() as usize;
            let desc_size = cursor.add(4).cast::<u32>().read_unaligned() as usize;
            let kind = cursor.add(8).cast::<u32>().read_unaligned();
            let name = cursor.add(12);
            let desc = name.add((name_size + 3) & !3);
            if desc.add(desc_size) > end { break; }
            if kind == 3 && name_size == 4 && std::slice::from_raw_parts(name, 4) == b"GNU\0" && desc_size == 16 {
                let mut id = [0u8; 16];
                std::ptr::copy_nonoverlapping(desc, id.as_mut_ptr(), 16);
                located.build_id = Some(id);
                return 1;
            }
            cursor = desc.add((desc_size + 3) & !3);
        }
    }
    1
}

/// Load the software AVC component and hook its decoder entry. Called once,
/// before the Codec2 store can create a component, so no decoder thread exists.
pub unsafe fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if policy() == Policy::Off { return; }
        if let Err(reason) = try_install() {
            eprintln!("[avc-hw] hardware H.264 not hooked: {reason}");
        } else if trace() {
            eprintln!("[avc-hw] ih264d_api_function hooked");
        }
    });
}
unsafe fn try_install() -> Result<(), String> {
    let library = libc::dlopen(b"libcodec2_soft_avcdec.so\0".as_ptr() as *const c_char, libc::RTLD_NOW | libc::RTLD_GLOBAL);
    if library.is_null() { return Err("libcodec2_soft_avcdec.so is not loadable".into()); }
    let factory = libc::dlsym(library, b"CreateCodec2Factory\0".as_ptr() as *const c_char);
    let mut info: libc::Dl_info = std::mem::zeroed();
    if factory.is_null() || libc::dladdr(factory, &mut info) == 0 || info.dli_fbase.is_null() {
        return Err("cannot locate the component library in memory".into());
    }
    let base = info.dli_fbase as usize;
    let mut located = Located { base, build_id: None };
    libc::dl_iterate_phdr(Some(find_build_id), (&mut located as *mut Located).cast());
    match located.build_id {
        Some(id) if id == BUILD_ID => {}
        Some(id) => return Err(format!("unknown build {}", id.iter().map(|b| format!("{b:02x}")).collect::<String>())),
        None => return Err("the library has no GNU build id".into()),
    }
    let entry = (base + API_OFFSET) as *mut u8;
    if std::slice::from_raw_parts(entry, PROLOGUE.len()) != PROLOGUE {
        return Err("the decoder entry does not start with the expected prologue".into());
    }
    // The CFI jump-table slot for the same function must point at it.
    let slot = (base + CFI_JUMP_OFFSET) as *const u8;
    let displacement = slot.add(1).cast::<i32>().read_unaligned() as isize;
    if *slot != 0xe9 || (slot as isize + 5 + displacement) as usize != entry as usize {
        return Err("the CFI jump slot does not lead to the decoder entry".into());
    }
    // Trampoline: the prologue, then an absolute jump to the instruction after it.
    let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
    let trampoline = libc::mmap(std::ptr::null_mut(), page, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
    if trampoline == libc::MAP_FAILED { return Err("cannot allocate the trampoline".into()); }
    let code = trampoline as *mut u8;
    std::ptr::copy_nonoverlapping(PROLOGUE.as_ptr(), code, PROLOGUE.len());
    write_absolute_jump(code.add(PROLOGUE.len()), entry as usize + PROLOGUE.len());
    if libc::mprotect(trampoline, page, libc::PROT_READ | libc::PROT_EXEC) != 0 {
        libc::munmap(trampoline, page);
        return Err("cannot make the trampoline executable".into());
    }
    // Patch the entry: 14 bytes of jump, one of padding, replacing 15 of prologue.
    let first = entry as usize & !(page - 1);
    let span = ((entry as usize + PROLOGUE.len() + page - 1) & !(page - 1)) - first;
    if libc::mprotect(first as *mut c_void, span, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC) != 0 {
        libc::munmap(trampoline, page);
        return Err("cannot make the decoder entry writable".into());
    }
    ORIGINAL.store(trampoline as usize, Ordering::Release);
    write_absolute_jump(entry, hook as usize);
    *entry.add(14) = 0x90;
    libc::mprotect(first as *mut c_void, span, libc::PROT_READ | libc::PROT_EXEC);
    Ok(())
}
/// `jmp *0(%rip)` followed by the 64-bit target: 14 bytes, no register used.
unsafe fn write_absolute_jump(at: *mut u8, target: usize) {
    let mut bytes = [0xff, 0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    bytes[6..].copy_from_slice(&(target as u64).to_le_bytes());
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_units_are_split_on_three_and_four_byte_start_codes() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let units = nal_units(&stream);
        assert_eq!(units.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(), [7, 8, 5]);
        assert_eq!(units[0].1, &stream[0..7]);
        assert_eq!(units[1].1, &stream[7..12]);
        assert_eq!(units[2].1, &stream[12..]);
        assert_eq!(sequence_parameter_set(&stream), Some(&stream[0..7]));
        assert!(has_picture(&stream));
        assert!(!has_picture(&stream[..12]));
        assert!(nal_units(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn parameter_sets_are_kept_for_the_hardware_decoder_but_bounded() {
        let mut state = Instance::default();
        let headers = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x06, 5];
        state.remember_parameter_sets(&headers);
        assert_eq!(state.prefix, &headers[..12], "SEI is not a parameter set");
        assert_eq!(state.last_sps, &headers[..6]);
        for _ in 0..20_000 { state.remember_parameter_sets(&headers); }
        assert!(state.prefix.len() <= 64 * 1024);
    }

    #[test]
    fn decode_output_offsets_match_the_pinned_binary() {
        // Eleven u32 fields and the color format (48 bytes); iv_yuv_buf_t as
        // size, three pointers, nine u32 (72, padded to 8); SEI flags padded to
        // 8; then e4_fld_type, u4_ts, u4_disp_buf_id, i4_reorder_depth,
        // i4_display_index.
        let field_type = 48 + 72 + 8;
        assert_eq!(OP_TS, field_type + 4);
        assert_eq!(OP_REORDER_DEPTH, field_type + 12);
        assert_eq!(OP_MIN_SIZE, field_type + 20);
        assert_eq!(IP_MIN_SIZE0, 32 + 64 * 8);
    }
}
