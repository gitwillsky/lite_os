#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use user_lib::{exit, get_args, kill, signals};

fn print_usage() {
    println!("Usage: kill [-SIGNAL] PID");
    println!("       kill -l");
    println!("");
    println!("Send a signal to a process");
    println!("");
    println!("Options:");
    println!("  -l            List available signals");
    println!("  -SIGNAL       Signal to send (default: TERM)");
    println!("  -s SIGNAL     Signal to send (alternative format)");
    println!("");
    println!("Common signals:");
    println!("  TERM (15)     Terminate (default)");
    println!("  KILL (9)      Kill (cannot be caught)");
    println!("  INT (2)       Interrupt");
    println!("  HUP (1)       Hangup");
    println!("  STOP (19)     Stop process");
    println!("  CONT (18)     Continue process");
}

fn list_signals() {
    println!("Available signals:");
    println!(" 1) SIGHUP      2) SIGINT      3) SIGQUIT     4) SIGILL");
    println!(" 5) SIGTRAP     6) SIGABRT     7) SIGBUS      8) SIGFPE");
    println!(" 9) SIGKILL    10) SIGUSR1    11) SIGSEGV    12) SIGUSR2");
    println!("13) SIGPIPE    14) SIGALRM    15) SIGTERM    16) SIGSTKFLT");
    println!("17) SIGCHLD    18) SIGCONT    19) SIGSTOP    20) SIGTSTP");
    println!("21) SIGTTIN    22) SIGTTOU    23) SIGURG     24) SIGXCPU");
    println!("25) SIGXFSZ    26) SIGVTALRM  27) SIGPROF    28) SIGWINCH");
    println!("29) SIGIO      30) SIGPWR     31) SIGSYS");
}

fn parse_signal(signal_str: &str) -> Option<u32> {
    // Remove '-' prefix if present
    let signal_str = if signal_str.starts_with('-') {
        &signal_str[1..]
    } else {
        signal_str
    };

    // Try to parse as number first
    if let Ok(num) = signal_str.parse::<u32>() {
        if num >= 1 && num <= 31 {
            return Some(num);
        }
    }

    // Try to parse as signal name
    match signal_str.to_uppercase().as_str() {
        "HUP" | "SIGHUP" => Some(signals::SIGHUP),
        "INT" | "SIGINT" => Some(signals::SIGINT),
        "QUIT" | "SIGQUIT" => Some(signals::SIGQUIT),
        "ILL" | "SIGILL" => Some(signals::SIGILL),
        "TRAP" | "SIGTRAP" => Some(signals::SIGTRAP),
        "ABRT" | "SIGABRT" => Some(signals::SIGABRT),
        "BUS" | "SIGBUS" => Some(signals::SIGBUS),
        "FPE" | "SIGFPE" => Some(signals::SIGFPE),
        "KILL" | "SIGKILL" => Some(signals::SIGKILL),
        "USR1" | "SIGUSR1" => Some(signals::SIGUSR1),
        "SEGV" | "SIGSEGV" => Some(signals::SIGSEGV),
        "USR2" | "SIGUSR2" => Some(signals::SIGUSR2),
        "PIPE" | "SIGPIPE" => Some(signals::SIGPIPE),
        "ALRM" | "SIGALRM" => Some(signals::SIGALRM),
        "TERM" | "SIGTERM" => Some(signals::SIGTERM),
        "STKFLT" | "SIGSTKFLT" => Some(signals::SIGSTKFLT),
        "CHLD" | "SIGCHLD" => Some(signals::SIGCHLD),
        "CONT" | "SIGCONT" => Some(signals::SIGCONT),
        "STOP" | "SIGSTOP" => Some(signals::SIGSTOP),
        "TSTP" | "SIGTSTP" => Some(signals::SIGTSTP),
        "TTIN" | "SIGTTIN" => Some(signals::SIGTTIN),
        "TTOU" | "SIGTTOU" => Some(signals::SIGTTOU),
        "URG" | "SIGURG" => Some(signals::SIGURG),
        "XCPU" | "SIGXCPU" => Some(signals::SIGXCPU),
        "XFSZ" | "SIGXFSZ" => Some(signals::SIGXFSZ),
        "VTALRM" | "SIGVTALRM" => Some(signals::SIGVTALRM),
        "PROF" | "SIGPROF" => Some(signals::SIGPROF),
        "WINCH" | "SIGWINCH" => Some(signals::SIGWINCH),
        "IO" | "SIGIO" => Some(signals::SIGIO),
        "PWR" | "SIGPWR" => Some(signals::SIGPWR),
        "SYS" | "SIGSYS" => Some(signals::SIGSYS),
        _ => None,
    }
}

fn parse_args() -> (Vec<String>, i32) {
    let mut argc: usize = 0;
    let mut argv_buf = [0u8; 4096];

    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        return (Vec::new(), -1);
    }

    let mut args = Vec::new();
    let mut start = 0;

    for i in 0..argv_buf.len() {
        if argv_buf[i] == 0 {
            if start < i {
                if let Ok(arg) = core::str::from_utf8(&argv_buf[start..i]) {
                    args.push(String::from(arg));
                }
            }
            start = i + 1;
            if args.len() >= argc {
                break;
            }
        }
    }

    (args, 0)
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let (args, result) = parse_args();
    if result < 0 {
        println!("Error: Failed to get command line arguments");
        return 1;
    }

    if args.len() < 2 {
        print_usage();
        return 1;
    }

    // Handle -l option (list signals)
    if args.len() == 2 && args[1] == "-l" {
        list_signals();
        return 0;
    }

    let mut signal = signals::SIGTERM; // Default signal
    let mut pid_str = "";
    let mut i = 1;

    // Parse arguments
    while i < args.len() {
        let arg = &args[i];

        if arg.starts_with('-') && arg.len() > 1 {
            if arg == "-s" {
                // -s SIGNAL format
                i += 1;
                if i >= args.len() {
                    println!("Error: -s requires a signal argument");
                    return 1;
                }
                if let Some(sig) = parse_signal(&args[i]) {
                    signal = sig;
                } else {
                    println!("Error: Invalid signal '{}'", args[i]);
                    return 1;
                }
            } else {
                // -SIGNAL format
                if let Some(sig) = parse_signal(arg) {
                    signal = sig;
                } else {
                    println!("Error: Invalid signal '{}'", arg);
                    return 1;
                }
            }
        } else {
            // This should be the PID
            pid_str = arg;
            break;
        }
        i += 1;
    }

    if pid_str.is_empty() {
        println!("Error: No PID specified");
        print_usage();
        return 1;
    }

    // Parse PID
    let pid = match pid_str.parse::<u32>() {
        Ok(p) => p,
        Err(_) => {
            println!("Error: Invalid PID '{}'", pid_str);
            return 1;
        }
    };

    if pid == 0 {
        println!("Error: Cannot kill process 0");
        return 1;
    }

    // Send the signal
    let result = kill(pid as usize, signal);
    if result < 0 {
        match result {
            -1 => println!("Error: No such process (PID {})", pid),
            -2 => println!("Error: Permission denied"),
            -3 => println!("Error: Invalid signal"),
            _ => println!("Error: Failed to send signal (code: {})", result),
        }
        return 1;
    }

    // Success - don't print anything unless verbose
    0
}
