use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

/// @description VirtIO input transport 产生的无 timestamp 原始事件。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RawInputEvent {
    pub(crate) event_type: u16,
    pub(crate) code: u16,
    pub(crate) value: i32,
}

/// @description Linux input identity 的 transport-neutral 投影。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputId {
    pub(crate) bustype: u16,
    pub(crate) vendor: u16,
    pub(crate) product: u16,
    pub(crate) version: u16,
}

/// @description absolute axis 的 immutable limits；live value 由 input core 拥有。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputAbsInfo {
    pub(crate) minimum: i32,
    pub(crate) maximum: i32,
    pub(crate) fuzz: i32,
    pub(crate) flat: i32,
    pub(crate) resolution: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputDeviceError {
    Device,
}

/// @description 不泄漏 VirtIO queue/config 的通用 input adapter seam。
pub(crate) trait InputDevice: Send + Sync {
    /// @return 不含 NUL 的设备名称 bytes。
    fn name(&self) -> &[u8];
    /// @return 不含 NUL 的稳定 platform path bytes。
    fn physical_path(&self) -> &[u8];
    /// @return 不含 NUL 的唯一 serial bytes；空 slice 表示设备未提供。
    fn serial(&self) -> &[u8];
    /// @return immutable bus/vendor/product/version identity。
    fn id(&self) -> InputId;
    /// @return `INPUT_PROP_*` bitmap 的最低有效 bytes。
    fn properties(&self) -> &[u8];
    /// @return 支持的 `EV_*` type bitmap。
    fn event_types(&self) -> &[u8];
    /// @param event_type Linux `EV_*` value。
    /// @return 对应 code bitmap；不支持该 type 返回空 slice。
    fn event_codes(&self, event_type: u16) -> &[u8];
    /// @param code Linux `ABS_*` value。
    /// @return device 声明的 axis limits。
    fn abs_info(&self, code: u16) -> Option<InputAbsInfo>;
    /// @return 一个已完成事件；eventq 暂空返回 `None`。
    /// @errors used ring、descriptor 或 event shape 损坏返回 `Device`。
    fn receive_event(&self) -> Result<Option<RawInputEvent>, InputDeviceError>;
    /// @return 本批 repost 成功返回 unit。
    /// @errors queue notification 失败返回 `Device`。
    fn finish_receive_batch(&self) -> Result<(), InputDeviceError>;
    /// @return eventq 尚有未消费 used entry 时为 true。
    fn has_pending_event(&self) -> bool;
}

// OWNER: drivers input registry 唯一保存 DTB 枚举顺序与 raw adapter Arc；input core 只按
// index 取得不可变快照。缺失该 owner 会让 devfs event minor 与 IRQ adapter 身份分裂。
static INPUT_DEVICES: Once<Mutex<Vec<Arc<dyn InputDevice>>>> = Once::new();

fn registry() -> &'static Mutex<Vec<Arc<dyn InputDevice>>> {
    INPUT_DEVICES.call_once(|| Mutex::new(Vec::new()))
}

/// @description 按 DTB probe 顺序注册一个 input adapter。
/// @param device 已完成 feature/queue 初始化的唯一 adapter Arc。
/// @return 后续 `/dev/input/eventN` 使用的零基 index。
/// @errors registry 扩容失败返回原 device。
pub(super) fn register(device: Arc<dyn InputDevice>) -> Result<usize, Arc<dyn InputDevice>> {
    let mut devices = registry().lock();
    if devices.try_reserve(1).is_err() {
        return Err(device);
    }
    let index = devices.len();
    devices.push(device);
    Ok(index)
}

/// @description 读取已注册 raw input adapter 数量。
/// @return DTB probe 完成后的稳定数量。
pub(crate) fn device_count() -> usize {
    registry().lock().len()
}

/// @description 按 event index 取得 raw adapter。
/// @param index `register` 返回的稳定 index。
/// @return 对应 adapter Arc；越界返回 `None`。
pub(crate) fn device(index: usize) -> Option<Arc<dyn InputDevice>> {
    registry().lock().get(index).cloned()
}
