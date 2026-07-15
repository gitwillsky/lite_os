use super::{DisplayMode, DrmFile, DrmMode};

pub(super) fn cvt_mode(mode: DisplayMode) -> DrmMode {
    const HV_FACTOR: u64 = 1000;
    const MIN_VSYNC_BACK_PORCH_US: u64 = 550;
    const MIN_V_PORCH: u64 = 3;
    const H_GRANULARITY: u64 = 8;
    const HSYNC_PERCENTAGE: u64 = 8;
    const M_PRIME: u64 = 300;
    const C_PRIME: u64 = 30;
    const CLOCK_STEP_KHZ: u64 = 250;
    const REFRESH: u64 = 60;

    let width = u64::from(mode.width) / H_GRANULARITY * H_GRANULARITY;
    let height = u64::from(mode.height);
    assert!(
        width != 0 && height != 0,
        "DRM mode dimensions must be nonzero"
    );
    let vsync = if height.is_multiple_of(3) && height * 4 / 3 == width {
        4
    } else if height.is_multiple_of(9) && height * 16 / 9 == width {
        5
    } else if height.is_multiple_of(10) && height * 16 / 10 == width {
        6
    } else if height.is_multiple_of(4) && height * 5 / 4 == width
        || height.is_multiple_of(9) && height * 15 / 9 == width
    {
        7
    } else {
        10
    };

    // 1. 与 Linux virtio-gpu 相同，以 display-info resolution 生成 non-reduced CVT 60 Hz mode。
    let horizontal_period = (HV_FACTOR * 1_000_000 - MIN_VSYNC_BACK_PORCH_US * HV_FACTOR * REFRESH)
        * 2
        / ((height + MIN_V_PORCH) * 2 * REFRESH);
    let sync_and_back_porch =
        (MIN_VSYNC_BACK_PORCH_US * HV_FACTOR / horizontal_period + 1).max(vsync + MIN_V_PORCH);
    let vtotal = height + sync_and_back_porch + MIN_V_PORCH;

    // 2. CVT duty-cycle 计算决定 horizontal blanking；全部运算保持整数，与 Linux helper 一致。
    let blank_percentage =
        (C_PRIME * HV_FACTOR - M_PRIME * horizontal_period / 1000).max(20 * HV_FACTOR);
    let mut hblank = width * blank_percentage / (100 * HV_FACTOR - blank_percentage);
    hblank -= hblank % (2 * H_GRANULARITY);
    let htotal = width + hblank;
    let hsync_end = width + hblank / 2;
    let mut hsync_start = hsync_end - htotal * HSYNC_PERCENTAGE / 100;
    hsync_start += H_GRANULARITY - hsync_start % H_GRANULARITY;
    let vsync_start = height + MIN_V_PORCH;
    let vsync_end = vsync_start + vsync;

    // 3. UAPI pixel clock 使用 kHz 并落到 250 kHz step；越过 u16 timing ABI 直接 fail-stop。
    let mut clock = htotal * HV_FACTOR * 1000 / horizontal_period;
    clock -= clock % CLOCK_STEP_KHZ;
    DrmMode {
        clock: u32::try_from(clock).expect("DRM pixel clock exceeds u32"),
        hdisplay: u16::try_from(width).expect("DRM hdisplay exceeds u16"),
        hsync_start: u16::try_from(hsync_start).expect("DRM hsync_start exceeds u16"),
        hsync_end: u16::try_from(hsync_end).expect("DRM hsync_end exceeds u16"),
        htotal: u16::try_from(htotal).expect("DRM htotal exceeds u16"),
        vdisplay: u16::try_from(height).expect("DRM vdisplay exceeds u16"),
        vsync_start: u16::try_from(vsync_start).expect("DRM vsync_start exceeds u16"),
        vsync_end: u16::try_from(vsync_end).expect("DRM vsync_end exceeds u16"),
        vtotal: u16::try_from(vtotal).expect("DRM vtotal exceeds u16"),
        vrefresh: REFRESH as u32,
        // non-reduced CVT uses positive VSync and negative HSync.
        flags: (1 << 2) | (1 << 1),
        mode_type: (1 << 3) | (1 << 6),
    }
}

impl DrmFile {
    /// @description 读取当前 single-connector preferred mode。
    /// @return 与最新已提交 VirtIO display-info resolution 对应的 Linux CVT 60 Hz mode。
    pub(crate) fn mode(&self) -> DrmMode {
        cvt_mode(self.device.state.lock().mode)
    }

    /// @description 原子读取 completion 已确认的 active CRTC framebuffer 与 mode。
    /// @return 尚未由 userspace modeset 时返回 `None`。
    pub(crate) fn active_crtc(&self) -> Option<(u32, DrmMode)> {
        self.device
            .completion
            .lock()
            .active
            .map(|active| (active.framebuffer, cvt_mode(active.mode)))
    }
}
