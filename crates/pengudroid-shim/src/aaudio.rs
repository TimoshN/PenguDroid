//! AAudio output on the host's PipeWire/ALSA sink.
//!
//! There is no AudioFlinger here, so neither the platform's x86_64 libaaudio
//! nor a translated ARM64 one can open a stream: the ARM64 copy goes further
//! and aborts the process opening /dev/binder from translated code (measured on
//! Genshin Impact). This implements the NDK `AAudio*` API (r29 surface, every
//! symbol the native-bridge proxy binds) on the same `pw-cat` sink the OpenSL ES
//! player uses:
//!
//! * output streams work in both models: a data callback driven from a
//!   dedicated thread, or blocking `AAudioStream_write`;
//! * PCM I16 and FLOAT (converted to S16), any rate/channel count `pw-cat`
//!   accepts; unspecified values become 48 kHz, stereo, FLOAT;
//! * writing is paced to real time with at most ~60 ms queued ahead, so the
//!   callback cadence and `getTimestamp` track what is actually audible;
//! * input (capture) streams are refused with `AAUDIO_ERROR_UNAVAILABLE`, which
//!   is what a device without a microphone reports.
//!
//! Reference: ndk/include/aaudio/AAudio.h (constants are the published ABI).
#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const AAUDIO_OK: i32 = 0;
const AAUDIO_ERROR_ILLEGAL_ARGUMENT: i32 = -898;
const AAUDIO_ERROR_INVALID_STATE: i32 = -895;
const AAUDIO_ERROR_UNIMPLEMENTED: i32 = -890;
const AAUDIO_ERROR_UNAVAILABLE: i32 = -889;
const AAUDIO_ERROR_NULL: i32 = -886;
const AAUDIO_ERROR_TIMEOUT: i32 = -885;
const AAUDIO_ERROR_INVALID_FORMAT: i32 = -883;

const DIRECTION_OUTPUT: i32 = 0;
const FORMAT_UNSPECIFIED: i32 = 0;
const FORMAT_I16: i32 = 1;
const FORMAT_FLOAT: i32 = 2;
const SHARING_SHARED: i32 = 1;
const PERFORMANCE_NONE: i32 = 10;
const USAGE_MEDIA: i32 = 1;
const CONTENT_MUSIC: i32 = 2;
const CALLBACK_CONTINUE: i32 = 0;

const STATE_OPEN: i32 = 2;
const STATE_STARTED: i32 = 4;
const STATE_PAUSED: i32 = 6;
const STATE_FLUSHED: i32 = 8;
const STATE_STOPPED: i32 = 10;
const STATE_CLOSED: i32 = 12;
const STATE_DISCONNECTED: i32 = 13;

const DEFAULT_RATE: i32 = 48_000;
const DEFAULT_CHANNELS: i32 = 2;
/// 10 ms at 48 kHz: the burst size a shared legacy stream typically reports.
const BURST_FRAMES: i32 = 480;
/// How far ahead of real time the sink may be fed.
const AHEAD: Duration = Duration::from_millis(60);

type DataCallback = Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, i32) -> i32>;
type ErrorCallback = Option<unsafe extern "C" fn(*mut c_void, *mut c_void, i32)>;

/// `AAudioStreamBuilder`: plain settings, copied into the stream on open.
#[derive(Clone, Copy)]
struct Builder {
    direction: i32,
    rate: i32,
    channels: i32,
    format: i32,
    performance: i32,
    usage: i32,
    content: i32,
    session: i32,
    capacity: i32,
    frames_per_callback: i32,
    data: DataCallback,
    data_user: usize,
    error: ErrorCallback,
    error_user: usize,
}
impl Default for Builder {
    fn default() -> Self {
        Builder { direction: DIRECTION_OUTPUT, rate: 0, channels: 0, format: FORMAT_UNSPECIFIED,
            performance: PERFORMANCE_NONE, usage: USAGE_MEDIA, content: CONTENT_MUSIC, session: -1,
            capacity: 0, frames_per_callback: 0, data: None, data_user: 0, error: None, error_user: 0 }
    }
}

/// `AAudioStream`. Shared with the callback thread, which never outlives
/// `AAudioStream_close` (close joins it before freeing).
struct Stream {
    settings: Builder,
    rate: i32,
    channels: i32,
    format: i32,
    buffer_size: AtomicI32,
    capacity: i32,
    burst: i32,
    state: AtomicI32,
    written: AtomicI64,
    /// Wall-clock origin of the frames written since the last start.
    clock: Mutex<Option<(Instant, i64)>>,
    sink: Mutex<Option<crate::Sink>>,
    running: AtomicBool,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Stream {
    fn frame_bytes(&self) -> usize {
        self.channels as usize * if self.format == FORMAT_I16 { 2 } else { 4 }
    }
    /// Convert `frames` of the stream's format at `data` to S16 and queue them
    /// on the sink, first sleeping until no more than `AHEAD` is queued.
    unsafe fn push(&self, data: *const c_void, frames: i32, deadline: Option<Instant>) -> i32 {
        if frames <= 0 { return 0; }
        let samples = frames as usize * self.channels as usize;
        let mut pcm = Vec::with_capacity(samples * 2);
        if self.format == FORMAT_I16 {
            pcm.extend_from_slice(std::slice::from_raw_parts(data as *const u8, samples * 2));
        } else {
            for sample in std::slice::from_raw_parts(data as *const f32, samples) {
                let value = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                pcm.extend_from_slice(&value.to_le_bytes());
            }
        }
        // Pace to real time.
        let written = self.written.load(Ordering::Acquire);
        let origin = *self.clock.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert((Instant::now(), written));
        let due = origin.0 + Duration::from_nanos(((written - origin.1).max(0) as u64) * 1_000_000_000 / self.rate as u64);
        let wake = due.checked_sub(AHEAD).unwrap_or(due);
        let now = Instant::now();
        if wake > now {
            if let Some(deadline) = deadline {
                if wake > deadline { return 0; }
            }
            std::thread::sleep(wake - now);
        } else if now > due + Duration::from_millis(200) {
            // Fell behind (the app stalled): restart the clock rather than
            // bursting everything that was missed.
            *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = Some((now, written));
        }
        let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if sink.is_none() {
            *sink = crate::spawn_sink(self.rate as u32, self.channels as u32);
        }
        if let Some(stdin) = sink.as_mut().and_then(|s| s.stdin.as_mut()) {
            // A player that exited is not fatal to the app; the frames are lost.
            let _ = stdin.write_all(&pcm);
        }
        self.written.fetch_add(frames as i64, Ordering::AcqRel);
        frames
    }
    fn stop_thread(&self) {
        self.running.store(false, Ordering::Release);
        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            // A callback may call requestStop on its own stream: never join self.
            if handle.thread().id() != std::thread::current().id() { let _ = handle.join(); }
        }
    }
}

struct StreamPtr(*const Stream);
unsafe impl Send for StreamPtr {}

unsafe fn stream<'a>(pointer: *mut c_void) -> Option<&'a Stream> {
    (pointer as *const Stream).as_ref()
}
unsafe fn builder<'a>(pointer: *mut c_void) -> Option<&'a mut Builder> {
    (pointer as *mut Builder).as_mut()
}

#[no_mangle]
pub unsafe extern "C" fn AAudio_createStreamBuilder(out: *mut *mut c_void) -> i32 {
    if out.is_null() { return AAUDIO_ERROR_NULL; }
    *out = Box::into_raw(Box::<Builder>::default()).cast();
    AAUDIO_OK
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_delete(b: *mut c_void) -> i32 {
    if b.is_null() { return AAUDIO_ERROR_NULL; }
    drop(Box::from_raw(b as *mut Builder));
    AAUDIO_OK
}

macro_rules! builder_setter {
    ($name:ident, $field:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(b: *mut c_void, value: i32) {
            if let Some(b) = builder(b) { b.$field = value; }
        }
    };
}
builder_setter!(AAudioStreamBuilder_setDirection, direction);
builder_setter!(AAudioStreamBuilder_setSampleRate, rate);
builder_setter!(AAudioStreamBuilder_setChannelCount, channels);
builder_setter!(AAudioStreamBuilder_setSamplesPerFrame, channels);
builder_setter!(AAudioStreamBuilder_setFormat, format);
builder_setter!(AAudioStreamBuilder_setPerformanceMode, performance);
builder_setter!(AAudioStreamBuilder_setUsage, usage);
builder_setter!(AAudioStreamBuilder_setContentType, content);
builder_setter!(AAudioStreamBuilder_setSessionId, session);
builder_setter!(AAudioStreamBuilder_setBufferCapacityInFrames, capacity);
builder_setter!(AAudioStreamBuilder_setFramesPerDataCallback, frames_per_callback);

/// Settings with no effect on a shared host stream; accepted and ignored.
macro_rules! builder_ignored {
    ($($name:ident($ty:ty)),* $(,)?) => {
        $(#[no_mangle] pub unsafe extern "C" fn $name(_b: *mut c_void, _value: $ty) {})*
    };
}
builder_ignored!(AAudioStreamBuilder_setDeviceId(i32), AAudioStreamBuilder_setSharingMode(i32),
    AAudioStreamBuilder_setInputPreset(i32), AAudioStreamBuilder_setAllowedCapturePolicy(i32),
    AAudioStreamBuilder_setPrivacySensitive(bool), AAudioStreamBuilder_setSpatializationBehavior(i32),
    AAudioStreamBuilder_setIsContentSpatialized(bool), AAudioStreamBuilder_setChannelMask(u32),
    AAudioStreamBuilder_setPackageName(*const c_char), AAudioStreamBuilder_setAttributionTag(*const c_char));

#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_setDataCallback(b: *mut c_void, callback: DataCallback, user: *mut c_void) {
    if let Some(b) = builder(b) { b.data = callback; b.data_user = user as usize; }
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_setErrorCallback(b: *mut c_void, callback: ErrorCallback, user: *mut c_void) {
    if let Some(b) = builder(b) { b.error = callback; b.error_user = user as usize; }
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_setPartialDataCallback(_b: *mut c_void, _callback: *mut c_void, _user: *mut c_void) -> i32 {
    AAUDIO_ERROR_UNIMPLEMENTED
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_setPresentationEndCallback(_b: *mut c_void, _callback: *mut c_void, _user: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_setRoutingChangedCallback(_b: *mut c_void, _callback: *mut c_void, _user: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_addTag(_b: *mut c_void, _tag: *const c_char) -> i32 { AAUDIO_OK }
#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_clearTags(_b: *mut c_void) {}

#[no_mangle]
pub unsafe extern "C" fn AAudioStreamBuilder_openStream(b: *mut c_void, out: *mut *mut c_void) -> i32 {
    let (Some(settings), false) = (builder(b), out.is_null()) else { return AAUDIO_ERROR_NULL };
    if settings.direction != DIRECTION_OUTPUT { return AAUDIO_ERROR_UNAVAILABLE; }
    let format = match settings.format {
        FORMAT_UNSPECIFIED => FORMAT_FLOAT,
        FORMAT_I16 | FORMAT_FLOAT => settings.format,
        _ => return AAUDIO_ERROR_INVALID_FORMAT,
    };
    let rate = if settings.rate > 0 { settings.rate } else { DEFAULT_RATE };
    let channels = if settings.channels > 0 { settings.channels } else { DEFAULT_CHANNELS };
    if !(8_000..=192_000).contains(&rate) || !(1..=8).contains(&channels) { return AAUDIO_ERROR_ILLEGAL_ARGUMENT; }
    let burst = if settings.frames_per_callback > 0 { settings.frames_per_callback } else { BURST_FRAMES * rate / DEFAULT_RATE };
    let capacity = settings.capacity.max(burst * 4);
    let stream = Arc::new(Stream {
        settings: *settings, rate, channels, format,
        buffer_size: AtomicI32::new(burst * 2), capacity, burst,
        state: AtomicI32::new(STATE_OPEN), written: AtomicI64::new(0),
        clock: Mutex::new(None), sink: Mutex::new(None),
        running: AtomicBool::new(false), thread: Mutex::new(None),
    });
    *out = Arc::into_raw(stream) as *mut c_void;
    AAUDIO_OK
}

#[no_mangle]
pub unsafe extern "C" fn AAudioStream_requestStart(s: *mut c_void) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    match stream.state.load(Ordering::Acquire) {
        STATE_STARTED => return AAUDIO_OK,
        STATE_CLOSED | STATE_DISCONNECTED => return AAUDIO_ERROR_INVALID_STATE,
        _ => {}
    }
    *stream.clock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    stream.state.store(STATE_STARTED, Ordering::Release);
    if let Some(callback) = stream.settings.data {
        stream.running.store(true, Ordering::Release);
        let pointer = StreamPtr(stream as *const Stream);
        let user = stream.settings.data_user;
        let handle = std::thread::Builder::new().name("AAudio".into()).spawn(move || {
            let pointer = pointer;
            let stream = &*pointer.0;
            let mut buffer = vec![0u8; stream.burst as usize * stream.frame_bytes()];
            while stream.running.load(Ordering::Acquire) {
                buffer.fill(0);
                let result = callback(pointer.0 as *mut c_void, user as *mut c_void, buffer.as_mut_ptr().cast(), stream.burst);
                stream.push(buffer.as_ptr().cast(), stream.burst, None);
                if result != CALLBACK_CONTINUE {
                    stream.state.store(STATE_STOPPED, Ordering::Release);
                    break;
                }
            }
        });
        match handle {
            Ok(handle) => *stream.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle),
            Err(_) => { stream.state.store(STATE_OPEN, Ordering::Release); return AAUDIO_ERROR_UNAVAILABLE; }
        }
    }
    AAUDIO_OK
}

unsafe fn transition(s: *mut c_void, state: i32) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    if matches!(stream.state.load(Ordering::Acquire), STATE_CLOSED | STATE_DISCONNECTED) {
        return AAUDIO_ERROR_INVALID_STATE;
    }
    stream.stop_thread();
    stream.state.store(state, Ordering::Release);
    AAUDIO_OK
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_requestPause(s: *mut c_void) -> i32 { transition(s, STATE_PAUSED) }
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_requestStop(s: *mut c_void) -> i32 { transition(s, STATE_STOPPED) }
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_requestFlush(s: *mut c_void) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    if stream.state.load(Ordering::Acquire) != STATE_PAUSED { return AAUDIO_ERROR_INVALID_STATE; }
    stream.state.store(STATE_FLUSHED, Ordering::Release);
    AAUDIO_OK
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_release(s: *mut c_void) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    stream.stop_thread();
    *stream.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
    AAUDIO_OK
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_close(s: *mut c_void) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    stream.stop_thread();
    stream.state.store(STATE_CLOSED, Ordering::Release);
    drop(Arc::from_raw(s as *const Stream));
    AAUDIO_OK
}

#[no_mangle]
pub unsafe extern "C" fn AAudioStream_write(s: *mut c_void, data: *const c_void, frames: i32, timeout_ns: i64) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    if data.is_null() || frames < 0 { return AAUDIO_ERROR_ILLEGAL_ARGUMENT; }
    if stream.settings.data.is_some() { return AAUDIO_ERROR_INVALID_STATE; }
    match stream.state.load(Ordering::Acquire) {
        STATE_CLOSED | STATE_DISCONNECTED => return AAUDIO_ERROR_INVALID_STATE,
        _ => {}
    }
    let deadline = Instant::now() + Duration::from_nanos(timeout_ns.max(0) as u64);
    stream.push(data, frames, Some(deadline))
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_read(_s: *mut c_void, _data: *mut c_void, _frames: i32, _timeout_ns: i64) -> i32 {
    AAUDIO_ERROR_UNAVAILABLE
}

#[no_mangle]
pub unsafe extern "C" fn AAudioStream_waitForStateChange(s: *mut c_void, input: i32, next: *mut i32, timeout_ns: i64) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    let deadline = Instant::now() + Duration::from_nanos(timeout_ns.max(0) as u64);
    loop {
        let state = stream.state.load(Ordering::Acquire);
        if state != input {
            if !next.is_null() { *next = state; }
            return AAUDIO_OK;
        }
        if Instant::now() >= deadline {
            if !next.is_null() { *next = state; }
            return AAUDIO_ERROR_TIMEOUT;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[no_mangle]
pub unsafe extern "C" fn AAudioStream_getTimestamp(s: *mut c_void, clock: i32, position: *mut i64, time: *mut i64) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    if stream.state.load(Ordering::Acquire) != STATE_STARTED { return AAUDIO_ERROR_INVALID_STATE; }
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    if libc::clock_gettime(clock, &mut now) != 0 { return AAUDIO_ERROR_ILLEGAL_ARGUMENT; }
    // Everything queued ahead of real time has not been heard yet.
    let ahead = (AHEAD.as_nanos() as i64 * stream.rate as i64) / 1_000_000_000;
    if !position.is_null() { *position = (stream.written.load(Ordering::Acquire) - ahead).max(0); }
    if !time.is_null() { *time = i64::from(now.tv_sec) * 1_000_000_000 + i64::from(now.tv_nsec); }
    AAUDIO_OK
}

macro_rules! stream_getter {
    ($name:ident -> $ty:ty, $stream:ident => $value:expr, $missing:expr) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(s: *mut c_void) -> $ty {
            match stream(s) { Some($stream) => $value, None => $missing }
        }
    };
}
stream_getter!(AAudioStream_getSampleRate -> i32, s => s.rate, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getHardwareSampleRate -> i32, s => s.rate, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getChannelCount -> i32, s => s.channels, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getSamplesPerFrame -> i32, s => s.channels, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getHardwareChannelCount -> i32, s => s.channels, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getFormat -> i32, s => s.format, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getHardwareFormat -> i32, s => { let _ = s; FORMAT_I16 }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getState -> i32, s => s.state.load(Ordering::Acquire), AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getBufferSizeInFrames -> i32, s => s.buffer_size.load(Ordering::Acquire), AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getBufferCapacityInFrames -> i32, s => s.capacity, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getFramesPerBurst -> i32, s => s.burst, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getFramesPerDataCallback -> i32, s => s.settings.frames_per_callback, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getFramesWritten -> i64, s => s.written.load(Ordering::Acquire), AAUDIO_ERROR_NULL as i64);
stream_getter!(AAudioStream_getFramesRead -> i64, s => s.written.load(Ordering::Acquire), AAUDIO_ERROR_NULL as i64);
stream_getter!(AAudioStream_getXRunCount -> i32, s => { let _ = s; 0 }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getPerformanceMode -> i32, s => s.settings.performance, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getSharingMode -> i32, s => { let _ = s; SHARING_SHARED }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getDirection -> i32, s => s.settings.direction, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getDeviceId -> i32, s => { let _ = s; 0 }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getSessionId -> i32, s => s.settings.session, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getUsage -> i32, s => s.settings.usage, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getContentType -> i32, s => s.settings.content, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getInputPreset -> i32, s => { let _ = s; 0 }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getAllowedCapturePolicy -> i32, s => { let _ = s; 1 }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getSpatializationBehavior -> i32, s => { let _ = s; 2 }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getChannelMask -> u32, s => if s.channels == 1 { 1 } else { 3 }, 0);
stream_getter!(AAudioStream_getOffloadDelay -> i32, s => { let _ = s; AAUDIO_ERROR_UNIMPLEMENTED }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_getOffloadPadding -> i32, s => { let _ = s; AAUDIO_ERROR_UNIMPLEMENTED }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_setOffloadEndOfStream -> i32, s => { let _ = s; AAUDIO_ERROR_UNIMPLEMENTED }, AAUDIO_ERROR_NULL);
stream_getter!(AAudioStream_isMMapUsed -> bool, s => { let _ = s; false }, false);
stream_getter!(AAudioStream_isPrivacySensitive -> bool, s => { let _ = s; false }, false);
stream_getter!(AAudioStream_isContentSpatialized -> bool, s => { let _ = s; false }, false);

#[no_mangle]
pub unsafe extern "C" fn AAudioStream_setBufferSizeInFrames(s: *mut c_void, frames: i32) -> i32 {
    let Some(stream) = stream(s) else { return AAUDIO_ERROR_NULL };
    let size = frames.clamp(stream.burst, stream.capacity);
    stream.buffer_size.store(size, Ordering::Release);
    size
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_getDeviceIds(s: *mut c_void, ids: *mut i32, count: *mut i32) -> i32 {
    if stream(s).is_none() || count.is_null() { return AAUDIO_ERROR_NULL; }
    if *count >= 1 && !ids.is_null() { *ids = 0; }
    *count = 1;
    AAUDIO_OK
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_setOffloadDelayPadding(_s: *mut c_void, _delay: i32, _padding: i32) -> i32 { AAUDIO_ERROR_UNIMPLEMENTED }
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_flushFromFrame(_s: *mut c_void, _accuracy: u32, _position: *mut i64) -> i32 { AAUDIO_ERROR_UNIMPLEMENTED }
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_getPlaybackParameters(_s: *mut c_void, _params: *mut c_void) -> i32 { AAUDIO_ERROR_UNIMPLEMENTED }
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_setPlaybackParameters(_s: *mut c_void, _params: *const c_void) -> i32 { AAUDIO_ERROR_UNIMPLEMENTED }
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_obtainTags(_s: *mut c_void, tags: *mut *mut *mut c_char) -> i32 {
    if !tags.is_null() { *tags = std::ptr::null_mut(); }
    0
}
#[no_mangle]
pub unsafe extern "C" fn AAudioStream_destroyTags(_s: *mut c_void, _tags: *mut *mut c_char) {}

#[no_mangle]
pub extern "C" fn AAudio_getMMapPolicy() -> i32 { 1 } // AAUDIO_POLICY_NEVER
#[no_mangle]
pub extern "C" fn AAudio_setMMapPolicy(_policy: i32) -> i32 { AAUDIO_OK }
#[no_mangle]
pub extern "C" fn AAudio_getPlatformMMapPolicy(_device: u32, _direction: i32) -> i32 { 1 }
#[no_mangle]
pub extern "C" fn AAudio_getPlatformMMapExclusivePolicy(_device: u32, _direction: i32) -> i32 { 1 }
#[no_mangle]
pub extern "C" fn AAudio_getFlushFromFrameSupport(_builder: *const c_void) -> u32 { 0 }

#[no_mangle]
pub extern "C" fn AAudio_convertResultToText(result: i32) -> *const c_char {
    let text: &'static [u8] = match result {
        AAUDIO_OK => b"AAUDIO_OK\0",
        -899 => b"AAUDIO_ERROR_DISCONNECTED\0",
        AAUDIO_ERROR_ILLEGAL_ARGUMENT => b"AAUDIO_ERROR_ILLEGAL_ARGUMENT\0",
        AAUDIO_ERROR_INVALID_STATE => b"AAUDIO_ERROR_INVALID_STATE\0",
        AAUDIO_ERROR_UNIMPLEMENTED => b"AAUDIO_ERROR_UNIMPLEMENTED\0",
        AAUDIO_ERROR_UNAVAILABLE => b"AAUDIO_ERROR_UNAVAILABLE\0",
        AAUDIO_ERROR_NULL => b"AAUDIO_ERROR_NULL\0",
        AAUDIO_ERROR_TIMEOUT => b"AAUDIO_ERROR_TIMEOUT\0",
        AAUDIO_ERROR_INVALID_FORMAT => b"AAUDIO_ERROR_INVALID_FORMAT\0",
        _ => b"Unrecognized AAudio error.\0",
    };
    text.as_ptr().cast()
}
#[no_mangle]
pub extern "C" fn AAudio_convertStreamStateToText(state: i32) -> *const c_char {
    const NAMES: [&[u8]; 14] = [b"AAUDIO_STREAM_STATE_UNINITIALIZED\0", b"AAUDIO_STREAM_STATE_UNKNOWN\0",
        b"AAUDIO_STREAM_STATE_OPEN\0", b"AAUDIO_STREAM_STATE_STARTING\0", b"AAUDIO_STREAM_STATE_STARTED\0",
        b"AAUDIO_STREAM_STATE_PAUSING\0", b"AAUDIO_STREAM_STATE_PAUSED\0", b"AAUDIO_STREAM_STATE_FLUSHING\0",
        b"AAUDIO_STREAM_STATE_FLUSHED\0", b"AAUDIO_STREAM_STATE_STOPPING\0", b"AAUDIO_STREAM_STATE_STOPPED\0",
        b"AAUDIO_STREAM_STATE_CLOSING\0", b"AAUDIO_STREAM_STATE_CLOSED\0", b"AAUDIO_STREAM_STATE_DISCONNECTED\0"];
    NAMES.get(state as usize).copied().unwrap_or(b"Unrecognized AAudio state.\0").as_ptr().cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_stream_lifecycle_defaults_and_refused_input() {
        unsafe {
            let mut builder = std::ptr::null_mut();
            assert_eq!(AAudio_createStreamBuilder(&mut builder), AAUDIO_OK);
            AAudioStreamBuilder_setDirection(builder, 1);
            let mut stream = std::ptr::null_mut();
            assert_eq!(AAudioStreamBuilder_openStream(builder, &mut stream), AAUDIO_ERROR_UNAVAILABLE);
            AAudioStreamBuilder_setDirection(builder, DIRECTION_OUTPUT);
            AAudioStreamBuilder_setFormat(builder, 7);
            assert_eq!(AAudioStreamBuilder_openStream(builder, &mut stream), AAUDIO_ERROR_INVALID_FORMAT);
            AAudioStreamBuilder_setFormat(builder, FORMAT_UNSPECIFIED);
            assert_eq!(AAudioStreamBuilder_openStream(builder, &mut stream), AAUDIO_OK);
            assert_eq!(AAudioStreamBuilder_delete(builder), AAUDIO_OK);
            assert_eq!((AAudioStream_getSampleRate(stream), AAudioStream_getChannelCount(stream),
                AAudioStream_getFormat(stream)), (48_000, 2, FORMAT_FLOAT));
            assert_eq!(AAudioStream_getState(stream), STATE_OPEN);
            let mut next = 0;
            assert_eq!(AAudioStream_waitForStateChange(stream, STATE_OPEN, &mut next, 1_000_000), AAUDIO_ERROR_TIMEOUT);
            assert_eq!(AAudioStream_requestPause(stream), AAUDIO_OK);
            assert_eq!(AAudioStream_requestFlush(stream), AAUDIO_OK);
            assert_eq!(AAudioStream_getState(stream), STATE_FLUSHED);
            assert_eq!(AAudioStream_close(stream), AAUDIO_OK);
            assert_eq!(std::ffi::CStr::from_ptr(AAudio_convertStreamStateToText(STATE_STARTED)).to_str().unwrap(),
                "AAUDIO_STREAM_STATE_STARTED");
        }
    }
}
