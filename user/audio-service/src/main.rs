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

const DIAGNOSTIC_LOG_FLAG: &str = "--diagnostic-log";

fn parse_diagnostic_log(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<bool, &'static str> {
    match (arguments.next(), arguments.next()) {
        (None, None) => Ok(false),
        (Some(argument), None) if argument == DIAGNOSTIC_LOG_FLAG => Ok(true),
        _ => Err("usage: audio-service [--diagnostic-log]"),
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("audio-service: invariant failure: {info}")
    }));
    // Periodic metrics are opt-in because the default service writes stderr to
    // the system console; enabling them unconditionally floods serial logs and
    // hides lifecycle or failure records during long playback.
    let diagnostic_log = match parse_diagnostic_log(std::env::args_os().skip(1)) {
        Ok(enabled) => enabled,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if let Err(error) = service::run(diagnostic_log) {
        eprintln!("audio-service: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_diagnostics_require_the_explicit_flag() {
        assert_eq!(parse_diagnostic_log(std::iter::empty()), Ok(false));
        assert_eq!(
            parse_diagnostic_log([DIAGNOSTIC_LOG_FLAG.into()].into_iter()),
            Ok(true)
        );
        assert!(parse_diagnostic_log(["--unknown".into()].into_iter()).is_err());
        assert!(
            parse_diagnostic_log(
                [DIAGNOSTIC_LOG_FLAG.into(), DIAGNOSTIC_LOG_FLAG.into()].into_iter()
            )
            .is_err()
        );
    }
}
