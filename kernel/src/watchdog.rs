use crate::timer::get_time_us;
use alloc::vec::Vec;
use spin::{Mutex, Once};

/// Watchdog 配置结构体
#[derive(Debug, Clone, Copy)]
pub struct WatchdogConfig {
    /// 超时时间（微秒）
    pub timeout_us: u64,
    /// 是否启用
    pub enabled: bool,
    /// 是否在超时时重启系统
    pub reboot_on_timeout: bool,
    /// 预警时间（微秒），在超时前这个时间发出警告
    pub warning_time_us: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            timeout_us: 10_000_000,     // 10 秒 - 给内核更多时间启动
            enabled: true,              // 默认开启
            reboot_on_timeout: false,   // 默认不重启，而是蓝屏
            warning_time_us: 3_000_000, // 3 秒预警
        }
    }
}

/// Watchdog 状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WatchdogState {
    Disabled,
    Active,
    Warning,
    Timeout,
}

/// Watchdog 事件类型
#[derive(Debug, Clone, Copy)]
pub enum WatchdogEvent {
    Started,
    Fed,
    Warning,
    Timeout,
    Stopped,
}

/// Watchdog 回调函数类型
pub type WatchdogCallback = fn(event: WatchdogEvent);

/// Watchdog 核心结构体
struct WatchdogCore {
    config: WatchdogConfig,
    state: WatchdogState,
    last_feed_time: u64,
    callback: Option<WatchdogCallback>,
    feed_count: u64,
    timeout_count: u64,
}

impl WatchdogCore {
    pub fn new() -> Self {
        Self {
            config: Default::default(),
            state: WatchdogState::Disabled,
            last_feed_time: 0,
            callback: None,
            feed_count: 0,
            timeout_count: 0,
        }
    }

    fn configure(&mut self, config: WatchdogConfig) {
        self.config = config;
        if !config.enabled && self.state != WatchdogState::Disabled {
            self.stop();
        }
    }

    fn start(&mut self) -> Result<(), &'static str> {
        if !self.config.enabled {
            return Err("Watchdog not enabled in configuration");
        }

        self.last_feed_time = get_time_us();
        self.state = WatchdogState::Active;
        self.feed_count = 0;
        self.timeout_count = 0;

        if let Some(callback) = self.callback {
            callback(WatchdogEvent::Started);
        }

        debug!(
            "Watchdog started with timeout: {}us",
            self.config.timeout_us
        );
        Ok(())
    }

    fn stop(&mut self) {
        if self.state != WatchdogState::Disabled {
            self.state = WatchdogState::Disabled;

            if let Some(callback) = self.callback {
                callback(WatchdogEvent::Stopped);
            }

            debug!("Watchdog stopped");
        }
    }

    fn feed(&mut self) -> Result<(), &'static str> {
        if self.state == WatchdogState::Disabled {
            return Err("Watchdog is disabled");
        }

        self.last_feed_time = get_time_us();
        self.feed_count += 1;

        // 如果之前处于警告状态，现在恢复正常
        if self.state == WatchdogState::Warning {
            self.state = WatchdogState::Active;
            debug!("Watchdog fed, returned to active state");
        }

        if let Some(callback) = self.callback {
            callback(WatchdogEvent::Fed);
        }

        Ok(())
    }

    fn check(&mut self) {
        if self.state == WatchdogState::Disabled {
            return;
        }

        let current_time = get_time_us();
        let elapsed = current_time - self.last_feed_time;

        match self.state {
            WatchdogState::Active => {
                // 检查是否需要发出警告
                if elapsed > (self.config.timeout_us - self.config.warning_time_us) {
                    self.state = WatchdogState::Warning;

                    if let Some(callback) = self.callback {
                        callback(WatchdogEvent::Warning);
                    }

                    warn!("Watchdog warning: {}us since last feed", elapsed);
                }
            }
            WatchdogState::Warning => {
                // 检查是否超时
                if elapsed > self.config.timeout_us {
                    self.state = WatchdogState::Timeout;
                    self.timeout_count += 1;

                    if let Some(callback) = self.callback {
                        callback(WatchdogEvent::Timeout);
                    }

                    error!("Watchdog timeout! {}us since last feed", elapsed);
                }
            }
            WatchdogState::Timeout => {
                // 已经超时，继续等待处理或重启
                if self.config.reboot_on_timeout {
                    self.trigger_reboot();
                } else {
                    self.trigger_panic();
                }
            }
            _ => {}
        }
    }

    fn trigger_reboot(&self) {
        error!("Watchdog triggered system reboot!");
        error!(
            "Stats: feeds={}, timeouts={}",
            self.feed_count, self.timeout_count
        );

        // 触发系统重启
        crate::arch::sbi::shutdown();
    }

    fn trigger_panic(&self) {
        error!("Watchdog triggered kernel panic!");
        error!("System appears to be hung or unresponsive");
        error!(
            "Stats: feeds={}, timeouts={}",
            self.feed_count, self.timeout_count
        );
        error!(
            "Last feed was {}us ago",
            crate::timer::get_time_us() - self.last_feed_time
        );

        // 触发内核 panic（蓝屏）
        panic!(
            "Watchdog timeout: System unresponsive for {}us",
            crate::timer::get_time_us() - self.last_feed_time
        );
    }

    fn set_callback(&mut self, callback: Option<WatchdogCallback>) {
        self.callback = callback;
    }

    fn get_info(&self) -> WatchdogInfo {
        let current_time = get_time_us();
        let time_since_feed = if self.state != WatchdogState::Disabled {
            current_time - self.last_feed_time
        } else {
            0
        };

        WatchdogInfo {
            state: self.state,
            config: self.config,
            time_since_feed_us: time_since_feed,
            feed_count: self.feed_count,
            timeout_count: self.timeout_count,
        }
    }
}

/// Watchdog 信息结构体（用于查询状态）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WatchdogInfo {
    pub state: WatchdogState,
    pub config: WatchdogConfig,
    pub time_since_feed_us: u64,
    pub feed_count: u64,
    pub timeout_count: u64,
}

// 全局 watchdog 实例
static WATCHDOG: Once<Mutex<WatchdogCore>> = Once::new();

/// 初始化 watchdog 系统
pub fn init() {
    WATCHDOG.call_once(|| Mutex::new(WatchdogCore::new()));

    debug!("Watchdog system initialized");

    // 设置默认回调
    set_callback(Some(default_callback));

    // 使用默认配置启动 watchdog
    if let Err(e) = start() {
        warn!("Failed to start watchdog: {}", e);
    } else {
        info!("Watchdog started with 60s timeout");
    }
}

/// 配置 watchdog
pub fn configure(config: WatchdogConfig) -> Result<(), &'static str> {
    let mut watchdog = WATCHDOG.wait().lock();
    watchdog.configure(config);
    Ok(())
}

/// 启动 watchdog
pub fn start() -> Result<(), &'static str> {
    let mut watchdog = WATCHDOG.wait().lock();
    watchdog.start()
}

/// 停止 watchdog
pub fn stop() {
    let mut watchdog = WATCHDOG.wait().lock();
    watchdog.stop();
}

/// 喂狗（重置计时器）
pub fn feed() -> Result<(), &'static str> {
    let mut watchdog = WATCHDOG.wait().lock();
    watchdog.feed()
}

/// 检查 watchdog 状态（由定时器调用）
pub fn check() {
    // 使用 try_lock 避免在中断上下文中阻塞
    if let Some(mut watchdog) = WATCHDOG.wait().try_lock() {
        watchdog.check();
    }
}

/// 设置 watchdog 回调函数
pub fn set_callback(callback: Option<WatchdogCallback>) {
    let mut watchdog = WATCHDOG.wait().lock();
    watchdog.set_callback(callback);
}

/// 获取 watchdog 信息
pub fn get_info() -> WatchdogInfo {
    let watchdog = WATCHDOG.wait().lock();
    watchdog.get_info()
}

/// 创建预设配置
pub mod presets {
    use super::WatchdogConfig;

    /// 开发模式配置（较长超时时间）
    pub fn development() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 60_000_000, // 60 秒
            enabled: true,
            reboot_on_timeout: false,    // 开发时不重启
            warning_time_us: 10_000_000, // 10 秒预警
        }
    }

    /// 生产模式配置（较短超时时间）
    pub fn production() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 30_000_000, // 30 秒
            enabled: true,
            reboot_on_timeout: true,
            warning_time_us: 5_000_000, // 5 秒预警
        }
    }

    /// 严格模式配置（很短超时时间）
    pub fn strict() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 10_000_000, // 10 秒
            enabled: true,
            reboot_on_timeout: true,
            warning_time_us: 2_000_000, // 2 秒预警
        }
    }

    /// 测试模式配置（用于测试）
    pub fn testing() -> WatchdogConfig {
        WatchdogConfig {
            timeout_us: 5_000_000, // 5 秒
            enabled: true,
            reboot_on_timeout: false,
            warning_time_us: 1_000_000, // 1 秒预警
        }
    }
}

/// 默认的 watchdog 回调函数
pub fn default_callback(event: WatchdogEvent) {
    match event {
        WatchdogEvent::Started => {
            info!("Watchdog started");
        }
        WatchdogEvent::Fed => {
            // 正常情况下不需要日志，避免过多输出
        }
        WatchdogEvent::Warning => {
            warn!("Watchdog warning: system may be unresponsive");
        }
        WatchdogEvent::Timeout => {
            error!("Watchdog timeout: system appears to be hung");
        }
        WatchdogEvent::Stopped => {
            info!("Watchdog stopped");
        }
    }
}
