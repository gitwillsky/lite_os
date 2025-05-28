use dtb::BoardInfo;
use spin::Once;

mod dtb;

static BOARD_INFO: Once<BoardInfo> = Once::new();

pub fn init(dtb_addr: usize) {
    BOARD_INFO.call_once(|| BoardInfo::parse(dtb_addr));
}

pub fn get_board_info() -> &'static BoardInfo {
    BOARD_INFO.wait()
}
