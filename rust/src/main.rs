fn main() {
    if let Err(err) = megaserver::run() {
        eprintln!("megaserver: {err:#}");
        std::process::exit(1);
    }
}
