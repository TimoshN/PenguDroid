fn main() {
    // Restore the default SIGPIPE disposition: Rust ignores it, which turns a
    // closed stdout (e.g. `pengudroid ... | head`) into a panic.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(pengudroid_cli::real_main(&args));
}
