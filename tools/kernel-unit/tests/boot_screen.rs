#![allow(dead_code)]

extern crate alloc;

use std::cell::RefCell;

macro_rules! warn {
    ($($argument:tt)*) => {
        $crate::WARNINGS.with(|log| log.borrow_mut().push(format!($($argument)*)))
    };
}

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// 内核 `drivers` seam 里 boot_screen 用到的那一部分；字段与语义和生产类型一致。
mod drivers {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct DisplayMode {
        pub(crate) width: u32,
        pub(crate) height: u32,
        pub(crate) pitch: u32,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct DisplayRect {
        pub(crate) x: u32,
        pub(crate) y: u32,
        pub(crate) width: u32,
        pub(crate) height: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DisplayError {
        WouldBlock,
        InvalidRectangle,
        Device,
    }

    pub(crate) trait BootFramebuffer {
        fn mode(&self) -> DisplayMode;
        fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), DisplayError>;
        fn flush(&self, rect: DisplayRect) -> Result<(), DisplayError>;
    }
}

#[path = "../../../kernel/src/boot_screen.rs"]
mod boot_screen;

use std::cell::Cell;

use boot_screen::{BootScreen, Milestone};
use drivers::{BootFramebuffer, DisplayError, DisplayMode, DisplayRect};

/// 仓库提交的默认画面，与内核实际编入的格式一致。
const SCENE: &[u8] = include_bytes!("../../../assets/classic/boot-scene.lbs");

/// 把写入落在一块内存里的假 scanout，并记录每次 flush；可以在第 N 次 flush 起模拟 DRM 已接管。
struct FakeScanout {
    mode: DisplayMode,
    memory: Cell<Vec<u8>>,
    flushes: Cell<Vec<DisplayRect>>,
    refuse_flush_from: Cell<Option<usize>>,
}

impl FakeScanout {
    fn new(width: u32, height: u32) -> Self {
        let pitch = width * 4;
        Self {
            mode: DisplayMode { width, height, pitch },
            memory: Cell::new(vec![0xAA; (pitch * height) as usize]),
            flushes: Cell::new(Vec::new()),
            refuse_flush_from: Cell::new(None),
        }
    }

    fn pixel(&self, x: usize, y: usize) -> u32 {
        let memory = self.memory.take();
        let offset = y * self.mode.pitch as usize + x * 4;
        let value = u32::from_le_bytes(memory[offset..offset + 4].try_into().unwrap());
        self.memory.set(memory);
        value
    }
}

impl BootFramebuffer for FakeScanout {
    fn mode(&self) -> DisplayMode {
        self.mode
    }

    fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), DisplayError> {
        let mut memory = self.memory.take();
        let result = match memory.get_mut(offset..offset + bytes.len()) {
            Some(target) => {
                target.copy_from_slice(bytes);
                Ok(())
            }
            None => Err(DisplayError::InvalidRectangle),
        };
        self.memory.set(memory);
        result
    }

    fn flush(&self, rect: DisplayRect) -> Result<(), DisplayError> {
        let mut flushes = self.flushes.take();
        let refuse = self.refuse_flush_from.get().is_some_and(|from| flushes.len() >= from);
        if !refuse {
            flushes.push(rect);
        }
        self.flushes.set(flushes);
        if refuse { Err(DisplayError::WouldBlock) } else { Ok(()) }
    }
}

fn flush_count(scanout: &FakeScanout) -> usize {
    let flushes = scanout.flushes.take();
    let count = flushes.len();
    scanout.flushes.set(flushes);
    count
}

fn last_flush(scanout: &FakeScanout) -> DisplayRect {
    let flushes = scanout.flushes.take();
    let last = *flushes.last().expect("a flush");
    scanout.flushes.set(flushes);
    last
}

#[test]
fn start_clears_the_whole_screen_draws_the_centered_scene_and_flushes_it_once() {
    let scanout = FakeScanout::new(1504, 846);
    let _screen = BootScreen::start(Some(&scanout), SCENE);
    // 整屏清黑：旧内容（0xAA）不能留在位图之外。
    assert_eq!(scanout.pixel(0, 0), 0);
    assert_eq!(scanout.pixel(1503, 845), 0);
    // 1504×846 下 1 倍，位图左上角在 (432, 183)；进度槽凹边的两个角点。
    assert_eq!(scanout.pixel(432 + 273, 183 + 436), 0x0080_8080);
    assert_eq!(scanout.pixel(432 + 437, 183 + 445), 0x00FF_FFFF);
    assert_eq!(flush_count(&scanout), 1);
    assert_eq!(last_flush(&scanout), DisplayRect { x: 0, y: 0, width: 1504, height: 846 });
}

#[test]
fn milestones_only_paint_the_newly_covered_part_of_the_slot() {
    let scanout = FakeScanout::new(1504, 846);
    let mut screen = BootScreen::start(Some(&scanout), SCENE);
    screen.advance(Milestone::Namespaces);
    // 100‰ × 163 = 16 像素。
    assert_eq!(last_flush(&scanout), DisplayRect { x: 432 + 274, y: 183 + 437, width: 16, height: 8 });
    assert_eq!(scanout.pixel(432 + 274 + 15, 183 + 437), 0x0000_299C);
    assert_eq!(scanout.pixel(432 + 274 + 16, 183 + 437), 0x00C0_C0C0);
    screen.advance(Milestone::Audio);
    // 400‰ × 163 = 65：只重画 16..65 这新增的 49 个像素，已画部分不再提交。
    assert_eq!(
        last_flush(&scanout),
        DisplayRect { x: 432 + 274 + 16, y: 183 + 437, width: 49, height: 8 }
    );
    assert_eq!(scanout.pixel(432 + 274 + 64, 183 + 444), 0x0000_299C);
    assert_eq!(scanout.pixel(432 + 274 + 65, 183 + 444), 0x00C0_C0C0);
}

#[test]
fn repeating_or_regressing_a_milestone_never_touches_the_screen_again() {
    let scanout = FakeScanout::new(1504, 846);
    let mut screen = BootScreen::start(Some(&scanout), SCENE);
    screen.advance(Milestone::Devices);
    let flushes = flush_count(&scanout);
    screen.advance(Milestone::Devices);
    screen.advance(Milestone::Namespaces); // 进度只增不减
    assert_eq!(flush_count(&scanout), flushes);
}

#[test]
fn large_screens_scale_by_an_integer_and_keep_the_slot_aligned() {
    let scanout = FakeScanout::new(3008, 1692);
    let mut screen = BootScreen::start(Some(&scanout), SCENE);
    // scale 3，位图左上角 (544, 126)；每个源像素是 3×3 方块。
    assert_eq!(scanout.pixel(544 + 273 * 3 + 2, 126 + 436 * 3 + 2), 0x0080_8080);
    screen.advance(Milestone::Audio);
    // 400‰ × 163 × 3 = 195（整除后向下取整）。
    assert_eq!(
        last_flush(&scanout),
        DisplayRect { x: 544 + 274 * 3, y: 126 + 437 * 3, width: 195, height: 24 }
    );
}

#[test]
fn no_display_or_unusable_scene_is_a_silent_no_op_that_never_blocks_boot() {
    let mut without_display = BootScreen::start(None, SCENE);
    without_display.advance(Milestone::Audio);

    let tiny = FakeScanout::new(320, 200);
    let mut screen = BootScreen::start(Some(&tiny), SCENE);
    screen.advance(Milestone::Audio);
    assert_eq!(flush_count(&tiny), 0);

    let scanout = FakeScanout::new(1504, 846);
    let mut broken = BootScreen::start(Some(&scanout), b"not a scene");
    broken.advance(Milestone::Audio);
    assert_eq!(flush_count(&scanout), 0);
    let warnings = WARNINGS.with(|log| log.borrow().clone());
    assert!(warnings.iter().any(|line| line.contains("smaller than 640x480")), "{warnings:?}");
    assert!(warnings.iter().any(|line| line.contains("invalid scene bitmap")), "{warnings:?}");
}

#[test]
fn once_the_display_refuses_updates_progress_stops_for_good() {
    let scanout = FakeScanout::new(1504, 846);
    let mut screen = BootScreen::start(Some(&scanout), SCENE);
    screen.advance(Milestone::Namespaces);
    // 模拟 DRM 接管：此后所有 flush 返回 WouldBlock。
    scanout.refuse_flush_from.set(Some(flush_count(&scanout)));
    screen.advance(Milestone::Scheduler);
    let seen = flush_count(&scanout);
    screen.advance(Milestone::Audio); // 已经停止，不再尝试
    assert_eq!(flush_count(&scanout), seen);
    let warnings = WARNINGS.with(|log| log.borrow().clone());
    assert!(warnings.iter().any(|line| line.contains("progress stopped")), "{warnings:?}");
}

/// 整帧 FNV-1a：把每个像素按 0x00RRGGBB 的四个字节（小端）依次喂入。
fn frame_hash(scanout: &FakeScanout) -> u64 {
    let width = scanout.mode.width as usize;
    let height = scanout.mode.height as usize;
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for y in 0..height {
        for x in 0..width {
            for byte in scanout.pixel(x, y).to_le_bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
    }
    hash
}

/// 基准值由 `scripts/tests/test_boot_scene_handoff.py` 的独立 Python 实现按契约算出；compositor 的测试断言同样的值，
/// 三方一致才说明内核到 compositor 的接管逐像素连续。任何一方的缩放、居中或填充规则漂移，这里立刻失败。
#[test]
fn full_frame_matches_the_contract_hash_shared_with_the_compositor() {
    for (width, height, expected) in [
        (1504, 846, 0xaf73_2633_392f_568e_u64),
        (3008, 1692, 0xbd09_5a70_f144_ef5a_u64),
    ] {
        let scanout = FakeScanout::new(width, height);
        let mut screen = BootScreen::start(Some(&scanout), SCENE);
        screen.advance(Milestone::Audio);
        assert_eq!(frame_hash(&scanout), expected, "{width}x{height}");
    }
}
