//! Host threads and Android-style, thread-owned message queues. No VM or bridge
//! closure is sent across threads: workers own an interpreter and share immutable
//! DEX definitions plus synchronized heap/static/queue state.
use crate::jni_host::jni_state;
use crate::{HeapObject, ObjId, Value, Vm};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;

pub fn uptime_millis() -> i64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec
        .saturating_mul(1000)
        .saturating_add(time.tv_nsec / 1_000_000)
}
fn field(object: ObjId, key: &str) -> Value {
    match jni_state().lock().unwrap().heap.get(object) {
        Some(HeapObject::Instance { fields, .. }) => {
            fields.get(key).cloned().unwrap_or(Value::Ref(None))
        }
        _ => Value::Ref(None),
    }
}
fn put(object: ObjId, key: &str, value: Value) {
    if let Some(HeapObject::Instance { fields, .. }) =
        jni_state().lock().unwrap().heap.get_mut(object)
    {
        fields.insert(key.into(), value);
    }
}
fn object(class: &str) -> ObjId {
    jni_state().lock().unwrap().heap.new_instance(class)
}
fn class(object: ObjId) -> String {
    jni_state()
        .lock()
        .unwrap()
        .heap
        .class_name(object)
        .map(str::to_owned)
        .unwrap_or_default()
}
fn int(args: &[Value], i: usize) -> i32 {
    args.get(i).and_then(|v| v.as_int().ok()).unwrap_or(0)
}
fn long(args: &[Value], i: usize) -> i64 {
    args.get(i).and_then(|v| v.as_long().ok()).unwrap_or(0)
}
fn arg(args: &[Value], i: usize) -> Result<ObjId, String> {
    args.get(i)
        .and_then(Value::as_ref)
        .ok_or_else(|| "null threading receiver/argument".into())
}

#[derive(Default)]
struct ThreadState {
    started: bool,
    finished: bool,
    looper: Option<ObjId>,
    interrupted: bool,
    error: Option<String>,
}
#[derive(Default)]
struct JavaThread {
    state: Mutex<ThreadState>,
    changed: Condvar,
}
#[derive(Clone)]
struct Entry {
    message: ObjId,
    handler: ObjId,
    runnable: Option<ObjId>,
    token: Option<ObjId>,
    what: i32,
    when: i64,
    sequence: i64,
}
#[derive(Default)]
struct QueueState {
    entries: Vec<Entry>,
    quitting: bool,
    safe_cutoff: Option<i64>,
    next: i64,
    front: i64,
}
struct Queue {
    owner: ObjId,
    in_use: Arc<Mutex<HashSet<ObjId>>>,
    state: Mutex<QueueState>,
    changed: Condvar,
}
impl Queue {
    fn new(owner: ObjId, in_use: Arc<Mutex<HashSet<ObjId>>>) -> Self {
        Self {
            owner,
            in_use,
            state: Mutex::new(QueueState::default()),
            changed: Condvar::new(),
        }
    }
    fn enqueue(&self, mut entry: Entry, front: bool) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        if state.quitting {
            return Ok(false);
        }
        if !self.in_use.lock().unwrap().insert(entry.message) {
            return Err("IllegalStateException: Message is already in use".into());
        }
        if front {
            state.front -= 1;
            entry.sequence = state.front;
            entry.when = 0;
        } else {
            entry.sequence = state.next;
            state.next += 1;
        }
        state.entries.push(entry);
        self.changed.notify_one();
        Ok(true)
    }
    fn next(&self, block: bool) -> Option<Entry> {
        let mut state = self.state.lock().unwrap();
        loop {
            let best = state
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.when, e.sequence))
                .map(|(i, e)| (i, e.when));
            if let Some((index, when)) = best {
                let now = uptime_millis();
                if when <= now {
                    return Some(state.entries.remove(index));
                }
                if !block {
                    return None;
                }
                let (guard, _) = self
                    .changed
                    .wait_timeout(state, Duration::from_millis((when - now) as u64))
                    .unwrap();
                state = guard;
            } else {
                if state.quitting || !block {
                    return None;
                }
                state = self.changed.wait(state).unwrap();
            }
        }
    }
    fn quit(&self, safely: bool) {
        let mut state = self.state.lock().unwrap();
        state.quitting = true;
        let cutoff = uptime_millis();
        state.safe_cutoff = safely.then_some(cutoff);
        let removed: Vec<_> = state
            .entries
            .iter()
            .filter(|e| !safely || e.when > cutoff)
            .map(|e| e.message)
            .collect();
        state.entries.retain(|e| safely && e.when <= cutoff);
        for id in removed {
            self.in_use.lock().unwrap().remove(&id);
        }
        self.changed.notify_all();
    }
}
#[derive(Default)]
struct RuntimeState {
    threads: HashMap<ObjId, Arc<JavaThread>>,
    attached: HashMap<ThreadId, ObjId>,
    loopers: HashMap<ObjId, Arc<Queue>>,
    thread_loopers: HashMap<ObjId, ObjId>,
    main: Option<ObjId>,
}
enum InitState {
    Running(ThreadId),
    Done,
    Failed(String),
}
#[derive(Default)]
struct Gate {
    count: Mutex<i64>,
    changed: Condvar,
}
#[derive(Default)]
pub(crate) struct ThreadRuntime {
    state: Mutex<RuntimeState>,
    pub statics: Mutex<HashMap<String, Value>>,
    pub missing_methods: Mutex<BTreeMap<String, u64>>,
    messages_in_use: Arc<Mutex<HashSet<ObjId>>>,
    native_loads: Mutex<HashMap<ObjId, (String, std::sync::mpsc::SyncSender<Result<(), String>>)>>,
    native_wraps: Mutex<HashMap<ObjId, (String, String, String, usize, std::sync::mpsc::SyncSender<Result<usize, String>>)>>,
    initialization: Mutex<HashMap<String, InitState>>,
    init_changed: Condvar,
    gates: Mutex<HashMap<ObjId, Arc<Gate>>>,
}
impl ThreadRuntime {
    pub(crate) fn begin_init(&self, class: &str) -> Result<bool, String> {
        let owner = std::thread::current().id();
        let mut state = self.initialization.lock().unwrap();
        loop {
            match state.get(class) {
                Some(InitState::Done) => return Ok(false),
                Some(InitState::Running(current)) if *current == owner => return Ok(false),
                Some(InitState::Running(_)) => state = self.init_changed.wait(state).unwrap(),
                Some(InitState::Failed(error)) => return Err(error.clone()),
                None => {
                    state.insert(class.into(), InitState::Running(owner));
                    return Ok(true);
                }
            }
        }
    }
    pub(crate) fn finish_init(&self, class: &str, result: &Result<(), String>) {
        self.initialization.lock().unwrap().insert(
            class.into(),
            match result {
                Ok(()) => InitState::Done,
                Err(e) => InitState::Failed(e.clone()),
            },
        );
        self.init_changed.notify_all();
    }
    fn interrupted(&self, clear: bool) -> bool {
        let record = self.record(self.current());
        let mut state = record.state.lock().unwrap();
        let value = state.interrupted;
        if clear {
            state.interrupted = false;
        }
        value
    }
    fn record(&self, id: ObjId) -> Arc<JavaThread> {
        self.state
            .lock()
            .unwrap()
            .threads
            .entry(id)
            .or_default()
            .clone()
    }
    fn bind(&self, id: ObjId) {
        self.state
            .lock()
            .unwrap()
            .attached
            .insert(std::thread::current().id(), id);
    }
    fn current(&self) -> ObjId {
        let os = std::thread::current().id();
        if let Some(id) = self.state.lock().unwrap().attached.get(&os).copied() {
            return id;
        }
        let id = object("Ljava/lang/Thread;");
        self.bind(id);
        let record = self.record(id);
        record.state.lock().unwrap().started = true;
        id
    }
    fn my_looper(&self) -> Option<ObjId> {
        let current = self.current();
        self.state
            .lock()
            .unwrap()
            .thread_loopers
            .get(&current)
            .copied()
    }
    fn prepare(&self) -> Result<ObjId, String> {
        let thread = self.current();
        let looper = object("Landroid/os/Looper;");
        let mut state = self.state.lock().unwrap();
        if state.thread_loopers.contains_key(&thread) {
            return Err("RuntimeException: Only one Looper may be created per thread".into());
        }
        state.loopers.insert(
            looper,
            Arc::new(Queue::new(thread, self.messages_in_use.clone())),
        );
        state.thread_loopers.insert(thread, looper);
        drop(state);
        let record = self.record(thread);
        record.state.lock().unwrap().looper = Some(looper);
        record.changed.notify_all();
        Ok(looper)
    }
    fn queue(&self, looper: ObjId) -> Result<Arc<Queue>, String> {
        self.state
            .lock()
            .unwrap()
            .loopers
            .get(&looper)
            .cloned()
            .ok_or_else(|| "Invalid Looper".into())
    }
}

#[derive(Clone)]
pub(crate) struct ThreadTemplate {
    dexes: Vec<Arc<crate::DexFile>>,
    classes: Arc<HashMap<String, crate::vm::IndexedClass>>,
    statics: HashMap<String, Value>,
    initialized: HashSet<String>,
    native_dirs: Vec<std::path::PathBuf>,
    wrapped: Arc<Mutex<HashMap<(String, String, String), usize>>>,
    runtime: Arc<ThreadRuntime>,
    bridge: bool,
    trace: bool,
}
static ATTACH_TEMPLATE: OnceLock<Mutex<Option<ThreadTemplate>>> = OnceLock::new();
pub(crate) fn publish_template(template: ThreadTemplate) {
    *ATTACH_TEMPLATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some(template);
}
thread_local! { static ATTACHED_VM: std::cell::RefCell<Option<Box<Vm>>> = const { std::cell::RefCell::new(None) }; }
pub(crate) fn attach_native_thread() {
    if crate::jni_host::has_vm_invoker() {
        return;
    }
    let template = ATTACH_TEMPLATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone();
    if let Some(template) = template {
        let mut vm = Box::new(template.instantiate());
        vm.threads.current();
        let ptr = &mut *vm as *mut Vm as usize;
        ATTACHED_VM.with(|slot| *slot.borrow_mut() = Some(vm));
        crate::jni_host::set_vm_invoker(ptr, Vm::vm_jni_invoke_trampoline);
    }
}
pub(crate) fn detach_native_thread() -> bool {
    ATTACHED_VM
        .try_with(|slot| {
            if slot.borrow().is_none() && crate::jni_host::has_vm_invoker() {
                return false;
            }
            crate::jni_host::clear_vm_invoker();
            if let Some(vm) = slot.borrow_mut().take() {
                let id = vm.threads.current();
                vm.threads
                    .state
                    .lock()
                    .unwrap()
                    .attached
                    .remove(&std::thread::current().id());
                let record = vm.threads.record(id);
                record.state.lock().unwrap().finished = true;
                record.changed.notify_all();
            }
            true
        })
        .unwrap_or_else(|_| {
            crate::jni_host::clear_vm_invoker();
            true
        })
}
impl ThreadTemplate {
    fn instantiate(self) -> Vm {
        let mut vm = Vm::new();
        vm.dexes = self.dexes;
        vm.classes = self.classes;
        vm.static_fields = self.statics;
        vm.clinit_done = self.initialized;
        vm.native_dirs = self.native_dirs;
        vm.wrapped_natives = self.wrapped;
        vm.threads = self.runtime;
        vm.needs_bridge = self.bridge;
        vm.owns_runtime = false;
        vm.trace = self.trace;
        vm
    }
}

impl Vm {
    pub(crate) fn thread_template(&mut self) -> Result<ThreadTemplate, String> {
        // Resolve declared native entry points while the owner can still use
        // its bridge resolver. Worker interpreters never borrow that closure.
        if let Some(resolver) = self.symbol_resolver.as_mut() {
            let registered = jni_state().lock().unwrap().registry.natives.clone();
            for dex in &self.dexes {
                for class in &dex.classes {
                    for encoded in class.direct_methods.iter().chain(&class.virtual_methods) {
                        if encoded.access_flags & 0x100 == 0 {
                            continue;
                        }
                        let method = &dex.methods[encoded.method_idx];
                        let key = (
                            class.name.clone(),
                            method.name.clone(),
                            method.descriptor.clone(),
                        );
                        if registered.contains_key(&key) {
                            continue;
                        }
                        let symbol = crate::vm::jni_mangle_name(&class.name, &method.name);
                        let shorty = crate::vm::descriptor_to_shorty(&method.descriptor);
                        if let Some(pointer) = resolver(&symbol, &shorty) {
                            crate::jni_host::register_native(&key.0, &key.1, &key.2, pointer);
                            self.wrapped_natives.lock().unwrap().insert(key, pointer);
                        }
                    }
                }
            }
        }
        // Wrap every registered ARM64 method on the bridge's owning thread before
        // any worker can invoke it. Raw ARM64 addresses never run as host code.
        if let Some(wrapper) = self.pointer_wrapper.as_mut() {
            let entries = jni_state().lock().unwrap().registry.natives.clone();
            for (key, pointer) in entries {
                if self.wrapped_natives.lock().unwrap().get(&key) != Some(&pointer) {
                    let shorty = crate::vm::descriptor_to_shorty(&key.2);
                    let wrapped = wrapper(pointer, &shorty)
                        .ok_or_else(|| format!("cannot prepare worker JNI method {key:?}"))?;
                    crate::jni_host::register_native(&key.0, &key.1, &key.2, wrapped);
                    self.wrapped_natives.lock().unwrap().insert(key, wrapped);
                }
            }
        }
        Ok(ThreadTemplate {
            dexes: self.dexes.clone(),
            classes: self.classes.clone(),
            statics: self.static_fields.clone(),
            initialized: self.clinit_done.clone(),
            native_dirs: self.native_dirs.clone(),
            wrapped: self.wrapped_natives.clone(),
            runtime: self.threads.clone(),
            bridge: self.needs_bridge,
            trace: self.trace,
        })
    }
    pub(crate) fn post_dialog_click(&self, listener: ObjId, dialog: ObjId, which: i32) {
        let Some(looper) = self.threads.state.lock().unwrap().main else {
            return;
        };
        let task = object("Lpengu/internal/DialogClickTask;");
        put(task, "listener", Value::Ref(Some(listener)));
        put(task, "dialog", Value::Ref(Some(dialog)));
        put(task, "which", Value::Int(which));
        let handler = object("Landroid/os/Handler;");
        put(handler, "mLooper", Value::Ref(Some(looper)));
        let message = object("Landroid/os/Message;");
        put(message, "callback", Value::Ref(Some(task)));
        let _ = self.enqueue_message(handler, message, uptime_millis().saturating_add(10), false);
    }
    pub(crate) fn post_idle_handler(&self, looper: ObjId, idle_handler: ObjId, delay_ms: i64) {
        let task = object("Lpengu/internal/IdleHandlerTask;");
        put(task, "handler", Value::Ref(Some(idle_handler)));
        put(task, "looper", Value::Ref(Some(looper)));
        let handler = object("Landroid/os/Handler;");
        put(handler, "mLooper", Value::Ref(Some(looper)));
        let message = object("Landroid/os/Message;");
        put(message, "callback", Value::Ref(Some(task)));
        let _ = self.enqueue_message(
            handler,
            message,
            uptime_millis().saturating_add(delay_ms),
            false,
        );
    }
    pub(crate) fn request_native_load(&self, library: &str) -> Result<(), String> {
        let looper = self
            .threads
            .state
            .lock()
            .unwrap()
            .main
            .ok_or("native load requires an owning main Looper")?;
        if self.threads.queue(looper)?.owner == self.threads.current() {
            return Err("native loader owner is not installed on this interpreter".into());
        }
        let task = object("Lpengu/internal/NativeLoadTask;");
        let (reply, receive) = std::sync::mpsc::sync_channel(1);
        self.threads
            .native_loads
            .lock()
            .unwrap()
            .insert(task, (library.into(), reply));
        let handler = object("Landroid/os/Handler;");
        put(handler, "mLooper", Value::Ref(Some(looper)));
        let message = object("Landroid/os/Message;");
        put(message, "callback", Value::Ref(Some(task)));
        let queued = self.enqueue_message(handler, message, uptime_millis(), false);
        if !matches!(queued, Ok(Value::Int(1))) {
            self.threads.native_loads.lock().unwrap().remove(&task);
            return Err("native loader main queue is unavailable".into());
        }
        match receive.recv_timeout(Duration::from_secs(120)) {
            Ok(result) => result,
            Err(error) => {
                self.threads.native_loads.lock().unwrap().remove(&task);
                Err(format!("native loader owner did not complete: {error}"))
            }
        }
    }
    pub(crate) fn request_native_wrap(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        fn_ptr: usize,
    ) -> Result<usize, String> {
        let looper = self
            .threads
            .state
            .lock()
            .unwrap()
            .main
            .ok_or("native wrap requires an owning main Looper")?;
        if self.threads.queue(looper)?.owner == self.threads.current() {
            return Err("pointer wrapper owner is not installed on this interpreter".into());
        }
        let task = object("Lpengu/internal/NativeWrapTask;");
        let (reply, receive) = std::sync::mpsc::sync_channel(1);
        self.threads.native_wraps.lock().unwrap().insert(
            task,
            (class_name.into(), method_name.into(), descriptor.into(), fn_ptr, reply),
        );
        let handler = object("Landroid/os/Handler;");
        put(handler, "mLooper", Value::Ref(Some(looper)));
        let message = object("Landroid/os/Message;");
        put(message, "callback", Value::Ref(Some(task)));
        let queued = self.enqueue_message(handler, message, uptime_millis(), false);
        if !matches!(queued, Ok(Value::Int(1))) {
            self.threads.native_wraps.lock().unwrap().remove(&task);
            return Err("native wrapper main queue is unavailable".into());
        }
        match receive.recv_timeout(Duration::from_secs(120)) {
            Ok(result) => result,
            Err(error) => {
                self.threads.native_wraps.lock().unwrap().remove(&task);
                Err(format!("native wrapper owner did not complete: {error}"))
            }
        }
    }
    pub fn worker_failures(&self) -> BTreeMap<ObjId, String> {
        let records: Vec<_> = self
            .threads
            .state
            .lock()
            .unwrap()
            .threads
            .iter()
            .map(|(id, record)| (*id, record.clone()))
            .collect();
        records
            .into_iter()
            .filter_map(|(id, record)| record.state.lock().unwrap().error.clone().map(|e| (id, e)))
            .collect()
    }
    pub fn prepare_main_thread(&mut self) -> Result<(), String> {
        let looper = match self.threads.my_looper() {
            Some(l) => l,
            None => self.threads.prepare()?,
        };
        let mut state = self.threads.state.lock().unwrap();
        if let Some(main) = state.main {
            if main != looper {
                return Err("main Looper already belongs to another thread".into());
            }
        }
        state.main = Some(looper);
        Ok(())
    }
    pub fn run_main_loop(&mut self) -> Result<(), String> {
        let looper = self
            .threads
            .state
            .lock()
            .unwrap()
            .main
            .ok_or("Main Looper not prepared")?;
        self.pump_queue(looper, true).map(|_| ())
    }
    pub fn shutdown_workers(&self) {
        let (queues, threads) = {
            let state = self.threads.state.lock().unwrap();
            (
                state.loopers.values().cloned().collect::<Vec<_>>(),
                state
                    .threads
                    .iter()
                    .map(|(id, r)| (*id, r.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        for queue in queues {
            queue.quit(false);
        }
        let current = self.threads.current();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        for (id, thread) in threads {
            if id == current {
                continue;
            }
            let mut state = thread
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            while state.started && !state.finished {
                let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                    break;
                };
                state = thread
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
        }
    }
    fn thread_subclass(&self, class: &str, base: &str) -> bool {
        let mut current = class.to_string();
        for _ in 0..64 {
            if current == base {
                return true;
            }
            match self
                .find_class(&current)
                .and_then(|(_, c)| c.superclass.clone())
                .or_else(|| crate::stdlib::builtin_superclass(&current).map(str::to_owned))
            {
                Some(parent) => current = parent,
                None => return false,
            }
        }
        false
    }
    fn start_java_thread(&mut self, id: ObjId) -> Result<(), String> {
        let record = self.threads.record(id);
        {
            let mut state = record.state.lock().unwrap();
            if state.started {
                return Err("IllegalThreadStateException: Thread already started".into());
            }
            state.started = true;
        }
        let template = match self.thread_template() {
            Ok(t) => t,
            Err(e) => {
                let mut s = record.state.lock().unwrap();
                s.finished = true;
                record.changed.notify_all();
                return Err(e);
            }
        };
        publish_template(template.clone());
        let name_id = field(id, "name").as_ref();
        let name = name_id
            .and_then(|id| {
                jni_state()
                    .lock()
                    .unwrap()
                    .heap
                    .as_str(id)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("Pengu-{id}"));
        let record_worker = record.clone();
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let mut vm = Box::new(template.instantiate());
            vm.threads.bind(id);
            let ptr = &mut *vm as *mut Vm as usize;
            let _guard = crate::jni_host::scoped_vm_invoker(ptr, Vm::vm_jni_invoke_trampoline);
            crate::jni_host::jni_env_ptr();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                vm.invoke_method(&class(id), "run", "()V", &[Value::Ref(Some(id))], true)
            }));
            let error = match result {
                Ok(Ok(_)) if vm.pending_exception.is_none() => None,
                Ok(Ok(_)) => Some(format!(
                    "uncaught Java exception {:?}",
                    vm.pending_exception
                )),
                Ok(Err(e)) => Some(e),
                Err(_) => Some("interpreter worker panicked".into()),
            };
            if let Some(ref error) = error {
                eprintln!("[pengu-thread] thread {id} failed: {error}");
            }
            let mut state = record_worker.state.lock().unwrap();
            state.finished = true;
            state.error = error;
            record_worker.changed.notify_all();
            drop(state);
            vm.threads
                .state
                .lock()
                .unwrap()
                .attached
                .remove(&std::thread::current().id());
        });
        if let Err(error) = spawned {
            let mut state = record.state.lock().unwrap();
            state.finished = true;
            record.changed.notify_all();
            return Err(error.to_string());
        }
        Ok(())
    }
    fn dispatch_message(&mut self, entry: &Entry) -> Result<(), String> {
        if let Some(runnable) = entry.runnable {
            self.invoke_method(
                &class(runnable),
                "run",
                "()V",
                &[Value::Ref(Some(runnable))],
                true,
            )?;
            return Ok(());
        }
        let callback = field(entry.handler, "mCallback").as_ref();
        if let Some(callback) = callback {
            if self
                .invoke_method(
                    &class(callback),
                    "handleMessage",
                    "(Landroid/os/Message;)Z",
                    &[Value::Ref(Some(callback)), Value::Ref(Some(entry.message))],
                    true,
                )?
                .as_int()?
                != 0
            {
                return Ok(());
            }
        }
        self.invoke_method(
            &class(entry.handler),
            "handleMessage",
            "(Landroid/os/Message;)V",
            &[
                Value::Ref(Some(entry.handler)),
                Value::Ref(Some(entry.message)),
            ],
            true,
        )?;
        Ok(())
    }
    fn pump_queue(&mut self, looper: ObjId, block: bool) -> Result<usize, String> {
        let _invoker = crate::jni_host::scoped_vm_invoker(
            self as *mut Vm as usize,
            Vm::vm_jni_invoke_trampoline,
        );
        let queue = self.threads.queue(looper)?;
        if queue.owner != self.threads.current() {
            return Err("Looper can only run on its owning thread".into());
        }
        let mut count = 0;
        while let Some(entry) = queue.next(block) {
            let result = self.dispatch_message(&entry);
            queue.in_use.lock().unwrap().remove(&entry.message);
            result?;
            count += 1;
            if self.pending_exception.is_some() {
                return Err("uncaught Java exception in Looper".into());
            }
        }
        Ok(count)
    }
    pub fn pump_main_messages(&mut self) -> Result<usize, String> {
        let looper = self.threads.state.lock().unwrap().main;
        if let Some(looper) = looper {
            self.pump_queue(looper, false)
        } else {
            Ok(0)
        }
    }
    fn enqueue_message(
        &self,
        handler: ObjId,
        message: ObjId,
        when: i64,
        front: bool,
    ) -> Result<Value, String> {
        let looper = field(handler, "mLooper")
            .as_ref()
            .ok_or("Handler has no Looper")?;
        put(message, "target", Value::Ref(Some(handler)));
        put(message, "when", Value::Long(when));
        let entry = Entry {
            message,
            handler,
            runnable: field(message, "callback").as_ref(),
            token: field(message, "obj").as_ref(),
            what: field(message, "what").as_int().unwrap_or(0),
            when,
            sequence: 0,
        };
        Ok(Value::Int(
            self.threads.queue(looper)?.enqueue(entry, front)? as i32,
        ))
    }
    fn invoke_gate(
        &mut self,
        class: &str,
        method: &str,
        sig: &str,
        args: &[Value],
    ) -> Result<Option<Value>, String> {
        let id = arg(args, 0)?;
        let latch = class == "Ljava/util/concurrent/CountDownLatch;";
        if method == "<init>" {
            let count = int(args, 1) as i64;
            if latch && count < 0 {
                return Err("IllegalArgumentException: negative count".into());
            }
            if sig == "(IZ)V" && int(args, 2) != 0 {
                return Err("fair Semaphore scheduling is not implemented".into());
            }
            self.threads.gates.lock().unwrap().insert(
                id,
                Arc::new(Gate {
                    count: Mutex::new(count),
                    changed: Condvar::new(),
                }),
            );
            return Ok(Some(Value::Void));
        }
        let gate = self
            .threads
            .gates
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or("synchronizer was not initialized")?;
        let mut count = gate.count.lock().unwrap();
        if method == "getCount" {
            return Ok(Some(Value::Long(*count)));
        }
        if method == "availablePermits" {
            return Ok(Some(Value::Int(*count as i32)));
        }
        if method == "countDown" {
            if *count > 0 {
                *count -= 1;
                if *count == 0 {
                    gate.changed.notify_all();
                }
            }
            return Ok(Some(Value::Void));
        }
        let permits = if sig.starts_with("(I") {
            int(args, 1) as i64
        } else {
            1
        };
        if permits < 0 {
            return Err("IllegalArgumentException: negative permits".into());
        }
        if method == "release" {
            *count = count
                .checked_add(permits)
                .filter(|v| *v <= i32::MAX as i64)
                .ok_or("Maximum permit count exceeded")?;
            gate.changed.notify_all();
            return Ok(Some(Value::Void));
        }
        if method == "drainPermits" {
            let old = *count;
            *count = 0;
            return Ok(Some(Value::Int(old as i32)));
        }
        if !matches!(
            method,
            "acquire" | "acquireUninterruptibly" | "tryAcquire" | "await"
        ) {
            return Ok(None);
        }
        let interruptible = method != "acquireUninterruptibly";
        let timed = sig.contains("TimeUnit;");
        let immediate = method == "tryAcquire" && !timed;
        let time_index = if !latch && sig.starts_with("(I") {
            2
        } else {
            1
        };
        let nanos = if timed {
            let unit = arg(args, time_index + 1)?;
            let factor = field(unit, "nanos").as_long()?;
            long(args, time_index).saturating_mul(factor).max(0)
        } else {
            0
        };
        let deadline =
            timed.then(|| std::time::Instant::now() + Duration::from_nanos(nanos as u64));
        loop {
            if interruptible && self.threads.interrupted(true) {
                return Err("InterruptedException".into());
            }
            if (latch && *count == 0) || (!latch && *count >= permits) {
                if !latch {
                    *count -= permits;
                }
                return Ok(Some(if timed || immediate {
                    Value::Int(1)
                } else {
                    Value::Void
                }));
            }
            let remaining =
                deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
            if immediate || remaining.is_some_and(|r| r.is_zero()) {
                return Ok(Some(Value::Int(0)));
            }
            count = gate
                .changed
                .wait_timeout(
                    count,
                    remaining
                        .unwrap_or(Duration::from_millis(50))
                        .min(Duration::from_millis(50)),
                )
                .unwrap()
                .0;
        }
    }
    pub(crate) fn invoke_thread_builtin(
        &mut self,
        requested: &str,
        method: &str,
        sig: &str,
        args: &[Value],
        virtual_call: bool,
    ) -> Result<Option<Value>, String> {
        if requested == "Lpengu/internal/NativeWrapTask;" && method == "run" {
            let request = self
                .threads
                .native_wraps
                .lock()
                .unwrap()
                .remove(&arg(args, 0)?);
            if let Some((cls, mname, desc, fn_ptr, reply)) = request {
                let res = if let Some(wrapper) = self.pointer_wrapper.as_mut() {
                    let shorty = crate::vm::descriptor_to_shorty(&desc);
                    if let Some(wrapped) = wrapper(fn_ptr, &shorty) {
                        crate::jni_host::register_native(&cls, &mname, &desc, wrapped);
                        self.wrapped_natives
                            .lock()
                            .unwrap()
                            .insert((cls, mname, desc), wrapped);
                        Ok(wrapped)
                    } else {
                        Ok(fn_ptr)
                    }
                } else {
                    Err("pointer_wrapper missing on owner".into())
                };
                let _ = reply.send(res);
            }
            return Ok(Some(Value::Void));
        }
        if requested == "Lpengu/internal/NativeLoadTask;" && method == "run" {
            let request = self
                .threads
                .native_loads
                .lock()
                .unwrap()
                .remove(&arg(args, 0)?);
            if let Some((library, reply)) = request {
                let result = self.load_library_by_name(&library).and_then(|()| {
                    let template = self.thread_template()?;
                    publish_template(template);
                    Ok(())
                });
                let _ = reply.send(result);
            }
            return Ok(Some(Value::Void));
        }
        if requested == "Lpengu/internal/DialogClickTask;" && method == "run" {
            let task = arg(args, 0)?;
            let lid = field(task, "listener").as_ref();
            let dlg = field(task, "dialog").as_ref();
            let which = field(task, "which").as_int().unwrap_or(-1);
            if let (Some(lid), Some(dlg)) = (lid, dlg) {
                let lcls = jni_state()
                    .lock()
                    .unwrap()
                    .heap
                    .get(lid)
                    .map(|o| o.class_name().to_string())
                    .unwrap_or_default();
                if !lcls.is_empty() {
                    let _ = self.invoke_method(
                        &lcls,
                        "onClick",
                        "(Landroid/content/DialogInterface;I)V",
                        &[Value::Ref(Some(lid)), Value::Ref(Some(dlg)), Value::Int(which)],
                        true,
                    );
                }
            }
            return Ok(Some(Value::Void));
        }
        if requested == "Lpengu/internal/IdleHandlerTask;" && method == "run" {
            let task = arg(args, 0)?;
            if let Some(ih) = field(task, "handler").as_ref() {
                let cls = jni_state()
                    .lock()
                    .unwrap()
                    .heap
                    .get(ih)
                    .map(|o| o.class_name().to_string())
                    .unwrap_or_default();
                if !cls.is_empty() {
                    let keep = matches!(
                        self.invoke_method(&cls, "queueIdle", "()Z", &[Value::Ref(Some(ih))], true),
                        Ok(Value::Int(v)) if v != 0
                    );
                    if keep {
                        let looper = field(task, "looper")
                            .as_ref()
                            .or_else(|| self.threads.my_looper())
                            .or_else(|| self.threads.state.lock().unwrap().main);
                        if let Some(looper) = looper {
                            let quitting = self
                                .threads
                                .queue(looper)
                                .map(|q| q.state.lock().unwrap().quitting)
                                .unwrap_or(true);
                            if !quitting {
                                self.post_idle_handler(looper, ih, 1);
                            }
                        }
                    }
                }
            }
            return Ok(Some(Value::Void));
        }
        if requested == "Lpengu/internal/FrameCallbackTask;" && method == "run" {
            let task = arg(args, 0)?;
            let callback = field(task, "callback")
                .as_ref()
                .ok_or("null frame callback")?;
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            unsafe {
                libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
            }
            self.invoke_method(
                &class(callback),
                "doFrame",
                "(J)V",
                &[
                    Value::Ref(Some(callback)),
                    Value::Long(
                        time.tv_sec
                            .saturating_mul(1_000_000_000)
                            .saturating_add(time.tv_nsec),
                    ),
                ],
                true,
            )?;
            return Ok(Some(Value::Void));
        }
        if requested == "Landroid/view/Choreographer;" {
            if method == "getInstance" {
                let looper = self
                    .threads
                    .my_looper()
                    .ok_or("IllegalStateException: Choreographer requires a Looper")?;
                let id = if let Some(id) = field(looper, "choreographer").as_ref() {
                    id
                } else {
                    let id = object(requested);
                    let handler = object("Landroid/os/Handler;");
                    put(handler, "mLooper", Value::Ref(Some(looper)));
                    put(id, "handler", Value::Ref(Some(handler)));
                    put(looper, "choreographer", Value::Ref(Some(id)));
                    id
                };
                return Ok(Some(Value::Ref(Some(id))));
            }
            if matches!(
                method,
                "postFrameCallback" | "postFrameCallbackDelayed" | "removeFrameCallback"
            ) {
                let owner = arg(args, 0)?;
                let callback = arg(args, 1)?;
                let handler = field(owner, "handler")
                    .as_ref()
                    .ok_or("invalid Choreographer")?;
                if method == "removeFrameCallback" {
                    self.invoke_thread_builtin(
                        "Landroid/os/Handler;",
                        "removeCallbacksAndMessages",
                        "(Ljava/lang/Object;)V",
                        &[Value::Ref(Some(handler)), Value::Ref(Some(callback))],
                        true,
                    )?;
                } else {
                    let task = object("Lpengu/internal/FrameCallbackTask;");
                    put(task, "callback", Value::Ref(Some(callback)));
                    let message = object("Landroid/os/Message;");
                    put(message, "callback", Value::Ref(Some(task)));
                    put(message, "obj", Value::Ref(Some(callback)));
                    // Monotonic software frame clock; compositor vsync is not wired yet.
                    let when = uptime_millis().saturating_add(17).saturating_add(
                        if method.ends_with("Delayed") {
                            long(args, 2).max(0)
                        } else {
                            0
                        },
                    );
                    self.enqueue_message(handler, message, when, false)?;
                }
                return Ok(Some(Value::Void));
            }
        }
        if method == "runOnUiThread"
            && sig == "(Ljava/lang/Runnable;)V"
            && self.thread_subclass(requested, "Landroid/app/Activity;")
        {
            let task = arg(args, 1)?;
            let looper = self
                .threads
                .state
                .lock()
                .unwrap()
                .main
                .ok_or("Main Looper not prepared")?;
            if self.threads.queue(looper)?.owner == self.threads.current() {
                self.invoke_method(&class(task), "run", "()V", &[Value::Ref(Some(task))], true)?;
            } else {
                let handler = object("Landroid/os/Handler;");
                put(handler, "mLooper", Value::Ref(Some(looper)));
                let message = object("Landroid/os/Message;");
                put(message, "callback", Value::Ref(Some(task)));
                self.enqueue_message(handler, message, uptime_millis(), false)?;
            }
            return Ok(Some(Value::Void));
        }
        let thread_method = matches!(
            method,
            "<init>"
                | "start"
                | "run"
                | "yield"
                | "currentThread"
                | "isAlive"
                | "getLooper"
                | "getThreadId"
                | "getId"
                | "getName"
                | "setName"
                | "setDaemon"
                | "setPriority"
                | "isDaemon"
                | "getPriority"
                | "join"
                | "sleep"
                | "interrupt"
                | "interrupted"
                | "isInterrupted"
                | "quit"
                | "quitSafely"
        );
        let is_thread = thread_method && self.thread_subclass(requested, "Ljava/lang/Thread;");
        let handler_thread =
            is_thread && self.thread_subclass(requested, "Landroid/os/HandlerThread;");
        let is_handler = matches!(
            method,
            "<init>"
                | "getLooper"
                | "sendMessage"
                | "sendMessageDelayed"
                | "sendMessageAtTime"
                | "sendMessageAtFrontOfQueue"
                | "post"
                | "postDelayed"
                | "postAtTime"
                | "postAtFrontOfQueue"
                | "sendEmptyMessage"
                | "sendEmptyMessageDelayed"
                | "sendEmptyMessageAtTime"
                | "hasMessages"
                | "hasCallbacks"
                | "removeMessages"
                | "removeCallbacks"
                | "removeCallbacksAndMessages"
                | "dispatchMessage"
                | "handleMessage"
        ) && self.thread_subclass(requested, "Landroid/os/Handler;");
        let void = Some(Value::Void);
        if matches!(
            requested,
            "Ljava/util/concurrent/Semaphore;" | "Ljava/util/concurrent/CountDownLatch;"
        ) {
            return self.invoke_gate(requested, method, sig, args);
        }
        if matches!(method, "wait" | "notify" | "notifyAll") {
            let receiver = arg(args, 0)?;
            if method == "wait" {
                let record = self.threads.record(self.threads.current());
                crate::monitors::wait(receiver, long(args, 1), int(args, 2), || {
                    let mut state = record.state.lock().unwrap();
                    let interrupted = state.interrupted;
                    state.interrupted = false;
                    interrupted
                })?;
            } else {
                crate::monitors::notify(receiver, method == "notifyAll")?;
            }
            return Ok(void);
        }
        if is_thread {
            match method {
                "<init>"
                    if requested == "Ljava/lang/Thread;"
                        || requested == "Landroid/os/HandlerThread;" =>
                {
                    let id = arg(args, 0)?;
                    if sig.starts_with("(Ljava/lang/Runnable;") {
                        put(
                            id,
                            "target",
                            args.get(1).cloned().unwrap_or(Value::Ref(None)),
                        );
                    }
                    if sig.starts_with("(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;") {
                        put(
                            id,
                            "target",
                            args.get(2).cloned().unwrap_or(Value::Ref(None)),
                        );
                    }
                    if sig.starts_with("(Ljava/lang/String;") {
                        put(id, "name", args.get(1).cloned().unwrap_or(Value::Ref(None)));
                    }
                    if sig.starts_with("(Ljava/lang/Runnable;Ljava/lang/String;") {
                        put(id, "name", args.get(2).cloned().unwrap_or(Value::Ref(None)));
                    }
                    if handler_thread {
                        put(id, "tid", Value::Int(-1));
                        if sig == "(Ljava/lang/String;I)V" {
                            put(id, "linuxPriority", Value::Int(int(args, 2)));
                        }
                    }
                    self.threads.record(id);
                    return Ok(void);
                }
                "start" if sig == "()V" => {
                    self.start_java_thread(arg(args, 0)?)?;
                    return Ok(void);
                }
                "currentThread" => return Ok(Some(Value::Ref(Some(self.threads.current())))),
                "isAlive" => {
                    let r = self.threads.record(arg(args, 0)?);
                    let s = r.state.lock().unwrap();
                    return Ok(Some(Value::Int((s.started && !s.finished) as i32)));
                }
                "getLooper" if handler_thread => {
                    let r = self.threads.record(arg(args, 0)?);
                    let mut s = r.state.lock().unwrap();
                    while s.started && !s.finished && s.looper.is_none() {
                        s = r.changed.wait(s).unwrap();
                    }
                    return Ok(Some(Value::Ref(if s.finished { None } else { s.looper })));
                }
                "getThreadId" if handler_thread => return Ok(Some(field(arg(args, 0)?, "tid"))),
                "getId" => return Ok(Some(Value::Long(arg(args, 0)? as i64))),
                "getName" => return Ok(Some(field(arg(args, 0)?, "name"))),
                "setName" => {
                    put(
                        arg(args, 0)?,
                        "name",
                        args.get(1).cloned().unwrap_or(Value::Ref(None)),
                    );
                    return Ok(void);
                }
                "setDaemon" | "setPriority" => {
                    put(
                        arg(args, 0)?,
                        if method == "setDaemon" {
                            "daemon"
                        } else {
                            "priority"
                        },
                        args.get(1).cloned().unwrap_or(Value::Int(0)),
                    );
                    return Ok(void);
                }
                "isDaemon" => {
                    return Ok(Some(Value::Int(
                        field(arg(args, 0)?, "daemon").as_int().unwrap_or(0),
                    )));
                }
                "getPriority" => {
                    return Ok(Some(Value::Int(
                        field(arg(args, 0)?, "priority").as_int().unwrap_or(5),
                    )));
                }
                "join" => {
                    let r = self.threads.record(arg(args, 0)?);
                    let current = self.threads.record(self.threads.current());
                    let millis = long(args, 1);
                    let nanos = int(args, 2);
                    if millis < 0 || !(0..=999999).contains(&nanos) {
                        return Err("IllegalArgumentException: invalid join timeout".into());
                    }
                    let duration = Duration::from_millis(millis as u64)
                        .saturating_add(Duration::from_millis((nanos > 0) as u64));
                    let deadline =
                        (!duration.is_zero()).then(|| std::time::Instant::now() + duration);
                    loop {
                        let interrupted = {
                            let mut own = current.state.lock().unwrap();
                            let old = own.interrupted;
                            own.interrupted = false;
                            old
                        };
                        if interrupted {
                            return Err("InterruptedException".into());
                        }
                        let state = r.state.lock().unwrap();
                        if !state.started || state.finished {
                            break;
                        }
                        let left = deadline
                            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
                            .unwrap_or(Duration::from_millis(50));
                        if left.is_zero() {
                            break;
                        }
                        drop(
                            r.changed
                                .wait_timeout(state, left.min(Duration::from_millis(50)))
                                .unwrap(),
                        );
                    }
                    return Ok(void);
                }
                "sleep" => {
                    let millis = long(args, 0);
                    if millis < 0 {
                        return Err(
                            "IllegalArgumentException: negative Thread.sleep duration".into()
                        );
                    }
                    let nanos = int(args, 1);
                    if !(0..=999999).contains(&nanos) {
                        return Err("IllegalArgumentException: invalid sleep nanos".into());
                    }
                    let record = self.threads.record(self.threads.current());
                    let mut state = record.state.lock().unwrap();
                    let duration = Duration::from_millis(millis as u64)
                        .saturating_add(Duration::from_nanos(nanos as u64));
                    let deadline = std::time::Instant::now() + duration;
                    loop {
                        if state.interrupted {
                            state.interrupted = false;
                            return Err("InterruptedException".into());
                        }
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        state = record.changed.wait_timeout(state, left).unwrap().0;
                    }
                    return Ok(void);
                }
                "yield" => {
                    std::thread::yield_now();
                    return Ok(void);
                }
                "interrupted" | "isInterrupted" | "interrupt" => {
                    let id = if method == "interrupted" {
                        self.threads.current()
                    } else {
                        arg(args, 0)?
                    };
                    let r = self.threads.record(id);
                    let mut s = r.state.lock().unwrap();
                    let old = s.interrupted;
                    if method == "interrupt" {
                        s.interrupted = true;
                        r.changed.notify_all();
                    } else if method == "interrupted" {
                        s.interrupted = false;
                    }
                    return Ok(if method == "interrupt" {
                        void
                    } else {
                        Some(Value::Int(old as i32))
                    });
                }
                "quit" | "quitSafely" if handler_thread => {
                    let r = self.threads.record(arg(args, 0)?);
                    let l = r.state.lock().unwrap().looper;
                    if let Some(l) = l {
                        self.threads.queue(l)?.quit(method == "quitSafely");
                    }
                    return Ok(Some(Value::Int(l.is_some() as i32)));
                }
                "run"
                    if requested == "Ljava/lang/Thread;"
                        || requested == "Landroid/os/HandlerThread;"
                        || (virtual_call && !self.has_dex_method(requested, "run", "()V")) =>
                {
                    let id = arg(args, 0)?;
                    if handler_thread {
                        put(
                            id,
                            "tid",
                            Value::Int(unsafe { libc::syscall(libc::SYS_gettid) } as i32),
                        );
                        let looper = self.threads.prepare()?;
                        if self.has_dex_method(&class(id), "onLooperPrepared", "()V") {
                            self.invoke_method(&class(id), "onLooperPrepared", "()V", args, true)?;
                        }
                        self.pump_queue(looper, true)?;
                        put(id, "tid", Value::Int(-1));
                    } else if let Some(target) = field(id, "target").as_ref() {
                        self.invoke_method(
                            &class(target),
                            "run",
                            "()V",
                            &[Value::Ref(Some(target))],
                            true,
                        )?;
                    }
                    return Ok(void);
                }
                _ => {}
            }
        }
        match (requested, method) {
            ("Landroid/os/SystemClock;", "uptimeMillis" | "elapsedRealtime") => {
                return Ok(Some(Value::Long(uptime_millis())));
            }
            ("Landroid/os/Looper;", "getMainLooper") => {
                return Ok(Some(Value::Ref(self.threads.state.lock().unwrap().main)));
            }
            ("Landroid/os/Looper;", "myLooper") => {
                return Ok(Some(Value::Ref(self.threads.my_looper())));
            }
            ("Landroid/os/Looper;", "prepare") => {
                self.threads.prepare()?;
                return Ok(void);
            }
            ("Landroid/os/Looper;", "prepareMainLooper") => {
                self.prepare_main_thread()?;
                return Ok(void);
            }
            ("Landroid/os/Looper;", "loop") => {
                let l = self
                    .threads
                    .my_looper()
                    .ok_or("No Looper; Looper.prepare() wasn't called")?;
                self.pump_queue(l, true)?;
                return Ok(void);
            }
            ("Landroid/os/Looper;", "getThread") => {
                return Ok(Some(Value::Ref(Some(
                    self.threads.queue(arg(args, 0)?)?.owner,
                ))));
            }
            ("Landroid/os/Looper;", "quit" | "quitSafely") => {
                let looper = arg(args, 0)?;
                if self.threads.state.lock().unwrap().main == Some(looper) {
                    return Err("IllegalStateException: Main thread not allowed to quit".into());
                }
                self.threads.queue(looper)?.quit(method == "quitSafely");
                return Ok(void);
            }
            ("Landroid/os/Looper;", "isCurrentThread") => {
                return Ok(Some(Value::Int(
                    (self.threads.queue(arg(args, 0)?)?.owner == self.threads.current()) as i32,
                )));
            }
            ("Landroid/os/Looper;", "myQueue") => {
                let mq = crate::jni_host::jni_state()
                    .lock()
                    .unwrap()
                    .heap
                    .new_instance("Landroid/os/MessageQueue;");
                let looper = self
                    .threads
                    .my_looper()
                    .or_else(|| self.threads.state.lock().unwrap().main);
                if let Some(looper) = looper {
                    put(mq, "looper", Value::Ref(Some(looper)));
                }
                return Ok(Some(Value::Ref(Some(mq))));
            }
            ("Landroid/os/MessageQueue;", "addIdleHandler") => {
                let mq = arg(args, 0)?;
                let looper = field(mq, "looper")
                    .as_ref()
                    .or_else(|| self.threads.my_looper())
                    .or_else(|| self.threads.state.lock().unwrap().main);
                if let (Some(looper), Some(ih)) = (looper, args.get(1).and_then(|v| v.as_ref())) {
                    self.post_idle_handler(looper, ih, 1);
                }
                return Ok(void);
            }
            ("Landroid/os/MessageQueue;", "removeIdleHandler") => {
                return Ok(void);
            }
            ("Landroid/os/Message;", "sendToTarget") => {
                let m = arg(args, 0)?;
                let h = field(m, "target").as_ref().ok_or("Message has no target")?;
                self.enqueue_message(h, m, uptime_millis(), false)?;
                return Ok(void);
            }
            _ => {}
        }
        if is_handler {
            let id = arg(args, 0)?;
            match method {
                "<init>" if requested == "Landroid/os/Handler;" => {
                    let explicit = sig.starts_with("(Landroid/os/Looper;");
                    let looper = if explicit {
                        args.get(1).and_then(Value::as_ref)
                    } else {
                        self.threads.my_looper()
                    }
                    .ok_or("Can't create handler without a prepared Looper")?;
                    self.threads.queue(looper)?;
                    put(id, "mLooper", Value::Ref(Some(looper)));
                    let cb = if explicit {
                        sig.contains("Handler$Callback;")
                            .then(|| args.get(2).cloned())
                            .flatten()
                    } else {
                        sig.contains("Handler$Callback;")
                            .then(|| args.get(1).cloned())
                            .flatten()
                    };
                    put(id, "mCallback", cb.unwrap_or(Value::Ref(None)));
                    return Ok(void);
                }
                "getLooper" => return Ok(Some(field(id, "mLooper"))),
                "sendMessage"
                | "sendMessageDelayed"
                | "sendMessageAtTime"
                | "sendMessageAtFrontOfQueue" => {
                    let when = if method == "sendMessageAtTime" {
                        long(args, 2)
                    } else {
                        uptime_millis().saturating_add(if method == "sendMessageDelayed" {
                            long(args, 2).max(0)
                        } else {
                            0
                        })
                    };
                    return self
                        .enqueue_message(id, arg(args, 1)?, when, method.ends_with("FrontOfQueue"))
                        .map(Some);
                }
                "post" | "postDelayed" | "postAtTime" | "postAtFrontOfQueue" => {
                    let message = object("Landroid/os/Message;");
                    put(
                        message,
                        "callback",
                        args.get(1).cloned().unwrap_or(Value::Ref(None)),
                    );
                    let token = sig.starts_with("(Ljava/lang/Runnable;Ljava/lang/Object;");
                    if token {
                        put(
                            message,
                            "obj",
                            args.get(2).cloned().unwrap_or(Value::Ref(None)),
                        );
                    }
                    let time = long(args, if token { 3 } else { 2 });
                    let when = if method == "postAtTime" {
                        time
                    } else {
                        uptime_millis().saturating_add(if method == "postDelayed" {
                            time.max(0)
                        } else {
                            0
                        })
                    };
                    return self
                        .enqueue_message(id, message, when, method.ends_with("FrontOfQueue"))
                        .map(Some);
                }
                "sendEmptyMessage" | "sendEmptyMessageDelayed" | "sendEmptyMessageAtTime" => {
                    let message = object("Landroid/os/Message;");
                    put(message, "what", Value::Int(int(args, 1)));
                    let time = long(args, 2);
                    let when = if method.ends_with("AtTime") {
                        time
                    } else {
                        uptime_millis().saturating_add(if method.ends_with("Delayed") {
                            time.max(0)
                        } else {
                            0
                        })
                    };
                    return self.enqueue_message(id, message, when, false).map(Some);
                }
                "hasMessages"
                | "hasCallbacks"
                | "removeMessages"
                | "removeCallbacks"
                | "removeCallbacksAndMessages" => {
                    let queue = self.threads.queue(
                        field(id, "mLooper")
                            .as_ref()
                            .ok_or("Handler has no Looper")?,
                    )?;
                    let mut state = queue.state.lock().unwrap();
                    let token = args.get(2).and_then(Value::as_ref);
                    let matching = |e: &Entry| {
                        e.handler == id
                            && match method {
                                "hasCallbacks" | "removeCallbacks" => {
                                    e.runnable == args.get(1).and_then(Value::as_ref)
                                        && (token.is_none() || e.token == token)
                                }
                                "removeCallbacksAndMessages" => args
                                    .get(1)
                                    .and_then(Value::as_ref)
                                    .is_none_or(|token| e.token == Some(token)),
                                _ => e.what == int(args, 1) && (args.len() < 3 || e.token == token),
                            }
                    };
                    let found = state.entries.iter().any(matching);
                    if method.starts_with("remove") {
                        let removed: Vec<_> = state
                            .entries
                            .iter()
                            .filter(|e| matching(e))
                            .map(|e| e.message)
                            .collect();
                        state.entries.retain(|e| !matching(e));
                        for m in removed {
                            queue.in_use.lock().unwrap().remove(&m);
                        }
                        queue.changed.notify_all();
                        return Ok(void);
                    }
                    return Ok(Some(Value::Int(found as i32)));
                }
                "dispatchMessage" => {
                    let m = arg(args, 1)?;
                    self.dispatch_message(&Entry {
                        message: m,
                        handler: id,
                        runnable: field(m, "callback").as_ref(),
                        token: None,
                        what: 0,
                        when: 0,
                        sequence: 0,
                    })?;
                    return Ok(void);
                }
                "handleMessage"
                    if !self.has_dex_method(requested, method, sig)
                        && crate::jni_host::lookup_native(requested, method, sig).is_none() =>
                {
                    return Ok(void);
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    type Task = Arc<dyn Fn() + Send + Sync>;
    fn tasks() -> &'static Mutex<HashMap<ObjId, Task>> {
        static TASKS: OnceLock<Mutex<HashMap<ObjId, Task>>> = OnceLock::new();
        TASKS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    unsafe extern "C" fn run_task(_: *mut std::ffi::c_void, receiver: *mut std::ffi::c_void) {
        let task = tasks().lock().unwrap().get(&(receiver as usize)).cloned();
        if let Some(task) = task {
            task();
        }
    }
    fn runnable(task: impl Fn() + Send + Sync + 'static) -> ObjId {
        let id = object("Lpengu/test/ThreadRunnable;");
        tasks().lock().unwrap().insert(id, Arc::new(task));
        crate::jni_host::register_native(
            "Lpengu/test/ThreadRunnable;",
            "run",
            "()V",
            run_task as *const () as usize,
        );
        id
    }
    fn invoke(vm: &mut Vm, class: &str, method: &str, sig: &str, args: &[Value]) -> Value {
        vm.invoke_method(class, method, sig, args, true).unwrap()
    }
    fn worker(vm: &mut Vm) -> (ObjId, ObjId, ObjId) {
        let thread = object("Landroid/os/HandlerThread;");
        invoke(
            vm,
            "Landroid/os/HandlerThread;",
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Ref(Some(thread)), Value::Ref(None)],
        );
        assert!(
            invoke(
                vm,
                "Landroid/os/HandlerThread;",
                "getLooper",
                "()Landroid/os/Looper;",
                &[Value::Ref(Some(thread))]
            )
            .as_ref()
            .is_none()
        );
        invoke(
            vm,
            "Landroid/os/HandlerThread;",
            "start",
            "()V",
            &[Value::Ref(Some(thread))],
        );
        let looper = invoke(
            vm,
            "Landroid/os/HandlerThread;",
            "getLooper",
            "()Landroid/os/Looper;",
            &[Value::Ref(Some(thread))],
        )
        .as_ref()
        .unwrap();
        let handler = object("Landroid/os/Handler;");
        invoke(
            vm,
            "Landroid/os/Handler;",
            "<init>",
            "(Landroid/os/Looper;)V",
            &[Value::Ref(Some(handler)), Value::Ref(Some(looper))],
        );
        (thread, looper, handler)
    }
    fn stop(vm: &mut Vm, thread: ObjId) {
        invoke(
            vm,
            "Landroid/os/HandlerThread;",
            "quitSafely",
            "()Z",
            &[Value::Ref(Some(thread))],
        );
        invoke(
            vm,
            "Ljava/lang/Thread;",
            "join",
            "(J)V",
            &[Value::Ref(Some(thread)), Value::Long(2000)],
        );
        let record = vm.threads.record(thread);
        let state = record.state.lock().unwrap();
        assert!(state.finished);
        assert!(state.error.is_none(), "{:?}", state.error);
    }
    #[test]
    fn handler_thread_runs_on_an_os_worker_and_honors_delays_removal_and_quit() {
        let mut vm = Vm::new();
        vm.prepare_main_thread().unwrap();
        let (thread, looper, handler) = worker(&mut vm);
        assert_ne!(looper, vm.threads.my_looper().unwrap());
        let (tx, rx) = mpsc::channel();
        let caller = std::thread::current().id();
        let delayed = runnable({
            let tx = tx.clone();
            move || tx.send((2, std::thread::current().id())).unwrap()
        });
        let immediate = runnable({
            let tx = tx.clone();
            move || tx.send((1, std::thread::current().id())).unwrap()
        });
        invoke(
            &mut vm,
            "Landroid/os/Handler;",
            "postDelayed",
            "(Ljava/lang/Runnable;J)Z",
            &[
                Value::Ref(Some(handler)),
                Value::Ref(Some(delayed)),
                Value::Long(150),
            ],
        );
        invoke(
            &mut vm,
            "Landroid/os/Handler;",
            "post",
            "(Ljava/lang/Runnable;)Z",
            &[Value::Ref(Some(handler)), Value::Ref(Some(immediate))],
        );
        let (first, os) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first, 1);
        assert_ne!(os, caller);
        assert!(rx.recv_timeout(Duration::from_millis(35)).is_err());
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, 2);
        let removed = runnable(move || tx.send((3, std::thread::current().id())).unwrap());
        invoke(
            &mut vm,
            "Landroid/os/Handler;",
            "postDelayed",
            "(Ljava/lang/Runnable;J)Z",
            &[
                Value::Ref(Some(handler)),
                Value::Ref(Some(removed)),
                Value::Long(10000),
            ],
        );
        assert_eq!(
            invoke(
                &mut vm,
                "Landroid/os/Handler;",
                "hasCallbacks",
                "(Ljava/lang/Runnable;)Z",
                &[Value::Ref(Some(handler)), Value::Ref(Some(removed))]
            )
            .as_int()
            .unwrap(),
            1
        );
        invoke(
            &mut vm,
            "Landroid/os/Handler;",
            "removeCallbacks",
            "(Ljava/lang/Runnable;)V",
            &[Value::Ref(Some(handler)), Value::Ref(Some(removed))],
        );
        assert_eq!(
            invoke(
                &mut vm,
                "Landroid/os/Handler;",
                "hasCallbacks",
                "(Ljava/lang/Runnable;)Z",
                &[Value::Ref(Some(handler)), Value::Ref(Some(removed))]
            )
            .as_int()
            .unwrap(),
            0
        );
        stop(&mut vm, thread);
        assert_eq!(
            invoke(
                &mut vm,
                "Landroid/os/Handler;",
                "post",
                "(Ljava/lang/Runnable;)Z",
                &[Value::Ref(Some(handler)), Value::Ref(Some(immediate))]
            )
            .as_int()
            .unwrap(),
            0
        );
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn java_thread_start_returns_while_worker_is_blocked_and_cannot_start_twice() {
        let mut vm = Vm::new();
        let (tx, rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let task = runnable({
            let gate = gate.clone();
            move || {
                tx.send(std::thread::current().id()).unwrap();
                let mut ready = gate.0.lock().unwrap();
                while !*ready {
                    ready = gate.1.wait(ready).unwrap();
                }
            }
        });
        let thread = object("Ljava/lang/Thread;");
        invoke(
            &mut vm,
            "Ljava/lang/Thread;",
            "<init>",
            "(Ljava/lang/Runnable;)V",
            &[Value::Ref(Some(thread)), Value::Ref(Some(task))],
        );
        invoke(
            &mut vm,
            "Ljava/lang/Thread;",
            "start",
            "()V",
            &[Value::Ref(Some(thread))],
        );
        assert_ne!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            std::thread::current().id()
        );
        vm.invoke_method(
            "Ljava/lang/Thread;",
            "start",
            "()V",
            &[Value::Ref(Some(thread))],
            true,
        )
        .unwrap();
        assert!(vm.pending_exception.take().is_some());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        invoke(
            &mut vm,
            "Ljava/lang/Thread;",
            "join",
            "(J)V",
            &[Value::Ref(Some(thread)), Value::Long(2000)],
        );
        assert_eq!(
            invoke(
                &mut vm,
                "Ljava/lang/Thread;",
                "isAlive",
                "()Z",
                &[Value::Ref(Some(thread))]
            )
            .as_int()
            .unwrap(),
            0
        );
    }
    unsafe extern "C" fn frame_task(
        env: *mut std::ffi::c_void,
        receiver: *mut std::ffi::c_void,
        _time: i64,
    ) {
        unsafe {
            run_task(env, receiver);
        }
    }
    #[test]
    fn choreographer_queues_once_on_its_looper_and_removes_callbacks() {
        let mut vm = Vm::new();
        vm.prepare_main_thread().unwrap();
        let (tx, rx) = mpsc::channel();
        let callback = runnable(move || tx.send(std::thread::current().id()).unwrap());
        crate::jni_host::register_native(
            "Lpengu/test/ThreadRunnable;",
            "doFrame",
            "(J)V",
            frame_task as *const () as usize,
        );
        let choreographer = invoke(
            &mut vm,
            "Landroid/view/Choreographer;",
            "getInstance",
            "()Landroid/view/Choreographer;",
            &[],
        )
        .as_ref()
        .unwrap();
        let args = [Value::Ref(Some(choreographer)), Value::Ref(Some(callback))];
        invoke(
            &mut vm,
            "Landroid/view/Choreographer;",
            "postFrameCallback",
            "(Landroid/view/Choreographer$FrameCallback;)V",
            &args,
        );
        assert_eq!(vm.pump_main_messages().unwrap(), 0);
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(vm.pump_main_messages().unwrap(), 1);
        assert_eq!(rx.try_recv().unwrap(), std::thread::current().id());
        assert_eq!(vm.pump_main_messages().unwrap(), 0);
        invoke(
            &mut vm,
            "Landroid/view/Choreographer;",
            "postFrameCallback",
            "(Landroid/view/Choreographer$FrameCallback;)V",
            &args,
        );
        invoke(
            &mut vm,
            "Landroid/view/Choreographer;",
            "removeFrameCallback",
            "(Landroid/view/Choreographer$FrameCallback;)V",
            &args,
        );
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(vm.pump_main_messages().unwrap(), 0);
        assert!(rx.try_recv().is_err());
        let looper = vm.threads.my_looper().unwrap();
        invoke(
            &mut vm,
            "Landroid/os/Looper;",
            "quit",
            "()V",
            &[Value::Ref(Some(looper))],
        );
        assert!(vm.pending_exception.take().is_some());
    }
    #[test]
    fn activity_ui_tasks_run_inline_on_main_and_queue_from_workers() {
        let mut vm = Vm::new();
        vm.prepare_main_thread().unwrap();
        let activity = object("Landroid/app/Activity;");
        let (tx, rx) = mpsc::channel();
        let task = runnable(move || tx.send(std::thread::current().id()).unwrap());
        let args = [Value::Ref(Some(activity)), Value::Ref(Some(task))];
        invoke(
            &mut vm,
            "Landroid/app/Activity;",
            "runOnUiThread",
            "(Ljava/lang/Runnable;)V",
            &args,
        );
        assert_eq!(rx.try_recv().unwrap(), std::thread::current().id());
        let template = vm.thread_template().unwrap();
        std::thread::spawn(move || {
            let mut worker = template.instantiate();
            invoke(
                &mut worker,
                "Landroid/app/Activity;",
                "runOnUiThread",
                "(Ljava/lang/Runnable;)V",
                &args,
            );
        })
        .join()
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(vm.pump_main_messages().unwrap(), 1);
        assert_eq!(rx.try_recv().unwrap(), std::thread::current().id());
    }
    #[test]
    fn worker_native_loads_execute_on_the_owner_without_sharing_bridge_closures() {
        let mut vm = Vm::new();
        vm.prepare_main_thread().unwrap();
        let owner = std::thread::current().id();
        let (loaded_tx, loaded_rx) = mpsc::channel();
        vm.set_native_loader(Box::new(move |path| {
            assert_eq!(std::thread::current().id(), owner);
            assert_eq!(path, "libThreadLoadTest.so");
            loaded_tx.send(()).unwrap();
            Ok(())
        }));
        let template = vm.thread_template().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut worker = template.instantiate();
            done_tx
                .send(worker.load_library_by_name("ThreadLoadTest"))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let result = loop {
            vm.pump_main_messages().unwrap();
            if let Ok(result) = done_rx.try_recv() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(result.is_ok());
        loaded_rx.try_recv().unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn messages_cannot_be_queued_to_two_loopers_at_the_same_time() {
        let shared = Arc::new(Mutex::new(HashSet::new()));
        let first = Queue::new(1, shared.clone());
        let second = Queue::new(2, shared);
        let entry = Entry {
            message: 123,
            handler: 456,
            runnable: None,
            token: None,
            what: 0,
            when: uptime_millis(),
            sequence: 0,
        };
        assert!(first.enqueue(entry.clone(), false).unwrap());
        assert!(
            second
                .enqueue(entry.clone(), false)
                .unwrap_err()
                .contains("already in use")
        );
        first.quit(false);
        assert!(second.enqueue(entry, false).unwrap());
    }
    #[test]
    fn joining_a_live_worker_is_interruptible_and_validates_timeouts() {
        let mut vm = Vm::new();
        vm.prepare_main_thread().unwrap();
        let (thread, _, _) = worker(&mut vm);
        let current = vm.threads.record(vm.threads.current());
        let interrupter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            current.state.lock().unwrap().interrupted = true;
            current.changed.notify_all();
        });
        invoke(
            &mut vm,
            "Ljava/lang/Thread;",
            "join",
            "()V",
            &[Value::Ref(Some(thread))],
        );
        assert!(vm.pending_exception.take().is_some());
        interrupter.join().unwrap();
        invoke(
            &mut vm,
            "Ljava/lang/Thread;",
            "join",
            "(J)V",
            &[Value::Ref(Some(thread)), Value::Long(-1)],
        );
        assert!(vm.pending_exception.take().is_some());
        stop(&mut vm, thread);
    }
    #[test]
    fn looper_identity_is_stable_and_main_messages_need_owner_pumping() {
        let mut vm = Vm::new();
        vm.prepare_main_thread().unwrap();
        let looper = vm.threads.my_looper().unwrap();
        assert_eq!(vm.threads.my_looper(), Some(looper));
        vm.invoke_method("Landroid/os/Looper;", "prepare", "()V", &[], false)
            .unwrap();
        assert!(vm.pending_exception.take().is_some());
        let (tx, rx) = mpsc::channel();
        let task = runnable(move || tx.send(std::thread::current().id()).unwrap());
        let handler = object("Landroid/os/Handler;");
        invoke(
            &mut vm,
            "Landroid/os/Handler;",
            "<init>",
            "()V",
            &[Value::Ref(Some(handler))],
        );
        invoke(
            &mut vm,
            "Landroid/os/Handler;",
            "post",
            "(Ljava/lang/Runnable;)Z",
            &[Value::Ref(Some(handler)), Value::Ref(Some(task))],
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(vm.pump_main_messages().unwrap(), 1);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            std::thread::current().id()
        );
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use crate::dex::{ClassDef, CodeItem, DexFile, EncodedMethod, FieldId, MethodId};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    static INITS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn bump(_env: *mut std::ffi::c_void, _class: *mut std::ffi::c_void) {
        INITS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
    }
    fn code(incoming: u16, words: Vec<u16>) -> Option<Arc<CodeItem>> {
        Some(Arc::new(CodeItem {
            registers_size: 2,
            ins_size: incoming,
            outs_size: 0,
            insns: words,
            tries: vec![],
        }))
    }
    #[test]
    fn dex_workers_share_static_writes_and_initialize_a_class_once() {
        INITS.store(0, Ordering::SeqCst);
        let mut vm = Vm::new();
        crate::jni_host::register_native(
            "Lpengu/test/InitProbe;",
            "bump",
            "()V",
            bump as *const () as usize,
        );
        let worker = "Lpengu/test/DexWorker;";
        let initialized = "Lpengu/test/Initialized;";
        let methods = vec![
            MethodId {
                class_name: worker.into(),
                proto_idx: 0,
                name: "run".into(),
                descriptor: "()V".into(),
            },
            MethodId {
                class_name: initialized.into(),
                proto_idx: 1,
                name: "get".into(),
                descriptor: "()I".into(),
            },
            MethodId {
                class_name: initialized.into(),
                proto_idx: 0,
                name: "<clinit>".into(),
                descriptor: "()V".into(),
            },
            MethodId {
                class_name: "Lpengu/test/InitProbe;".into(),
                proto_idx: 0,
                name: "bump".into(),
                descriptor: "()V".into(),
            },
        ];
        let classes = vec![
            ClassDef {
                name: worker.into(),
                access_flags: 1,
                superclass: Some("Ljava/lang/Thread;".into()),
                interfaces: vec![],
                signature: None,
                field_annotations: HashMap::new(),
                static_fields: vec![],
                instance_fields: vec![],
                direct_methods: vec![],
                virtual_methods: vec![EncodedMethod {
                    method_idx: 0,
                    access_flags: 1,
                    code: code(1, vec![0x0071, 1, 0, 0x000e]),
                }],
            },
            ClassDef {
                name: initialized.into(),
                access_flags: 1,
                superclass: Some("Ljava/lang/Object;".into()),
                interfaces: vec![],
                signature: None,
                field_annotations: HashMap::new(),
                static_fields: vec![],
                instance_fields: vec![],
                direct_methods: vec![
                    EncodedMethod {
                        method_idx: 1,
                        access_flags: 9,
                        code: code(0, vec![0x0060, 0, 0x000f]),
                    },
                    EncodedMethod {
                        method_idx: 2,
                        access_flags: 0x10008,
                        code: code(0, vec![0x0071, 3, 0, 0x7012, 0x0067, 0, 0x000e]),
                    },
                ],
                virtual_methods: vec![],
            },
        ];
        vm.load_parsed_dex(DexFile {
            strings: vec![],
            types: vec![],
            protos: vec![
                crate::dex::ProtoId {
                    shorty: "V".into(),
                    return_type: "V".into(),
                    params: vec![],
                },
                crate::dex::ProtoId {
                    shorty: "I".into(),
                    return_type: "I".into(),
                    params: vec![],
                },
            ],
            fields: vec![FieldId {
                class_name: initialized.into(),
                type_name: "I".into(),
                name: "value".into(),
            }],
            methods,
            classes,
            class_by_name: HashMap::new(),
        })
        .unwrap();
        assert!(vm.is_assignable_from("Ljava/lang/Thread;", worker));
        assert!(!vm.is_assignable_from(worker, "Ljava/lang/Thread;"));
        let a = vm.new_instance(worker).unwrap();
        let b = vm.new_instance(worker).unwrap();
        for id in [a, b] {
            vm.invoke_method(worker, "start", "()V", &[Value::Ref(Some(id))], true)
                .unwrap();
        }
        for id in [a, b] {
            vm.invoke_method(
                worker,
                "join",
                "(J)V",
                &[Value::Ref(Some(id)), Value::Long(2000)],
                true,
            )
            .unwrap();
            let r = vm.threads.record(id);
            let s = r.state.lock().unwrap();
            assert!(s.finished);
            assert!(s.error.is_none(), "{:?}", s.error);
        }
        assert_eq!(INITS.load(Ordering::SeqCst), 1);
        assert_eq!(
            vm.invoke_method(initialized, "get", "()I", &[], false)
                .unwrap()
                .as_int()
                .unwrap(),
            7
        );
    }
    #[test]
    fn latch_and_semaphore_block_until_another_thread_releases_them() {
        let mut vm = Vm::new();
        let latch = object("Ljava/util/concurrent/CountDownLatch;");
        vm.invoke_method(
            "Ljava/util/concurrent/CountDownLatch;",
            "<init>",
            "(I)V",
            &[Value::Ref(Some(latch)), Value::Int(2)],
            true,
        )
        .unwrap();
        let template = vm.thread_template().unwrap();
        let (tx, rx) = mpsc::channel();
        let (ready, waiting) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut vm = template.instantiate();
            ready.send(()).unwrap();
            vm.invoke_method(
                "Ljava/util/concurrent/CountDownLatch;",
                "await",
                "()V",
                &[Value::Ref(Some(latch))],
                true,
            )
            .unwrap();
            tx.send(()).unwrap();
            let record = vm.threads.record(vm.threads.current());
            record.state.lock().unwrap().finished = true;
            record.changed.notify_all();
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(rx.try_recv().is_err());
        vm.invoke_method(
            "Ljava/util/concurrent/CountDownLatch;",
            "countDown",
            "()V",
            &[Value::Ref(Some(latch))],
            true,
        )
        .unwrap();
        assert!(rx.try_recv().is_err());
        vm.invoke_method(
            "Ljava/util/concurrent/CountDownLatch;",
            "countDown",
            "()V",
            &[Value::Ref(Some(latch))],
            true,
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        let semaphore = object("Ljava/util/concurrent/Semaphore;");
        vm.invoke_method(
            "Ljava/util/concurrent/Semaphore;",
            "<init>",
            "(I)V",
            &[Value::Ref(Some(semaphore)), Value::Int(0)],
            true,
        )
        .unwrap();
        let unit = vm
            .static_fields
            .get("Ljava/util/concurrent/TimeUnit;.MILLISECONDS")
            .cloned()
            .unwrap();
        assert_eq!(
            vm.invoke_method(
                "Ljava/util/concurrent/Semaphore;",
                "tryAcquire",
                "(JLjava/util/concurrent/TimeUnit;)Z",
                &[Value::Ref(Some(semaphore)), Value::Long(15), unit],
                true
            )
            .unwrap()
            .as_int()
            .unwrap(),
            0
        );
        vm.invoke_method(
            "Ljava/util/concurrent/Semaphore;",
            "release",
            "()V",
            &[Value::Ref(Some(semaphore))],
            true,
        )
        .unwrap();
        assert_eq!(
            vm.invoke_method(
                "Ljava/util/concurrent/Semaphore;",
                "tryAcquire",
                "()Z",
                &[Value::Ref(Some(semaphore))],
                true
            )
            .unwrap()
            .as_int()
            .unwrap(),
            1
        );
        assert_eq!(
            vm.invoke_method(
                "Ljava/util/concurrent/Semaphore;",
                "tryAcquire",
                "()Z",
                &[Value::Ref(Some(semaphore))],
                true
            )
            .unwrap()
            .as_int()
            .unwrap(),
            0
        );
    }
}
