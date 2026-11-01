/// VirtIO input transport 产生的无 timestamp 原始事件。
#[derive(Debug, Clone, Copy, Default)]
pub struct RawInputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
}

/// Linux input identity 的 transport-neutral 投影。
#[derive(Debug, Clone, Copy, Default)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// absolute axis 的 immutable limits；live value 由 input core 拥有。
#[derive(Debug, Clone, Copy, Default)]
pub struct InputAbsInfo {
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDeviceError {
    Device,
}

/// 不泄漏 VirtIO queue/config 的通用 input adapter seam。
pub trait InputDevice: Send + Sync {
    /// 绑定消费者分配的 completion deferred vector；绑定前到达的 completion 由绑定时的一次
    /// 发布补偿。
    fn bind_activity(&self, activity: crate::Activity);

    /// # Returns
    ///
    /// 不含 NUL 的设备名称 bytes。
    fn name(&self) -> &[u8];
    /// # Returns
    ///
    /// 不含 NUL 的稳定 platform path bytes。
    fn physical_path(&self) -> &[u8];
    /// # Returns
    ///
    /// 不含 NUL 的唯一 serial bytes；空 slice 表示设备未提供。
    fn serial(&self) -> &[u8];
    /// # Returns
    ///
    /// immutable bus/vendor/product/version identity。
    fn id(&self) -> InputId;
    /// # Returns
    ///
    /// `INPUT_PROP_*` bitmap 的最低有效 bytes。
    fn properties(&self) -> &[u8];
    /// # Returns
    ///
    /// 支持的 `EV_*` type bitmap。
    fn event_types(&self) -> &[u8];
    /// # Parameters
    ///
    /// - `event_type`: Linux `EV_*` value。
    ///
    /// # Returns
    ///
    /// 对应 code bitmap；不支持该 type 返回空 slice。
    fn event_codes(&self, event_type: u16) -> &[u8];
    /// # Parameters
    ///
    /// - `code`: Linux `ABS_*` value。
    ///
    /// # Returns
    ///
    /// device 声明的 axis limits。
    fn abs_info(&self, code: u16) -> Option<InputAbsInfo>;
    /// # Returns
    ///
    /// 一个已完成事件；eventq 暂空返回 `None`。
    ///
    /// # Errors
    ///
    /// used ring、descriptor 或 event shape 损坏返回 `Device`。
    fn receive_event(&self) -> Result<Option<RawInputEvent>, InputDeviceError>;
    /// # Returns
    ///
    /// 本批 repost 成功返回 unit。
    ///
    /// # Errors
    ///
    /// queue notification 失败返回 `Device`。
    fn finish_receive_batch(&self) -> Result<(), InputDeviceError>;
    /// # Returns
    ///
    /// eventq 尚有未消费 used entry 时为 true。
    fn has_pending_event(&self) -> bool;
}
