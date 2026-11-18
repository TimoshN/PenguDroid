//! Optional CLI/daemon dispatch for the Aurora multi-call executable.
//!
//! Plain arguments are still treated as APK paths by the GUI. A recognized
//! PenguDroid command never reaches Slint: it runs in the same process instead.

/// Return without doing anything for the GUI; exit after running a command.
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let Some(command) = args.get(1).map(String::as_str) else {
        return;
    };
    if command == "--cli" {
        // Keep `--cli` as argv[0]; the following token becomes the command.
        std::process::exit(pengudroid_cli::real_main(&args[1..]));
    }
    const COMMANDS: &[&str] = &[
        "run", "launch", "extract", "demo", "start", "ps", "tap", "window", "key",
        "input", "info", "inspect", "deps", "doctor", "keymap-preset", "sysroot",
        "runtime", "art-test", "__art", "geometry", "gpu-info", "gpu-test",
        "video-decode-test", "audio-test", "input-test", "libpath", "de-bionic",
        "nativelib", "native", "__native", "__dlopen", "getprop", "setprop",
        "services", "telephony", "daemon", "stop", "status", "__guest", "__daemon",
        "-V", "--version", "-h", "--help",
    ];
    if COMMANDS.contains(&command) {
        std::process::exit(pengudroid_cli::real_main(&args));
    }
}
