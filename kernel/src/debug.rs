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

/// 获取符号地址，使用链接器符号
fn get_symbol_addresses() -> (usize, usize, usize, usize) {
    unsafe extern "C" {
        fn ssymtab();
        fn esymtab();
        fn sstrtab();
        fn estrtab();
    }
    
    unsafe {
        let ssymtab_addr = ssymtab as *const () as usize;
        let esymtab_addr = esymtab as *const () as usize;
        let sstrtab_addr = sstrtab as *const () as usize;
        let estrtab_addr = estrtab as *const () as usize;
        
        (ssymtab_addr, esymtab_addr, sstrtab_addr, estrtab_addr)
    }
}

/// 解析内核ELF符号表
pub fn try_parse_debug_info() {
    info!("Parsing kernel ELF symbol table...");
    
    let (symtab_start, symtab_end, strtab_start, strtab_end) = get_symbol_addresses();
    
    info!("Symbol table: {:#x} - {:#x} (size: {})", 
           symtab_start, symtab_end, symtab_end.wrapping_sub(symtab_start));
    info!("String table: {:#x} - {:#x} (size: {})", 
           strtab_start, strtab_end, strtab_end.wrapping_sub(strtab_start));
    
    // 验证地址范围和对齐
    let symtab_size = symtab_end.wrapping_sub(symtab_start);
    let strtab_size = strtab_end.wrapping_sub(strtab_start);
    
    if symtab_start == 0 || strtab_start == 0 {
        warn!("Symbol or string table has null address");
        return;
    }
    
    if symtab_size == 0 || strtab_size == 0 {
        warn!("Symbol or string table has zero size");
        return;
    }
    
    if symtab_size > (isize::MAX as usize) || strtab_size > (isize::MAX as usize) {
        warn!("Symbol or string table size exceeds isize::MAX");
        return;
    }
    
    // 检查符号表指针是否正确对齐
    if symtab_start % core::mem::align_of::<ElfSymbol>() != 0 {
        warn!("Symbol table not properly aligned");
        return;
    }
           
    if symtab_end > symtab_start && strtab_end > strtab_start {
        unsafe {
            parse_symbol_table(symtab_start, symtab_end, strtab_start, strtab_end);
        }
    } else {
        warn!("No valid symbol table found, using minimal symbols only");
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
    let strtab_size = strtab_end - strtab_start;
    let symbol_count = symtab_size / core::mem::size_of::<ElfSymbol>();
    
    info!("Found {} symbols in symbol table", symbol_count);
    debug!("String table size: {}", strtab_size);
    
    // 验证字符串表的合理性
    if strtab_size > 10 * 1024 * 1024 { // 限制在10MB以内
        warn!("String table too large: {} bytes", strtab_size);
        return;
    }
    
    // 获取当前的符号表，如果不存在则创建新的
    let table_ptr = core::ptr::addr_of_mut!(SYMBOL_TABLE);
    unsafe {
        if let Some(table) = &mut *table_ptr {
            // 安全地创建符号表切片
            let symbols = unsafe {
                core::slice::from_raw_parts(
                    symtab_start as *const ElfSymbol,
                    symbol_count
                )
            };
            
            // 安全地创建字符串表切片，添加额外检查
            let strtab = if strtab_start.checked_add(strtab_size).is_some() && strtab_size > 0 {
                unsafe {
                    core::slice::from_raw_parts(
                        strtab_start as *const u8,
                        strtab_size
                    )
                }
            } else {
                warn!("Invalid string table bounds");
                return;
            };
            
            let mut added_count = 0;
            let mut processed_count = 0;
            
            for symbol in symbols.iter().take(1000) { // 限制处理前1000个符号以避免过长的处理时间
                processed_count += 1;
                
                // 只处理函数和对象符号
                let symbol_type = symbol.info & 0xf;
                if symbol_type == 1 || symbol_type == 2 { // STT_OBJECT or STT_FUNC
                    if let Some(name) = get_symbol_name(strtab, symbol.name as usize) {
                        // 过滤掉一些不需要的符号
                        if !name.is_empty() && 
                           !name.starts_with('.') && 
                           !name.starts_with('_') &&
                           symbol.value > 0 && 
                           name.len() < 256 { // 限制符号名称长度
                            table.add_symbol(name, symbol.value as usize, symbol.size as usize);
                            added_count += 1;
                            
                            // 限制添加的符号数量以避免内存过度使用
                            if added_count >= 100 {
                                break;
                            }
                        }
                    }
                }
            }
            
            info!("Processed {} symbols, added {} symbols from ELF symbol table", processed_count, added_count);
        }
    }
}

/// 从字符串表中获取符号名称
fn get_symbol_name(strtab: &[u8], offset: usize) -> Option<String> {
    // 检查偏移是否在合理范围内
    if offset >= strtab.len() || offset == 0 {
        return None;
    }
    
    // 限制搜索长度，避免无限循环
    let max_search_len = core::cmp::min(strtab.len() - offset, 256);
    
    // 查找以null结尾的字符串
    let mut end = offset;
    let search_end = offset + max_search_len;
    
    while end < search_end && end < strtab.len() && strtab[end] != 0 {
        end += 1;
    }
    
    // 确保找到了null终止符
    if end < strtab.len() && strtab[end] == 0 && end > offset {
        // 将字节转换为字符串
        if let Ok(name) = core::str::from_utf8(&strtab[offset..end]) {
            // 过滤掉非打印字符和过长的名称
            if name.chars().all(|c| c.is_ascii_graphic() || c == '_') && name.len() < 128 {
                Some(String::from(name))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    }
}