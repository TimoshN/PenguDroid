# Independent DEX threading and SDK dispatch

The Pengu backend owns these implementations; they do not delegate to ART or
Android framework JARs. This is a tested subset, not full Android conformance.

## Execution and ownership

`Thread.start()` starts an OS thread and returns while it runs. Each Java worker
owns a separate interpreter and a thread-local JNI callback context. Immutable
DEX definitions and method indexes use `Arc`; the heap, DEX static writes, class
initialization, queues and synchronization state are shared behind locks. A worker
never receives the main interpreter pointer or its `Rc` bridge closures.

`HandlerThread` prepares its Looper on the worker, publishes readiness to
`getLooper()`, calls `onLooperPrepared()` and blocks in the queue. Delayed messages
wait on a condition variable using monotonic uptime. Runnable callbacks and
`Handler.Callback` execute on the queue owner. Removal, front posting, immediate
quit and safe quit have separate behavior. Message ownership spans all queues;
the same Message cannot be pending on two Loopers. The main Looper cannot be quit
through the Java API. Shutdown can stop owned queues directly.

`Activity.runOnUiThread()` executes inline on the main thread and posts there from
a worker. The launcher pumps that queue after lifecycle startup. A bounded audit
uses `PENGUDROID_PENGU_WAIT_SECS`; without it, the main Looper waits for events.
`Choreographer` instances belong to a Looper and deliver cancellable, one-shot
frame callbacks with monotonic nanosecond timestamps. Its current 17 ms software
clock is **not compositor vsync**.

DEX and JNI share reentrant object monitors. `wait()` releases and reacquires the
full recursion depth; notify, timeout and interruption use condition variables.
DEX static writes are shared and class initialization runs once across workers.
CountDownLatch and nonfair Semaphore block rather than returning fake success.
Sleep and join can be interrupted; timed join validates its timeout arguments.

Native-created threads attach an owned interpreter from an immutable template;
GetEnv distinguishes detached threads and unsupported JNI versions. Detaching a
Java interpreter thread is rejected. JNI reflection handles preserve method
metadata; dynamic proxies call the actual InvocationHandler with boxed arguments.
Class tests follow DEX inheritance/interfaces, builtin collection interfaces and
array covariance. Collection backing storage retains its allocated Java class.

## SDK startup corrections

Primitive wrapper TYPE fields identify their actual primitive Class objects.
Primitive constructors/valueOf preserve payloads. Reflection enumerates encoded
DEX methods, constructors and fields, supports constructor argument dispatch and
reads field payloads. Configuration exposes a limited host display configuration.
Unknown fields raise NoSuchFieldException. Builtin reflection metadata is still
limited; an empty builtin method list does not establish that its platform API is
complete. Fatal JNI callback dispatch errors are logged and become a pending
RuntimeException instead of disappearing silently.

Worker native library requests are marshalled to the main Looper so its owning
interpreter performs real ARM64 bridge loading and prepares native entry points.
The manifest Application now starts before the Activity; this newest lifecycle
addition needs a fresh ZZZ run. See [the next-agent checkpoint](NEXT_AGENT.md).

Missing-call counts aggregate all interpreters. The compatibility JSON includes
worker failures and its observation phase. Bounded audit mode refreshes the report
while pumping the main queue; a process killed inside a native call can still leave
an earlier snapshot. Keep the trace log.

## Remaining boundaries

There is no tracing garbage collector or complete JNI reference lifetime model.
Fair Semaphore scheduling is explicitly unsupported. Thread priority/daemon
metadata does not provide a complete Android scheduler implementation. General
ExecutorService/Future behavior, Android services, resource tables, View layout,
input dispatch, WebView and network framework coverage remain incomplete.
Native engine threads may outlive activity shutdown: bridge owners are retained
until process exit to avoid unloading code beneath their callbacks. Complete
activity teardown and safe native engine unload remain open work.

These changes advance ZZZ beyond its original worker startup blocker and into
nativeRender calls. They do not establish responsive login, correct rendering or
playability. See [the ZZZ investigation](ZZZ_HANDOFF.md).

## Regression checks

```sh
cargo test --offline -p pengudroid-dexvm
cargo test --release --offline -p pengudroid-dexvm
cargo test --workspace --offline
```

Tests cover actual OS workers, asynchronous start, queue ownership and timing,
message removal/quit, cross-queue Message exclusion, main UI task delivery,
interrupted join, shared DEX statics and once-only initialization, latch/semaphore
blocking, reentrant monitors, JNI attachment, proxy invocation and SDK type/field
regressions.

Behavior references: [HandlerThread](https://developer.android.com/reference/android/os/HandlerThread),
[Looper](https://developer.android.com/reference/android/os/Looper),
[Handler](https://developer.android.com/reference/android/os/Handler),
[Choreographer](https://developer.android.com/reference/android/view/Choreographer),
[JNI functions](https://docs.oracle.com/en/java/javase/17/docs/specs/jni/functions.html).
