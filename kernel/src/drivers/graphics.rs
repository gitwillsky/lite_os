use alloc::sync::Arc;

use crate::memory::DeviceBacking;

use super::{DisplayDevice, DisplayError, DisplayMode, DisplayRect};

/// @description 由 VirtIO-GPU 独立 cursorq 消费的硬件光标命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorCommand {
    /// 只移动当前光标，不重新读取像素。
    Move {
        /// Scanout 0 中的水平位置。
        x: u32,
        /// Scanout 0 中的垂直位置。
        y: u32,
        /// 当前 cursor resource 是否可见。QEMU 10 的 MOVE 路径仍以 resource_id
        /// 控制 host pointer 可见性；缺失该状态会让每次移动都隐藏刚更新的光标。
        visible: bool,
    },
    /// 切换光标资源并同时设置位置与热点；resource 0 表示隐藏。
    Update {
        /// Scanout 0 中的水平位置。
        x: u32,
        /// Scanout 0 中的垂直位置。
        y: u32,
        /// true 使用 adapter-owned 2D cursor resource；false 表示隐藏。
        visible: bool,
        /// Resource 内的水平热点。
        hot_x: u32,
        /// Resource 内的垂直热点。
        hot_y: u32,
    },
}

/// @description VirGL capset 的 stable identity 与 exact byte contract。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VirglCapsetInfo {
    /// `VIRTIO_GPU_CAPSET_VIRGL` 或 `VIRTIO_GPU_CAPSET_VIRGL2`。
    pub(crate) id: u32,
    /// host 宣告并由 adapter 选中的最高协议版本。
    pub(crate) version: u32,
    /// `GET_CAPSET` 返回的 exact capability byte 数。
    pub(crate) size: usize,
}

/// @description VirGL 3D transfer 使用的三维半开区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VirglBox {
    /// 左上前方 X 坐标。
    pub(crate) x: u32,
    /// 左上前方 Y 坐标。
    pub(crate) y: u32,
    /// 左上前方 Z 坐标。
    pub(crate) z: u32,
    /// 非零 X extent。
    pub(crate) width: u32,
    /// 非零 Y extent。
    pub(crate) height: u32,
    /// 非零 Z extent。
    pub(crate) depth: u32,
}

/// @description 标准 VirtIO-GPU 3D transfer 的数据流向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VirglTransferDirection {
    /// 从 guest backing 上传到 host resource。
    ToHost,
    /// 从 host resource 回读到 guest backing。
    FromHost,
}

/// @description 一条标准 VirtIO-GPU VirGL controlq operation。
#[derive(Clone)]
pub(crate) enum VirglCommand<'a> {
    /// 创建一个 VirGL context。
    ContextCreate {
        /// 非零、device-wide unique context identity。
        context_id: u32,
        /// `VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK` 编码后的 capset 初始化值。
        context_init: u32,
        /// 最多 64 bytes 的诊断名称。
        name: &'a [u8],
    },
    /// 销毁一个 context 及其 host-side state。
    ContextDestroy {
        /// 已存在的 context identity。
        context_id: u32,
    },
    /// 创建一个 VirGL pipe resource。
    ResourceCreate3d {
        /// 非零、device-wide unique resource identity。
        resource_id: u32,
        /// Gallium pipe texture target。
        target: u32,
        /// VirGL format identity。
        format: u32,
        /// Gallium/VirGL bind flags。
        bind: u32,
        /// Base-level pixel width。
        width: u32,
        /// Base-level pixel height。
        height: u32,
        /// Base-level pixel depth。
        depth: u32,
        /// Array layer count。
        array_size: u32,
        /// Highest mip level。
        last_level: u32,
        /// Sample count。
        samples: u32,
        /// Protocol resource flags。
        flags: u32,
    },
    /// 把 guest pages 绑定为 resource backing。
    ResourceAttachBacking {
        /// 已创建的 resource identity。
        resource_id: u32,
        /// 覆盖 resource storage 的 DMA-stable scatter/gather owner。
        backing: Arc<DeviceBacking>,
    },
    /// 从 resource 解绑 guest backing。
    ResourceDetachBacking {
        /// 已创建且已绑定 backing 的 resource identity。
        resource_id: u32,
    },
    /// 把 resource 加入 context 可见对象集合。
    ContextAttachResource {
        /// 已创建的 context identity。
        context_id: u32,
        /// 已创建的 resource identity。
        resource_id: u32,
    },
    /// 从 context 可见对象集合删除 resource。
    ContextDetachResource {
        /// 已创建的 context identity。
        context_id: u32,
        /// 已 attach 的 resource identity。
        resource_id: u32,
    },
    /// 提交一段 exact VirGL command stream。
    Submit3d {
        /// 接收 command stream 的 context identity。
        context_id: u32,
        /// 4-byte aligned VirGL dword stream。
        commands: &'a [u8],
    },
    /// 在 guest backing 与 host resource 之间传输一个三维区域。
    Transfer3d {
        /// 传输方向。
        direction: VirglTransferDirection,
        /// 接收 transfer 的 context identity。
        context_id: u32,
        /// guest backing 起始 byte offset。
        offset: u64,
        /// 目标 resource identity。
        resource_id: u32,
        /// 目标 mip level。
        level: u32,
        /// transfer 三维区域。
        region: VirglBox,
        /// 相邻 image row 的 byte 距离。
        stride: u32,
        /// 相邻 array layer 的 byte 距离。
        layer_stride: u32,
    },
    /// 让 scanout 直接引用一个 GPU resource。
    SetScanout {
        /// 当前 connector mode。
        mode: DisplayMode,
        /// 已创建的 GPU resource identity；零表示禁用。
        resource_id: u32,
    },
    /// 发布 resource 已完成渲染的矩形。
    Flush {
        /// 已创建的 GPU resource identity。
        resource_id: u32,
        /// scanout 坐标系中的非空矩形。
        rectangle: DisplayRect,
    },
    /// 释放一个 host resource。
    ResourceUnref {
        /// 已创建的 resource identity。
        resource_id: u32,
    },
}

/// @description 同时拥有 scanout 与 VirGL controlq 的唯一 graphics adapter seam。
pub(crate) trait GraphicsDevice: DisplayDevice {
    /// @description 把一个 64x64 ARGB dumb buffer 上传到规范要求的 2D cursor resource。
    /// @param identity DRM dumb buffer 的全局单调 identity。
    /// @param backing cursor pixels 的 SG lifetime owner。
    /// @return CREATE/ATTACH/TRANSFER 完整完成后发布的 adapter fence。
    /// @errors 已有非 render control transaction 时返回 `WouldBlock`；几何或 device failure
    /// 返回稳定 display error。
    fn submit_cursor_upload(
        &self,
        identity: u64,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError>;

    /// @description 向独立 cursorq 提交一条需要 exact completion 的光标图像更新。
    /// @param command 已由 DRM 验证 resource、geometry 与 master ownership 的命令。
    /// @return cursorq 单调 completion sequence。
    /// @errors 前一条 cursor command 尚未完成时返回 `WouldBlock`；queue 或 device failure
    /// 返回 `Device`。
    fn submit_cursor(&self, command: CursorCommand) -> Result<u64, DisplayError>;

    /// @description 异步移动硬件光标；cursorq 忙时由 adapter 覆盖尚未发布的旧坐标。
    /// @param x scanout 0 中的水平位置。
    /// @param y scanout 0 中的垂直位置。
    /// @return adapter 接受当前最新位置后返回 unit，不等待 device completion。
    /// @errors queue 或 device failure 返回 `Device`。
    fn move_cursor(&self, x: u32, y: u32) -> Result<(), DisplayError>;

    /// @description 返回 adapter 初始化时固定选中的 VirGL capset。
    /// @return host 未提供 VirGL 时为 `None`；有能力时返回 exact identity/version/size。
    fn virgl_capset_info(&self) -> Option<VirglCapsetInfo>;

    /// @description 返回标准 `VIRTIO_GPU_F_CONTEXT_INIT` 是否已协商。
    /// @return true 表示 userspace 可用 `DRM_IOCTL_VIRTGPU_CONTEXT_INIT` 显式选择 capset；
    /// false 时 VirGL context 仍可按 legacy `context_init=0` 惰性创建。
    fn supports_virgl_context_init(&self) -> bool;

    /// @description 复制 adapter 初始化时缓存的 immutable VirGL capset。
    /// @param output userspace UAPI 已验证长度后提供的 kernel output buffer。
    /// @return exact capability byte 数。
    /// @errors output 小于 capset size 或 adapter 不支持 VirGL 时返回 `Device`。
    fn copy_virgl_capset(&self, output: &mut [u8]) -> Result<usize, DisplayError>;

    /// @description 向唯一 controlq 异步提交一条标准 VirGL operation。
    /// @param command 已由 DRM domain 验证 ownership 与参数的 operation。
    /// @return 可由 DRM wait 观察的单调 adapter fence。
    /// @errors 已有 command 在途返回 `WouldBlock`；编码、queue 或 device failure 返回
    /// `Device`。
    fn submit_virgl(&self, command: VirglCommand<'_>) -> Result<u64, DisplayError>;

    /// @description 原子执行 GPU resource 的 SET_SCANOUT → RESOURCE_FLUSH presentation chain。
    /// @param mode 当前 CRTC mode；resource geometry 必须完全一致。
    /// @param resource_id 已由同一 VirGL context 完成渲染的 host resource。
    /// @return 只在两个 command 都完成后发布的单一 operation fence。
    /// @errors controlq busy、mode/resource 非法或 device failure。
    fn submit_virgl_scanout(
        &self,
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<u64, DisplayError>;
}
