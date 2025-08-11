#![allow(unused_macros)]

#[macro_export]
macro_rules! test_info {
    ($($arg:tt)*) => {{
        println!("[TEST] {}", format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! test_fail {
    ($($arg:tt)*) => {{
        println!("[FAIL] {}", format_args!($($arg)*));
        $crate::exit(1);
        0
    }};
}

#[macro_export]
macro_rules! test_assert {
    ($cond:expr $(,)?) => {{
        if !$cond { $crate::test_fail!("assertion failed: {}", stringify!($cond)); }
    }};
    ($cond:expr, $($arg:tt)+) => {{
        if !$cond { $crate::test_fail!($($arg)+); }
    }};
}


