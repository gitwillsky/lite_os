use spin::Once;

mod dtb;

// 重新导出 RTCDevice 和 BoardInfo 供其他模块使用
pub use dtb::{RTCDevice, BoardInfo};

static BOARD_INFO: Once<BoardInfo> = Once::new();

pub fn init(dtb_addr: usize) {
    // 启用浮点支持
    unsafe {
        use riscv::register::sstatus;
        sstatus::set_fs(sstatus::FS::Dirty);
    }

    BOARD_INFO.call_once(|| BoardInfo::parse(dtb_addr));
}

pub fn board_info() -> &'static BoardInfo {
    BOARD_INFO.wait()
}
