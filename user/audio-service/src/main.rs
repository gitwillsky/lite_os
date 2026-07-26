//! LiteOS system audio service.
//!
//! The control thread owns sockets, quotas, shared-memory publication and
//! settings. The mixer thread owns the ALSA OFD and is allocation- and
//! lock-free after startup.

mod allocation;
mod alsa;
mod limiter;
mod mixer;
mod queue;
mod service;
mod settings;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("audio-service: invariant failure: {info}")
    }));
    if let Err(error) = service::run() {
        eprintln!("audio-service: {error}");
        std::process::exit(1);
    }
}
