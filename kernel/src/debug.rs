use alloc::{format, string::String, vec::Vec};


// 包含构建时生成的符号文件
include!(concat!(env!("OUT_DIR"), "/symbols.rs"));



/// 符号信息结构
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub addr: usize,
    pub size: usize,
}

/// 简单的符号表，用于地址到函数名的映射
pub struct SymbolTable {
    symbols: Vec<Symbol>,
}

impl SymbolTable {
    /// 创建新的符号表
    pub fn new() -> Self {
        Self {
            symbols: Vec::new(),
        }
    }

    /// 添加符号
    pub fn add_symbol(&mut self, name: String, addr: usize, size: usize) {
        self.symbols.push(Symbol { name, addr, size });
        // 保持按地址排序
        self.symbols.sort_by_key(|s| s.addr);
    }

    /// 根据地址查找最接近的符号
    pub fn find_symbol(&self, addr: usize) -> Option<&Symbol> {
        // 二分查找最接近的符号
        let mut left = 0;
        let mut right = self.symbols.len();
        let mut best_match: Option<&Symbol> = None;

        while left < right {
            let mid = (left + right) / 2;
            let symbol = &self.symbols[mid];

            if addr >= symbol.addr && addr < symbol.addr + symbol.size {
                return Some(symbol);
            } else if addr >= symbol.addr {
                best_match = Some(symbol);
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        best_match
    }

    /// 格式化地址显示，包含符号信息
    pub fn format_address(&self, addr: usize) -> String {
        if let Some(symbol) = self.find_symbol(addr) {
            let offset = addr - symbol.addr;
            if offset == 0 {
                format!("{:#x} <{}>", addr, symbol.name)
            } else {
                format!("{:#x} <{}+{:#x}>", addr, symbol.name, offset)
            }
        } else {
            format!("{:#x} <unknown>", addr)
        }
    }
}

/// 全局符号表实例
static mut SYMBOL_TABLE: Option<SymbolTable> = None;

/// 初始化符号表
pub fn init_symbol_table() {
    info!("Initializing symbol table...");
    let mut table = SymbolTable::new();

    // 添加基本的内存段信息
    unsafe extern "C" {
        fn stext();
        fn etext();
        fn sdata();
        fn edata();
        fn sbss();
        fn ebss();
    }

    unsafe {
        // 添加内存段
        table.add_symbol(String::from(".text"), stext as *const () as usize, etext as usize - stext as usize);
        table.add_symbol(String::from(".data"), sdata as *const () as usize, edata as usize - sdata as usize);
        table.add_symbol(String::from(".bss"), sbss as *const () as usize, ebss as usize - sbss as usize);

        SYMBOL_TABLE = Some(table);
    }

    info!("Basic symbol table created, parsing ELF symbols...");

    // 解析ELF符号表来添加详细的函数符号
    try_parse_debug_info();

    info!("Symbol table initialization complete");
}

/// 获取符号表引用
pub fn get_symbol_table() -> Option<&'static SymbolTable> {
    unsafe {
        let ptr = core::ptr::addr_of!(SYMBOL_TABLE);
        (*ptr).as_ref()
    }
}

/// 格式化地址，包含符号信息
pub fn format_address(addr: usize) -> String {
    if let Some(table) = get_symbol_table() {
        table.format_address(addr)
    } else {
        format!("{:#x} <no symbols>", addr)
    }
}


/// 加载构建时生成的符号信息
pub fn try_parse_debug_info() {
    info!("Loading kernel symbol information...");

    // 使用构建时生成的符号信息，避免RISC-V重定位问题
    load_build_time_symbols();

    info!("Symbol information loaded successfully");
}

/// 加载构建时生成的符号
fn load_build_time_symbols() {
    // 获取当前的符号表并填充符号
    let table_ptr = core::ptr::addr_of_mut!(SYMBOL_TABLE);
    unsafe {
        if let Some(table) = &mut *table_ptr {
            populate_symbol_table(table);
        }
    }
}

