#![allow(unused_macros)]

#[macro_export]
macro_rules! test_info {
    ($($arg:tt)*) => {{
        println!("\x1b[36m[INFO]\x1b[0m {}", format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! test_pass {
    ($($arg:tt)*) => {{
        println!("\x1b[32m[PASS]\x1b[0m {}", format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! test_fail {
    ($($arg:tt)*) => {{
        println!("\x1b[31m[FAIL]\x1b[0m {}", format_args!($($arg)*));
        $crate::exit(1);
        0
    }};
}

#[macro_export]
macro_rules! test_warn {
    ($($arg:tt)*) => {{
        println!("\x1b[33m[WARN]\x1b[0m {}", format_args!($($arg)*));
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

#[macro_export]
macro_rules! test_section {
    ($name:expr) => {{
        println!("\x1b[1;34m=== {} ===\x1b[0m", $name);
    }};
}

#[macro_export]
macro_rules! test_subsection {
    ($name:expr) => {{
        println!("\x1b[34m--- {} ---\x1b[0m", $name);
    }};
}

#[macro_export]
macro_rules! test_summary {
    ($total:expr, $passed:expr, $failed:expr) => {{
        println!("\x1b[1;37m{}\x1b[0m", "=".repeat(60));
        println!("\x1b[1;37m测试总结\x1b[0m");
        println!("  总测试数: \x1b[1m{}\x1b[0m", $total);
        println!("  通过数: \x1b[32m{}\x1b[0m", $passed);
        println!("  失败数: \x1b[31m{}\x1b[0m", $failed);
        if $failed == 0 {
            println!("\x1b[32m🎉 所有测试通过！\x1b[0m");
        } else {
            println!("\x1b[31m❌ 有测试失败\x1b[0m");
        }
        println!("\x1b[1;37m{}\x1b[0m", "=".repeat(60));
    }};
}

pub struct TestStats {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

impl TestStats {
    pub fn new() -> Self {
        Self {
            total: 0,
            passed: 0,
            failed: 0,
        }
    }

    pub fn pass(&mut self) {
        self.total += 1;
        self.passed += 1;
    }

    pub fn fail(&mut self) {
        self.total += 1;
        self.failed += 1;
    }
}

#[macro_export]
macro_rules! run_test {
    ($stats:expr, $test_name:expr, $test_code:block) => {{
        test_subsection!($test_name);
        let result = std::panic::catch_unwind(|| $test_code);
        match result {
            Ok(_) => {
                test_pass!("{}", $test_name);
                $stats.pass();
            }
            Err(_) => {
                test_fail!("{}", $test_name);
                $stats.fail();
            }
        }
    }};
}
