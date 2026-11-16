//! Reentrant Java object monitors shared by DEX and JNI callers.
use crate::ObjId;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};
#[derive(Default)]
struct Waiter {
    notified: Mutex<bool>,
    changed: Condvar,
}
#[derive(Default)]
struct State {
    owner: Option<ThreadId>,
    depth: usize,
    waiters: VecDeque<Arc<Waiter>>,
}
#[derive(Default)]
struct Monitor {
    state: Mutex<State>,
    changed: Condvar,
}
fn monitor(id: ObjId) -> Arc<Monitor> {
    static ALL: OnceLock<Mutex<HashMap<ObjId, Arc<Monitor>>>> = OnceLock::new();
    ALL.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(id)
        .or_default()
        .clone()
}
pub(crate) fn enter(id: ObjId) {
    let monitor = monitor(id);
    let owner = std::thread::current().id();
    let mut s = monitor.state.lock().unwrap();
    while s.owner.is_some() && s.owner != Some(owner) {
        s = monitor.changed.wait(s).unwrap();
    }
    s.owner = Some(owner);
    s.depth += 1;
}
pub(crate) fn exit(id: ObjId) -> Result<(), String> {
    let monitor = monitor(id);
    let mut s = monitor.state.lock().unwrap();
    if s.owner != Some(std::thread::current().id()) {
        return Err("IllegalMonitorStateException: current thread does not own monitor".into());
    }
    s.depth -= 1;
    if s.depth == 0 {
        s.owner = None;
        monitor.changed.notify_all();
    }
    Ok(())
}
pub(crate) struct Guard(ObjId);
impl Guard {
    pub fn new(id: ObjId) -> Self {
        enter(id);
        Self(id)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = exit(self.0);
    }
}
pub(crate) fn notify(id: ObjId, all: bool) -> Result<(), String> {
    let monitor = monitor(id);
    let mut s = monitor.state.lock().unwrap();
    if s.owner != Some(std::thread::current().id()) {
        return Err("IllegalMonitorStateException: notify without ownership".into());
    }
    while let Some(waiter) = s.waiters.pop_front() {
        *waiter.notified.lock().unwrap() = true;
        waiter.changed.notify_one();
        if !all {
            break;
        }
    }
    Ok(())
}
pub(crate) fn wait(
    id: ObjId,
    millis: i64,
    nanos: i32,
    interrupted: impl Fn() -> bool,
) -> Result<(), String> {
    if millis < 0 || !(0..=999999).contains(&nanos) {
        return Err("IllegalArgumentException: invalid wait timeout".into());
    }
    let monitor = monitor(id);
    let owner = std::thread::current().id();
    let waiter = Arc::new(Waiter::default());
    let depth;
    {
        let mut s = monitor.state.lock().unwrap();
        if s.owner != Some(owner) {
            return Err("IllegalMonitorStateException: wait without ownership".into());
        }
        if interrupted() {
            return Err("InterruptedException".into());
        }
        depth = s.depth;
        s.owner = None;
        s.depth = 0;
        s.waiters.push_back(waiter.clone());
        monitor.changed.notify_all();
    }
    let deadline = (millis > 0 || nanos > 0).then(|| {
        Instant::now() + Duration::from_millis(millis as u64) + Duration::from_nanos(nanos as u64)
    });
    let mut notified = waiter.notified.lock().unwrap();
    let mut was_interrupted = false;
    while !*notified {
        if interrupted() {
            was_interrupted = true;
            break;
        }
        let left = deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(50));
        if left.is_zero() {
            break;
        }
        notified = waiter
            .changed
            .wait_timeout(notified, left.min(Duration::from_millis(50)))
            .unwrap()
            .0;
    }
    drop(notified);
    let mut s = monitor.state.lock().unwrap();
    s.waiters.retain(|w| !Arc::ptr_eq(w, &waiter));
    while s.owner.is_some() {
        s = monitor.changed.wait(s).unwrap();
    }
    s.owner = Some(owner);
    s.depth = depth;
    if was_interrupted {
        Err("InterruptedException".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    #[test]
    fn wait_releases_recursion_and_restores_ownership_after_notify() {
        let id = crate::jni_host::jni_state()
            .lock()
            .unwrap()
            .heap
            .new_instance("Ljava/lang/Object;");
        assert!(exit(id).is_err());
        assert!(notify(id, false).is_err());
        let (ready, rx) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            enter(id);
            enter(id);
            ready.send(()).unwrap();
            wait(id, 2000, 0, || false).unwrap();
            exit(id).unwrap();
            exit(id).unwrap();
            assert!(exit(id).is_err());
            done.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let guard = Guard::new(id);
        assert!(result.try_recv().is_err());
        notify(id, false).unwrap();
        drop(guard);
        result.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn reentrant_monitor_excludes_other_os_threads_until_final_exit() {
        let id = crate::jni_host::jni_state()
            .lock()
            .unwrap()
            .heap
            .new_instance("Ljava/lang/Object;");
        enter(id);
        enter(id);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = Guard::new(id);
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(25)).is_err());
        exit(id).unwrap();
        assert!(rx.try_recv().is_err());
        exit(id).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }
}
