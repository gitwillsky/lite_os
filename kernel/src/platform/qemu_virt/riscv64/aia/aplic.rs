//! Supervisor APLIC 的寄存器 owner；只访问 DTB 交付的 S-mode domain。
use mmio::MmioBus;

pub(super) struct Aplic {
    bus: MmioBus,
    pub(super) sources: u32,
}
impl Aplic {
    pub(super) fn new(range: core::ops::Range<usize>, sources: u32) -> Self {
        let bus = MmioBus::new(
            crate::arch::mmu::physical_to_virtual(range.start),
            range.end - range.start,
        )
        .expect("APLIC window");
        let result = Self { bus, sources };
        result.write(0, result.read(0) & !0x101 | 4);
        for word in 0..=sources / 32 {
            result.write(0x1f00 + word as usize * 4, u32::MAX);
            result.write(0x1d00 + word as usize * 4, u32::MAX);
        }
        for source in 1..=sources {
            result.configure(source, None);
            result.target(source, 0, 0);
        }
        assert_eq!(
            result.read(0) & 0x105,
            4,
            "APLIC must support little-endian MSI delivery"
        );
        result
    }
    pub(super) fn start(&self) {
        self.write(0, self.read(0) | 0x104);
        assert_eq!(self.read(0) & 0x105, 0x104, "APLIC delivery enable");
    }
    pub(super) fn configure(&self, source: u32, level: Option<bool>) {
        self.write(
            4 + (source - 1) as usize * 4,
            level.map_or(0, |level| if level { 6 } else { 4 }),
        );
    }
    pub(super) fn target(&self, source: u32, hart: u32, id: u32) {
        self.write(0x3004 + (source - 1) as usize * 4, hart << 18 | id);
    }
    pub(super) fn enable(&self, source: u32, enabled: bool) {
        self.write(if enabled { 0x1edc } else { 0x1fdc }, source);
    }
    pub(super) fn retrigger(&self, source: u32) {
        self.write(0x1cdc, source);
    }
    /// GENMSI 标准 fence marker；冷配置 owner 串行化，不能与另一 marker 交错。
    pub(super) fn marker(&self, hart: u32, progress: impl Fn()) {
        wait(|| self.read(0x3000) & (1 << 12) == 0, &progress);
        self.write(0x3000, hart << 18 | 1);
        wait(|| self.read(0x3000) & (1 << 12) == 0, progress);
    }
    fn read(&self, offset: usize) -> u32 {
        self.bus.read_u32(offset).expect("APLIC register")
    }
    fn write(&self, offset: usize, value: u32) {
        self.bus.write_u32(offset, value).expect("APLIC register");
    }
}
pub(super) fn wait(mut done: impl FnMut() -> bool, progress: impl Fn()) {
    let deadline = crate::arch::time::counter().saturating_add(super::super::timebase_frequency());
    while !done() {
        progress();
        assert!(
            crate::arch::time::counter() < deadline,
            "AIA synchronization timed out; retain routing owners"
        );
        core::hint::spin_loop();
    }
}
