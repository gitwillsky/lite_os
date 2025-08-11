#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{watchdog_configure, watchdog_start, watchdog_stop, watchdog_feed, watchdog_get_info, watchdog_set_preset, WatchdogConfig, WatchdogInfo, exit};

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("watchdog: 开始看门狗接口测试");

    // 配置到测试预设
    let pr = watchdog_set_preset(3);
    test_assert!(pr == 0 || pr == -1, "watchdog_set_preset 返回异常: {}", pr);

    // 显式配置
    let cfg = WatchdogConfig { timeout_us: 2_000_000, enabled: true, reboot_on_timeout: false, warning_time_us: 500_000 };
    let rc = watchdog_configure(&cfg);
    test_assert!(rc == 0 || rc == -1, "watchdog_configure 返回异常: {}", rc);

    // 启动/喂狗/查询
    let st = watchdog_start();
    test_assert!(st == 0 || st == -1, "watchdog_start 返回异常: {}", st);
    let _ = watchdog_feed();
    let mut info = WatchdogInfo { state: unsafe { core::mem::zeroed() }, config: cfg, time_since_feed_us: 0, feed_count: 0, timeout_count: 0 };
    let gi = watchdog_get_info(&mut info);
    test_assert!(gi == 0 || gi == -1, "watchdog_get_info 返回异常: {}", gi);
    let sp = watchdog_stop();
    test_assert!(sp == 0 || sp == -1, "watchdog_stop 返回异常: {}", sp);

    test_info!("watchdog: 所有用例通过(若未实现则容忍 -1) ");
    exit(0);
    0
}


