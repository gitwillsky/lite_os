use crate::watchdog::{self, WatchdogConfig, WatchdogInfo};

/// 配置 watchdog 系统调用
pub fn sys_watchdog_configure(config_ptr: *const WatchdogConfig) -> isize {
    if config_ptr.is_null() {
        return -22; // EINVAL
    }

    // 安全地从用户空间读取配置
    let token = crate::task::current_user_token();
    let config_buffers = crate::memory::page_table::translated_byte_buffer(
        token,
        config_ptr as *const u8,
        core::mem::size_of::<WatchdogConfig>(),
    );

    if config_buffers.is_empty() {
        return -14; // EFAULT
    }

    let config = unsafe { *(config_buffers[0].as_ptr() as *const WatchdogConfig) };

    // 验证配置参数
    if config.timeout_us == 0 || config.timeout_us > 3600_000_000 {
        return -22; // EINVAL: timeout should be between 0 and 1 hour
    }

    if config.warning_time_us >= config.timeout_us {
        return -22; // EINVAL: warning time should be less than timeout
    }

    match watchdog::configure(config) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// 启动 watchdog 系统调用
pub fn sys_watchdog_start() -> isize {
    match watchdog::start() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// 停止 watchdog 系统调用
pub fn sys_watchdog_stop() -> isize {
    watchdog::stop();
    0
}

/// 喂狗系统调用
pub fn sys_watchdog_feed() -> isize {
    match watchdog::feed() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// 获取 watchdog 信息系统调用
pub fn sys_watchdog_get_info(info_ptr: *mut WatchdogInfo) -> isize {
    if info_ptr.is_null() {
        return -22; // EINVAL
    }

    let info = watchdog::get_info();

    // 安全地写入用户空间
    let token = crate::task::current_user_token();
    let info_buffers = crate::memory::page_table::translated_byte_buffer(
        token,
        info_ptr as *mut u8,
        core::mem::size_of::<WatchdogInfo>(),
    );

    if info_buffers.is_empty() {
        return -14; // EFAULT
    }

    unsafe {
        *(info_buffers[0].as_ptr() as *mut WatchdogInfo) = info;
    }

    0
}

/// 设置 watchdog 预设配置系统调用
pub fn sys_watchdog_set_preset(preset: u32) -> isize {
    let config = match preset {
        0 => watchdog::presets::development(),
        1 => watchdog::presets::production(),
        2 => watchdog::presets::strict(),
        3 => watchdog::presets::testing(),
        _ => return -22, // EINVAL: invalid preset
    };

    match watchdog::configure(config) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
