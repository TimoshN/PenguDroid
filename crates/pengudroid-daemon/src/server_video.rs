//! Connection-local hardware decoding. Native driver objects never leave the
//! serving thread; only exported descriptors and release flags cross into graphics.
use super::{Daemon, Message};
use pengudroid_hostgpu::video::{Decoder, HardwareFrame, ReceiveStatus, SendStatus};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

const MAX_HELD: usize = 4;

struct HeldFrame {
    frame: HardwareFrame,
    released: Option<Arc<AtomicBool>>,
}
struct Session {
    decoder: Decoder,
    held: Vec<HeldFrame>,
    surface: u32,
    decoded: u64,
    presented: u64,
    last_pts_us: Option<i64>,
    eos_sent: bool,
    eos_received: bool,
}
/// Decoded frames the guest has not shown (yet). Its Codec2 pipeline holds at
/// most output delay + rendering depth + smoothness buffers, about sixteen.
const MAX_HELD_UNSHOWN: usize = 24;
/// Shown frames kept until the compositor releases their buffer. It never
/// holds more than three per surface; the margin covers a replaced layer.
const MAX_HELD_SHOWN: usize = 8;

struct HeldEntry {
    id: u64,
    frame: HardwareFrame,
    announced: bool,
    /// Set once shown; true after `wl_buffer.release`.
    released: Option<Arc<AtomicBool>>,
}
/// A decoder whose frames wait for the guest to name them (`VideoHeldPresent`).
struct HeldSession {
    decoder: Decoder,
    frames: std::collections::VecDeque<HeldEntry>,
    next_id: u64,
    eos_sent: bool,
    eos_received: bool,
}
#[derive(Default)]
pub(super) struct VideoConnection { session: Option<Session>, held: Option<HeldSession> }

pub(super) fn is_request(message: &Message) -> bool {
    matches!(message, Message::VideoDecoderOpen { .. } | Message::VideoPacket { .. }
        | Message::VideoDecoderFlush | Message::VideoDecoderClose | Message::VideoDecoderPoll
        | Message::VideoHeldOpen { .. } | Message::VideoHeldDecode { .. } | Message::VideoHeldPresent { .. })
}
impl VideoConnection {
    pub(super) fn handle(&mut self, daemon: &Daemon, pid: i32, message: Message) -> Message {
        match self.request(daemon, pid, message) {
            Ok(reply) => reply,
            Err(message) => Message::Error { code: -22, message },
        }
    }
    fn request(&mut self, daemon: &Daemon, pid: i32, message: Message) -> Result<Message, String> {
        if let Message::VideoHeldOpen { codec, device, extra } = message {
            if self.session.is_some() || self.held.is_some() {
                return Err("close the existing hardware decoder first".into());
            }
            let decoder = Decoder::open(&codec, std::path::Path::new(&device), &extra)?;
            self.held = Some(HeldSession { decoder, frames: Default::default(), next_id: 1,
                eos_sent: false, eos_received: false });
            return Ok(Message::VideoDecoderReady);
        }
        if let Some(held) = self.held.as_mut() {
            return match message {
                Message::VideoHeldDecode { pts_us, data, end_of_stream } => held.decode(daemon, pts_us, &data, end_of_stream),
                Message::VideoHeldPresent { frame_id, surface_id } => held.present(daemon, pid, frame_id, surface_id),
                Message::VideoDecoderFlush => {
                    held.decoder.flush();
                    // The guest discards its undisplayed buffers on a flush, so
                    // nothing can name these frames any more. Shown ones stay
                    // until the compositor lets go of them.
                    held.frames.retain(|entry| entry.released.is_some());
                    held.eos_sent = false; held.eos_received = false;
                    Ok(Message::Ack)
                }
                Message::VideoDecoderClose => { self.held = None; Ok(Message::Ack) }
                _ => Err("this connection has a held hardware decoder".into()),
            };
        }
        if matches!(message, Message::VideoHeldDecode { .. } | Message::VideoHeldPresent { .. }) {
            return Err("no held hardware decoder on this connection".into());
        }
        if let Message::VideoDecoderOpen { surface_id, codec, device, extra } = message {
            if self.session.is_some() { return Err("close the existing hardware decoder first".into()); }
            {
                let graphics = daemon.graphics.lock().unwrap();
                let graphics = graphics.as_ref().ok_or("graphics unavailable")?;
                if !graphics.contains_surface(surface_id)
                    || daemon.surface_owner.lock().unwrap().get(&surface_id) != Some(&pid) {
                    return Err("hardware decoder surface is unavailable or belongs to another process".into());
                }
            }
            let decoder = Decoder::open(&codec, std::path::Path::new(&device), &extra)?;
            self.session = Some(Session { decoder, held: Vec::new(), surface: surface_id,
                decoded: 0, presented: 0, last_pts_us: None, eos_sent: false, eos_received: false });
            return Ok(Message::VideoDecoderReady);
        }
        if message == Message::VideoDecoderClose {
            if let Some(session) = self.session.take() {
                // Detach the import. Closing the decoder stops pool reuse, and
                // exported DMA-BUF references keep compositor storage alive.
                if let Some(graphics) = daemon.graphics.lock().unwrap().as_mut() {
                    graphics.present(session.surface);
                }
                drop(session);
            }
            return Ok(Message::Ack);
        }
        let session = self.session.as_mut().ok_or("no hardware decoder on this connection")?;
        match message {
            Message::VideoDecoderFlush => {
                session.decoder.flush();
                // Submitted frames stay retained until actual release. Pending
                // frames from the previous stream generation can be discarded.
                session.held.retain(|frame| frame.released.is_some());
                session.decoded = 0; session.presented = 0; session.last_pts_us = None;
                session.eos_sent = false; session.eos_received = false;
                Ok(session.status(false))
            }
            Message::VideoPacket { pts_us, data, end_of_stream } => {
                if (end_of_stream && !data.is_empty()) || (!end_of_stream && data.is_empty()) {
                    return Err("send one nonempty access unit or an empty EOS; use Poll to drain".into());
                }
                if session.eos_sent { return Err("decoder is draining; flush before sending another packet".into()); }
                session.pump(daemon, pid)?;
                if session.blocked() { return Ok(session.status(false)); }
                let accepted = if end_of_stream {
                    session.decoder.end_of_stream()? == SendStatus::Accepted
                } else { session.decoder.send(&data, pts_us)? == SendStatus::Accepted };
                if accepted && end_of_stream { session.eos_sent = true; }
                session.drain(daemon, pid)?;
                Ok(session.status(accepted))
            }
            Message::VideoDecoderPoll => {
                session.drain(daemon, pid)?;
                Ok(session.status(false))
            }
            _ => Err("unexpected hardware decoder command".into()),
        }
    }
}
impl Session {
    fn blocked(&self) -> bool {
        self.held.len() >= MAX_HELD || self.held.iter().any(|frame| frame.released.is_none())
    }
    fn status(&self, input_accepted: bool) -> Message {
        Message::VideoDecodeStatus { input_accepted, decoded: self.decoded,
            presented: self.presented, end_of_stream: self.eos_received,
            blocked: self.blocked(), last_pts_us: self.last_pts_us }
    }
    fn pump(&mut self, daemon: &Daemon, pid: i32) -> Result<(), String> {
        // Read releases before pruning. Only graphics holds the Wayland queue.
        let mut guard = daemon.graphics.lock().unwrap();
        let graphics = guard.as_mut().ok_or("graphics unavailable")?;
        graphics.poll();
        if !graphics.contains_surface(self.surface)
            || daemon.surface_owner.lock().unwrap().get(&self.surface) != Some(&pid) {
            return Err("hardware decoder surface was retired".into());
        }
        self.held.retain(|frame| !frame.released.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)));
        for held in &mut self.held {
            if held.released.is_some() { continue; }
            let export = held.frame.export()?;
            let Some(released) = graphics.present_video_dmabuf(self.surface, export)? else { break };
            held.released = Some(released);
            self.presented += 1;
            self.last_pts_us = Some(held.frame.pts_us());
        }
        Ok(())
    }
    fn drain(&mut self, daemon: &Daemon, pid: i32) -> Result<(), String> {
        self.pump(daemon, pid)?;
        while !self.blocked() && !self.eos_received {
            match self.decoder.receive()? {
                ReceiveStatus::Frame(frame) => {
                    self.decoded += 1;
                    self.held.push(HeldFrame { frame, released: None });
                    self.pump(daemon, pid)?;
                }
                ReceiveStatus::NeedsInput => break,
                ReceiveStatus::EndOfStream => self.eos_received = true,
                ReceiveStatus::FramesHeld => break,
            }
        }
        Ok(())
    }
}
impl HeldSession {
    fn decode(&mut self, daemon: &Daemon, pts_us: i64, data: &[u8], end_of_stream: bool) -> Result<Message, String> {
        if end_of_stream && !data.is_empty() { return Err("an EOS packet carries no data".into()); }
        self.prune(daemon);
        let mut accepted = false;
        if !data.is_empty() {
            if self.eos_sent { return Err("decoder is draining; flush before sending another packet".into()); }
            loop {
                if self.decoder.send(data, pts_us)? == SendStatus::Accepted { break; }
                // The decoder wants its output taken first. If it then has none
                // to give, retrying would spin forever.
                if self.receive()? == 0 { return Err("hardware decoder accepts no input and yields no frame".into()); }
            }
            accepted = true;
        } else if end_of_stream && !self.eos_sent {
            self.eos_sent = self.decoder.end_of_stream()? == SendStatus::Accepted;
            accepted = self.eos_sent;
        }
        self.receive()?;
        let frame = self.frames.iter_mut().find(|entry| !entry.announced).map(|entry| {
            entry.announced = true;
            (entry.id, entry.frame.pts_us(), entry.frame.width(), entry.frame.height())
        });
        let drained = frame.is_none() && self.eos_received;
        Ok(Message::VideoHeldFrame { accepted, frame, drained })
    }
    /// Take every frame the decoder has ready; returns how many.
    fn receive(&mut self) -> Result<usize, String> {
        let mut taken = 0;
        while !self.eos_received {
            match self.decoder.receive()? {
                ReceiveStatus::Frame(frame) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.frames.push_back(HeldEntry { id, frame, announced: false, released: None });
                    taken += 1;
                    self.evict()?;
                }
                ReceiveStatus::EndOfStream => self.eos_received = true,
                ReceiveStatus::NeedsInput => break,
                ReceiveStatus::FramesHeld => {
                    // The plugin's own cap; eviction below it should prevent this.
                    return Err("hardware decoder is out of frame slots".into());
                }
            }
        }
        Ok(taken)
    }
    /// Bound what an unresponsive or buggy guest can make the GPU keep.
    fn evict(&mut self) -> Result<(), String> {
        while self.frames.iter().filter(|entry| entry.released.is_none()).count() > MAX_HELD_UNSHOWN {
            // Only a frame the guest already knows about may go: an unannounced
            // one is a picture its pipeline is still waiting for.
            let Some(index) = self.frames.iter().position(|entry| entry.released.is_none() && entry.announced) else {
                return Err("guest is not collecting decoded frames".into());
            };
            self.frames.remove(index);
        }
        while self.frames.iter().filter(|entry| entry.released.is_some()).count() > MAX_HELD_SHOWN {
            let index = self.frames.iter().position(|entry| entry.released.is_some()).unwrap();
            self.frames.remove(index);
        }
        Ok(())
    }
    /// Forget shown frames whose buffer the compositor has released.
    fn prune(&mut self, daemon: &Daemon) {
        if let Some(graphics) = daemon.graphics.lock().unwrap().as_mut() { graphics.poll(); }
        self.frames.retain(|entry| !entry.released.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)));
    }
    fn present(&mut self, daemon: &Daemon, pid: i32, frame_id: u64, surface: u32) -> Result<Message, String> {
        self.prune(daemon);
        let entry = self.frames.iter_mut().find(|entry| entry.id == frame_id)
            .ok_or("held frame is gone (evicted or flushed)")?;
        if entry.released.is_some() { return Ok(Message::Ack); } // shown already
        let mut guard = daemon.graphics.lock().unwrap();
        let graphics = guard.as_mut().ok_or("graphics unavailable")?;
        if !graphics.contains_surface(surface)
            || daemon.surface_owner.lock().unwrap().get(&surface) != Some(&pid) {
            return Err("video surface is unavailable or belongs to another process".into());
        }
        // None: the surface already has three buffers the compositor has not
        // released, so this picture is skipped, as the software path does.
        if let Some(released) = graphics.present_video_dmabuf(surface, entry.frame.export()?)? {
            entry.released = Some(released);
        }
        self.evict()?;
        Ok(Message::Ack)
    }
}
