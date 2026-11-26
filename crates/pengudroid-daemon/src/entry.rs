//! `pengudroid-daemon`: the always-on broker.
//!
//! Responsibilities (see `docs/ARCHITECTURE.md`):
//! * accept control connections from the CLI, guests, and the Bionic shim;
//! * broker userspace binder transactions (kernel binder when available);
//! * allocate ashmem (memfd) regions and pass their fds to peers;
//! * translate guest surfaces to Wayland toplevels;
//! * hold the audio ring and the Android property store.
//!
//! It is deliberately small and dependency-light so it can live in RAM
//! indefinitely alongside the user's session.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::{graphics, log, server};
use pengudroid_proto::default_socket_path;
use pengudroid_socket::{Connection, Listener};

use crate::server::Daemon;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    socket: PathBuf,
    enable_wayland: bool,
    daemonize: bool,
    wayland_selftest: bool,
    dmabuf_selftest: bool,
}

impl Args {
    fn parse(args: &[String]) -> Result<Args, String> {
        let mut parsed = Args {
            socket: default_socket_path(),
            enable_wayland: true,
            daemonize: false,
            wayland_selftest: false,
            dmabuf_selftest: false,
        };
        let mut it = args.iter().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--socket" => {
                    let p = it.next().ok_or("--socket needs a PATH")?;
                    parsed.socket = PathBuf::from(p);
                }
                "--no-wayland" => parsed.enable_wayland = false,
                "--daemonize" => parsed.daemonize = true,
                "--wayland-selftest" => parsed.wayland_selftest = true,
                "--dmabuf-selftest" => parsed.dmabuf_selftest = true,
                "-v" | "--verbose" => log::set_verbose(true),
                "--version" | "-V" => {
                    println!("pengudroid-daemon {VERSION}");
                    std::process::exit(0);
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(parsed)
    }
}

fn print_help() {
    println!(
        "pengudroid-daemon {VERSION}\n\
         \n\
         USAGE:\n    pengudroid-daemon [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20   --socket <PATH>       control socket (default: $XDG_RUNTIME_DIR/pengudroid/daemon.sock)\n\
         \x20   --no-wayland          run without the Wayland graphics broker\n\
         \x20   --daemonize           fork into the background\n\
         \x20   --wayland-selftest    connect to Wayland, report, and exit\n\
         \x20   --dmabuf-selftest     render on the GPU and present via linux-dmabuf\n\
         \x20   -v, --verbose         verbose logging\n\
         \x20   -V, --version         print version\n\
         \x20   -h, --help            print this help"
    );
}

/// Run the broker with an explicit argv (including argv[0]).
pub fn real_main(args: &[String]) {
    let args = match Args::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    if args.wayland_selftest {
        run_wayland_selftest();
        return;
    }
    if args.dmabuf_selftest {
        run_dmabuf_selftest();
        return;
    }

    install_signal_handlers();

    // Refuse to clobber a live daemon's socket.
    if let Ok(conn) = Connection::connect_endpoint(&args.socket) {
        if probe(&conn) {
            eprintln!("pengudroid-daemon: already running on {}", args.socket.display());
            std::process::exit(0);
        }
    }

    if args.daemonize {
        if let Err(e) = daemonize() {
            eprintln!("error: daemonize failed: {e}");
            std::process::exit(1);
        }
    }

    let props_path = args
        .socket
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("pengudroid.properties");

    let t0 = Instant::now();
    let daemon = Arc::new(Daemon::new(args.socket.clone(), props_path, args.enable_wayland));
    let shutdown_daemon = Arc::clone(&daemon);
    let listener = match Listener::bind_or_abstract(&args.socket) {
        Ok(l) => {
            if l.display_path() != args.socket {
                crate::warn!(
                    "filesystem socket unavailable; listening on the abstract address {}",
                    l.display_path().display()
                );
            }
            l
        }
        Err(e) => {
            eprintln!("error: cannot bind {}: {e}", args.socket.display());
            std::process::exit(1);
        }
    };
    crate::info!(
        "daemon {} starting; control socket bound in {} ms",
        VERSION,
        t0.elapsed().as_millis()
    );

    if let Err(e) = daemon.run(listener) {
        eprintln!("error: daemon loop failed: {e}");
        std::process::exit(1);
    }
    // Tear down guest processes with the daemon instead of leaving orphaned
    // `__art` children running after it is gone.
    shutdown_daemon.kill_all_guests();
}

/// Connect as a client and do a Ping/Pong to confirm a live daemon.
fn probe(conn: &Connection) -> bool {
    use pengudroid_proto::{Message, Role, PROTO_VERSION};
    if conn
        .send_message(
            &Message::Hello { version: PROTO_VERSION, role: Role::Cli, pid: std::process::id() as i32 },
            &[],
        )
        .is_err()
    {
        return false;
    }
    matches!(conn.recv_message(), Ok((Message::Welcome { .. }, _)))
}

fn run_wayland_selftest() {
    println!("pengudroid-daemon {VERSION} — Wayland self-test");
    match graphics::Graphics::connect() {
        Some(mut g) => {
            g.refresh();
            println!("OK: connected to the Wayland compositor and bound required globals");
            println!(
                "dmabuf: {} ({} formats advertised)",
                if g.dmabuf_available() { "available" } else { "unavailable" },
                g.dmabuf_format_count()
            );
        }
        None => {
            println!("FAIL: no usable Wayland session (set WAYLAND_DISPLAY)");
            std::process::exit(1);
        }
    }
}

/// Render on the host GPU, export the result as a dmabuf, and hand it to the
/// compositor via `zwp_linux_dmabuf_v1` — the zero-copy replacement for the
/// `wl_shm` presentation path.
fn run_dmabuf_selftest() {
    println!("pengudroid-daemon {VERSION} — dmabuf self-test");
    let mut g = match graphics::Graphics::connect() {
        Some(g) => g,
        None => {
            println!("FAIL: no usable Wayland session");
            std::process::exit(1);
        }
    };
    g.refresh();
    println!(
        "compositor dmabuf: {} ({} formats advertised)",
        if g.dmabuf_available() { "available" } else { "unavailable" },
        g.dmabuf_format_count()
    );
    if !g.dmabuf_available() {
        println!("FAIL: compositor does not advertise zwp_linux_dmabuf_v1 (v3+)");
        std::process::exit(1);
    }

    let mut off = match pengudroid_hostgpu::Offscreen::new(256, 256) {
        Ok(o) => o,
        Err(e) => {
            println!("FAIL: cannot create GPU context: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = off.render(0.1, 0.2, 0.9, 1.0) {
        println!("FAIL: GPU render failed: {e}");
        std::process::exit(1);
    }
    let d = match off.export_dmabuf() {
        Ok(d) => d,
        Err(e) => {
            println!("FAIL: dmabuf export failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "exported dmabuf: {}x{} stride {} fourcc {:#010x} modifier {:#x}",
        d.width, d.height, d.stride, d.fourcc, d.modifier
    );
    println!(
        "compositor supports this format: {}",
        if g.dmabuf_supports(d.fourcc, d.modifier) { "yes" } else { "not advertised" }
    );

    match g.dmabuf_selftest(d.width, d.height, d.fd, d.stride, d.fourcc, d.modifier, d.offset) {
        Ok(()) => println!("OK: compositor accepted the dmabuf-backed wl_buffer"),
        Err(e) => {
            println!("FAIL: {e}");
            std::process::exit(1);
        }
    }

    // Negative check: a plain memfd is not a dma-buf, so the compositor must
    // reject it. If this wrongly reports success, our failure detection is not
    // observing `zwp_linux_buffer_params_v1.failed` and the positive result
    // above would be untrustworthy.
    let memfd = unsafe { libc::memfd_create(c"pengudroid-not-a-dmabuf".as_ptr(), libc::MFD_CLOEXEC) };
    if memfd >= 0 {
        use std::os::fd::FromRawFd;
        let bad = unsafe { std::os::fd::OwnedFd::from_raw_fd(memfd) };
        let rejected = g
            .dmabuf_selftest(d.width, d.height, bad, d.stride, d.fourcc, 0, 0)
            .is_err();
        if rejected {
            println!("negative check: a plain memfd is rejected (failure detection works)");
        } else {
            println!("WARNING: a plain memfd was accepted; failure detection may be broken");
        }
    }
}

extern "C" fn on_signal(_sig: libc::c_int) {
    // Only async-signal-safe work here: set a flag the accept loop polls.
    server::SHUTDOWN.store(true, Ordering::Relaxed);
}

/// Diagnostic crash handler. On-device gdbserver cannot start the
/// multi-call binary under this ld.so (it dies in `_dl_start_user` before
/// main), and ptrace attach is blocked by Yama, so a plain signal handler is
/// the only practical way to learn where a Hybris vendor library crashed the
/// daemon. Everything here is async-signal-safe: raw `write(2)` to fd 2 and a
/// hand-rolled hex encoder. No `core::fmt`, no `Stderr` lock, no allocation:
/// a formatting path inside the handler faulted on ARM32 and masked the real
/// crash twice, so the handler must be dumber than the crash.
struct RawOut {
    buf: [u8; 96],
    len: usize,
}

impl RawOut {
    const fn new() -> Self {
        Self { buf: [0; 96], len: 0 }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            unsafe {
                libc::write(2, self.buf.as_ptr() as *const libc::c_void, self.len);
            }
            self.len = 0;
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn push_hex(&mut self, mut value: usize) {
        let mut digits = [0u8; 2 * std::mem::size_of::<usize>()];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b"0123456789abcdef"[value & 0xf];
            value >>= 4;
            if value == 0 || i == 0 {
                break;
            }
        }
        self.push(b"0x");
        self.push(&digits[i..]);
    }
}

extern "C" fn on_crash(sig: libc::c_int) {
    let mut out = RawOut::new();
    out.push(b"[pengudroid-daemon] signal ");
    out.push_hex(sig as usize);
    out.push(b"\n");
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    {
        let (pc, lr, sp): (usize, usize, usize);
        unsafe {
            #[cfg(target_arch = "arm")]
            std::arch::asm!(
                "mov {0}, pc", "mov {1}, lr", "mov {2}, sp",
                out(reg) pc, out(reg) lr, out(reg) sp,
                options(nomem, nostack, preserves_flags)
            );
            #[cfg(target_arch = "aarch64")]
            std::arch::asm!(
                "adr {0}, .", "mov {1}, x30", "mov {2}, sp",
                out(reg) pc, out(reg) lr, out(reg) sp,
                options(nomem, nostack, preserves_flags)
            );
        }
        out.push(b"[pengudroid-daemon] pc=");
        out.push_hex(pc);
        out.push(b" lr=");
        out.push_hex(lr);
        out.push(b" sp=");
        out.push_hex(sp);
        out.push(b"\n");
        // Thumb frame pointers on ARM32 are unreliable inside vendor code; a
        // raw stack window is the practical way to recover return addresses.
        // Only the words of the mapping that holds `sp` are read, so the
        // handler cannot fault on a guard page.
        let stack_top = stack_mapping_top(sp);
        out.push(b"[pengudroid-daemon] stack:\n");
        let mut addr = sp & !3;
        while addr + 4 <= stack_top && addr < sp + 1024 {
            let word = unsafe { std::ptr::read_volatile(addr as *const usize) };
            out.push(b"  ");
            out.push_hex(word);
            out.push(b"\n");
            addr += 4;
        }
    }
    out.flush();
    // Raw maps so `pc`/`lr` can be resolved to a library after the fact.
    unsafe {
        let fd = libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if fd >= 0 {
            out.push(b"[pengudroid-daemon] maps:\n");
            out.flush();
            let mut buf = [0u8; 4096];
            loop {
                let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                if n <= 0 {
                    break;
                }
                libc::write(2, buf.as_ptr() as *const libc::c_void, n as usize);
            }
            libc::close(fd);
        }
    }
    unsafe { libc::_exit(128 + sig) };
}

/// Async-signal-safe lookup of the end address of the mapping that contains
/// `addr` according to /proc/self/maps. Returns 0 when it cannot be found.
fn stack_mapping_top(addr: usize) -> usize {
    unsafe {
        let fd = libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return 0;
        }
        let hex = |b: u8| -> Option<usize> {
            match b {
                b'0'..=b'9' => Some((b - b'0') as usize),
                b'a'..=b'f' => Some((b - b'a' + 10) as usize),
                _ => None,
            }
        };
        // State machine over the stream: parse "lo-hi" per line.
        let mut lo = 0usize;
        let mut hi = 0usize;
        let mut in_lo = true;
        let mut top = 0usize;
        let mut chunk = [0u8; 512];
        loop {
            let n = libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len());
            if n <= 0 {
                break;
            }
            for &b in &chunk[..n as usize] {
                match b {
                    b'\n' => {
                        if lo <= addr && addr < hi {
                            top = hi;
                        }
                        lo = 0;
                        hi = 0;
                        in_lo = true;
                    }
                    b'-' => in_lo = false,
                    b' ' => break,
                    _ => {
                        if let Some(d) = hex(b) {
                            if in_lo {
                                lo = lo.wrapping_mul(16) | d;
                            } else {
                                hi = hi.wrapping_mul(16) | d;
                            }
                        }
                    }
                }
                if top != 0 {
                    break;
                }
            }
            if top != 0 {
                break;
            }
        }
        libc::close(fd);
        top
    }
}

fn install_signal_handlers() {
    // SAFETY: `on_signal` has the correct `extern "C"` signature for a handler.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        // A guest closing the pipe must not kill the daemon with SIGPIPE.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        // ARM32 Hybris vendor libraries can crash the daemon inside EGL/gralloc
        // calls; on-device gdbserver cannot start this binary under the old
        // ld.so, so keep a raw pc/lr reporter armed there by default.
        let crash_bt = cfg!(target_arch = "arm")
            || std::env::var_os("PENGUDROID_CRASH_BT").is_some();
        if crash_bt {
            libc::signal(libc::SIGSEGV, on_crash as *const () as libc::sighandler_t);
            libc::signal(libc::SIGBUS, on_crash as *const () as libc::sighandler_t);
            libc::signal(libc::SIGABRT, on_crash as *const () as libc::sighandler_t);
        }
    }
}

/// Classic double-fork daemonization with stdio redirected to `/dev/null`.
fn daemonize() -> std::io::Result<()> {
    // SAFETY: standard POSIX daemonization sequence.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid > 0 {
            libc::_exit(0);
        }
        if libc::setsid() < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pid2 = libc::fork();
        if pid2 < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid2 > 0 {
            libc::_exit(0);
        }
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            if std::env::var_os("PENGUDROID_DAEMON_KEEP_LOG").is_none() {
                libc::dup2(devnull, 1);
                libc::dup2(devnull, 2);
            }
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }
    Ok(())
}
