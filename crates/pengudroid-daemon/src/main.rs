fn main() {
    let args: Vec<String> = std::env::args().collect();
    pengudroid_daemon::real_main(&args);
}
