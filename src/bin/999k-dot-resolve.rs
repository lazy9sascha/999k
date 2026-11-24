//! Source: PigeonMQ e399161, src/bin/pigeonmq-dot-resolve.rs (MIT).
//! Owned native DNS subprocess for the dot callback transport. Not a service.
use std::io::Write;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || args[1] != "--dot-resolve" {
        std::process::exit(2);
    }
    match radio999k::events::callback_transport::resolver_child(&args[2]) {
        Ok(bytes) => {
            if std::io::stdout().write_all(&bytes).is_err() {
                std::process::exit(1);
            }
        }
        Err(_) => std::process::exit(1),
    }
}
