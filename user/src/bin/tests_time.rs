#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{get_time_ms, get_time_us, get_time_ns, time, gettimeofday, TimeVal, nanosleep, TimeSpec, exit};

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("time: 开始时间接口测试");

    let ms1 = get_time_ms();
    let us1 = get_time_us();
    let ns1 = get_time_ns();
    test_assert!(ms1 >= 0 && us1 >= 0 && ns1 >= 0, "获取时间失败");

    let mut tv = TimeVal { tv_sec: 0, tv_usec: 0 };
    let r = gettimeofday(&mut tv, core::ptr::null_mut());
    test_assert!(r == 0, "gettimeofday 失败: {}", r);

    // 睡眠 50ms
    let req = TimeSpec { tv_sec: 0, tv_nsec: 50_000_000 };
    let nr = nanosleep(&req, core::ptr::null_mut());
    test_assert!(nr == 0, "nanosleep 失败: {}", nr);

    let ms2 = get_time_ms();
    test_assert!(ms2 - ms1 >= 40, "sleep 后时间未前进: {} -> {}", ms1, ms2);

    let t = time();
    test_assert!(t >= 0, "time 失败: {}", t);

    test_info!("time: 所有用例通过");
    exit(0);
    0
}


