use alloc::{format, string::String, vec::Vec};

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
    
    // 解析ELF符号表来添加详细的函数符号
    try_parse_debug_info();
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

/// ELF64 符号表项 (24 bytes on 64-bit)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ElfSymbol {
    name: u32,      // Symbol name (string table index) - 4 bytes
    info: u8,       // Symbol type and binding - 1 byte  
    other: u8,      // Symbol visibility - 1 byte
    shndx: u16,     // Section index - 2 bytes
    value: u64,     // Symbol value (address) - 8 bytes
    size: u64,      // Symbol size - 8 bytes
}

/// 使用内联汇编获取符号地址，避免PC相对寻址问题
#[inline(never)]
unsafe fn get_symbol_addresses() -> (usize, usize, usize, usize) {
    let mut ssymtab_addr: usize;
    let mut esymtab_addr: usize;
    let mut sstrtab_addr: usize;
    let mut estrtab_addr: usize;
    
    // 使用内联汇编加载绝对地址
    core::arch::asm!(
        "lui {ssymtab}, %hi(ssymtab)",
        "addi {ssymtab}, {ssymtab}, %lo(ssymtab)",
        "lui {esymtab}, %hi(esymtab)", 
        "addi {esymtab}, {esymtab}, %lo(esymtab)",
        "lui {sstrtab}, %hi(sstrtab)",
        "addi {sstrtab}, {sstrtab}, %lo(sstrtab)",
        "lui {estrtab}, %hi(estrtab)",
        "addi {estrtab}, {estrtab}, %lo(estrtab)",
        ssymtab = out(reg) ssymtab_addr,
        esymtab = out(reg) esymtab_addr,
        sstrtab = out(reg) sstrtab_addr,
        estrtab = out(reg) estrtab_addr,
        options(pure, nomem, nostack)
    );
    
    (ssymtab_addr, esymtab_addr, sstrtab_addr, estrtab_addr)
}

/// 解析内核ELF符号表
pub fn try_parse_debug_info() {
    info!("Parsing kernel ELF symbol table...");
    
    unsafe {
        let (symtab_start, symtab_end, strtab_start, strtab_end) = get_symbol_addresses();
        
        debug!("Symbol table: {:#x} - {:#x} (size: {})", 
               symtab_start, symtab_end, symtab_end - symtab_start);
        debug!("String table: {:#x} - {:#x} (size: {})", 
               strtab_start, strtab_end, strtab_end - strtab_start);
               
        if symtab_end > symtab_start && strtab_end > strtab_start {
            parse_symbol_table(symtab_start, symtab_end, strtab_start, strtab_end);
        } else {
            warn!("No valid symbol table found, using minimal symbols only");
        }
    }
}

/// 解析符号表并添加到全局符号表
unsafe fn parse_symbol_table(
    symtab_start: usize,
    symtab_end: usize,
    strtab_start: usize,
    strtab_end: usize,
) {
    let symtab_size = symtab_end - symtab_start;
    let symbol_count = symtab_size / core::mem::size_of::<ElfSymbol>();
    
    info!("Found {} symbols in symbol table", symbol_count);
    
    // 获取当前的符号表，如果不存在则创建新的
    let table_ptr = core::ptr::addr_of_mut!(SYMBOL_TABLE);
    unsafe {
        if let Some(table) = &mut *table_ptr {
            let symbols = unsafe {
                core::slice::from_raw_parts(
                    symtab_start as *const ElfSymbol,
                    symbol_count
                )
            };
            
            let strtab = unsafe {
                core::slice::from_raw_parts(
                    strtab_start as *const u8,
                    strtab_end - strtab_start
                )
            };
            
            let mut added_count = 0;
            
            for symbol in symbols {
                // 只处理函数和对象符号
                let symbol_type = symbol.info & 0xf;
                if symbol_type == 1 || symbol_type == 2 { // STT_OBJECT or STT_FUNC
                    if let Some(name) = unsafe { get_symbol_name(strtab, symbol.name as usize) } {
                        // 过滤掉一些不需要的符号
                        if !name.is_empty() && 
                           !name.starts_with('.') && 
                           !name.starts_with('_') &&
                           symbol.value > 0 {
                            table.add_symbol(name, symbol.value as usize, symbol.size as usize);
                            added_count += 1;
                        }
                    }
                }
            }
            
            info!("Added {} symbols from ELF symbol table", added_count);
        }
    }
}

/// 从字符串表中获取符号名称
unsafe fn get_symbol_name(strtab: &[u8], offset: usize) -> Option<String> {
    if offset >= strtab.len() {
        return None;
    }
    
    // 查找以null结尾的字符串
    let mut end = offset;
    while end < strtab.len() && strtab[end] != 0 {
        end += 1;
    }
    
    if end > offset {
        // 将字节转换为字符串
        if let Ok(name) = core::str::from_utf8(&strtab[offset..end]) {
            Some(String::from(name))
        } else {
            None
        }
    } else {
        None
    }
}