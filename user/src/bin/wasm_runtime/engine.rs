//! WASM引擎模块 - 实现WASM字节码的解析和执行

use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;

/// WASM魔数和版本
const WASM_MAGIC: u32 = 0x6d736100; // '\0asm'
const WASM_VERSION: u32 = 1;

/// WASM段类型
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionType {
    Custom = 0,
    Type = 1,
    Import = 2,
    Function = 3,
    Table = 4,
    Memory = 5,
    Global = 6,
    Export = 7,
    Start = 8,
    Element = 9,
    Code = 10,
    Data = 11,
}

/// WASM值类型
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    I32 = 0x7F,
    I64 = 0x7E,
    F32 = 0x7D,
    F64 = 0x7C,
}

/// WASM函数类型
#[derive(Debug, Clone)]
pub struct FunctionType {
    pub params: Vec<ValueType>,
    pub results: Vec<ValueType>,
}

/// WASM函数导入
#[derive(Debug, Clone)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: ImportKind,
}

/// 导入类型
#[derive(Debug, Clone)]
pub enum ImportKind {
    Function(u32), // 类型索引
    Table,
    Memory,
    Global,
}

/// WASM函数导出
#[derive(Debug, Clone)]
pub struct Export {
    pub name: String,
    pub kind: ExportKind,
    pub index: u32,
}

/// 导出类型
#[derive(Debug, Clone)]
pub enum ExportKind {
    Function,
    Table,
    Memory,
    Global,
}

/// WASM函数体
#[derive(Debug, Clone)]
pub struct Function {
    pub type_index: u32,
    pub locals: Vec<ValueType>,
    pub body: Vec<u8>,
}

/// WASM模块
#[derive(Debug)]
pub struct WasmModule {
    pub types: Vec<FunctionType>,
    pub imports: Vec<Import>,
    pub functions: Vec<u32>, // 类型索引
    pub exports: Vec<Export>,
    pub start_function: Option<u32>,
    pub code: Vec<Function>,
    pub memory_min: Option<u32>,
    pub memory_max: Option<u32>,
}

/// WASM模块解析器
pub struct WasmParser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> WasmParser<'a> {
    /// 创建新的解析器
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    
    /// 解析WASM模块
    pub fn parse_module(&mut self) -> Result<WasmModule, String> {
        // 检查魔数
        let magic = self.read_u32()?;
        if magic != WASM_MAGIC {
            return Err(alloc::format!("Invalid WASM magic: 0x{:08x}", magic));
        }
        
        // 检查版本
        let version = self.read_u32()?;
        if version != WASM_VERSION {
            return Err(alloc::format!("Unsupported WASM version: {}", version));
        }
        
        println!("WASM module header valid: magic=0x{:08x}, version={}", magic, version);
        
        let mut module = WasmModule {
            types: Vec::new(),
            imports: Vec::new(),
            functions: Vec::new(),
            exports: Vec::new(),
            start_function: None,
            code: Vec::new(),
            memory_min: None,
            memory_max: None,
        };
        
        // 解析所有段
        while !self.is_at_end() {
            self.parse_section(&mut module)?;
        }
        
        println!("WASM module parsed successfully:");
        println!("  Types: {}", module.types.len());
        println!("  Imports: {}", module.imports.len());
        println!("  Functions: {}", module.functions.len());
        println!("  Exports: {}", module.exports.len());
        println!("  Code sections: {}", module.code.len());
        
        Ok(module)
    }
    
    /// 解析段
    fn parse_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let section_id = self.read_u8()?;
        let section_size = self.read_uleb128()? as usize;
        let section_start = self.pos;
        
        println!("Parsing section {} (size: {} bytes)", section_id, section_size);
        
        match section_id {
            1 => self.parse_type_section(module)?,
            2 => self.parse_import_section(module)?,
            3 => self.parse_function_section(module)?,
            5 => self.parse_memory_section(module)?,
            7 => self.parse_export_section(module)?,
            8 => self.parse_start_section(module)?,
            10 => self.parse_code_section(module)?,
            _ => {
                // 跳过未知段
                println!("Skipping unknown section {}", section_id);
            }
        }
        
        // 确保正确跳过整个段
        self.pos = section_start + section_size;
        Ok(())
    }
    
    /// 解析类型段
    fn parse_type_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let count = self.read_uleb128()?;
        println!("  Type section: {} types", count);
        
        for _ in 0..count {
            let form = self.read_u8()?;
            if form != 0x60 { // func type
                return Err(alloc::format!("Unsupported type form: 0x{:02x}", form));
            }
            
            let param_count = self.read_uleb128()?;
            let mut params = Vec::new();
            for _ in 0..param_count {
                params.push(self.read_value_type()?);
            }
            
            let result_count = self.read_uleb128()?;
            let mut results = Vec::new();
            for _ in 0..result_count {
                results.push(self.read_value_type()?);
            }
            
            module.types.push(FunctionType { params, results });
        }
        
        Ok(())
    }
    
    /// 解析导入段
    fn parse_import_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let count = self.read_uleb128()?;
        println!("  Import section: {} imports", count);
        
        for i in 0..count {
            println!("Parsing import {}/{}", i + 1, count);
            let module_name = self.read_string()?;
            println!("  Module name: {}", module_name);
            let name = self.read_string()?;
            println!("  Import name: {}", name);
            let kind = self.read_u8()?;
            println!("  Import kind byte: {} (0x{:02x})", kind, kind);
            
            let import_kind = match kind {
                0 => {
                    let type_idx = self.read_uleb128()?;
                    println!("  Function import, type index: {}", type_idx);
                    ImportKind::Function(type_idx)
                },
                1 => {
                    println!("  Table import");
                    ImportKind::Table
                },
                2 => {
                    println!("  Memory import");
                    ImportKind::Memory
                },
                3 => {
                    println!("  Global import");
                    ImportKind::Global
                },
                _ => {
                    println!("  ERROR: Unknown import kind: {} (0x{:02x}) at position {}", kind, kind, self.pos - 1);
                    println!("  This WASM file may be too complex for our simple runtime");
                    println!("  Treating as unsupported function import");
                    // 将未知类型视为函数导入，跳过类型索引
                    ImportKind::Function(0)
                },
            };
            
            module.imports.push(Import {
                module: module_name,
                name,
                kind: import_kind,
            });
        }
        
        Ok(())
    }
    
    /// 解析函数段
    fn parse_function_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let count = self.read_uleb128()?;
        println!("  Function section: {} functions", count);
        
        for _ in 0..count {
            let type_index = self.read_uleb128()?;
            module.functions.push(type_index);
        }
        
        Ok(())
    }
    
    /// 解析内存段
    fn parse_memory_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let count = self.read_uleb128()?;
        println!("  Memory section: {} memories", count);
        
        if count > 0 {
            let limits_type = self.read_u8()?;
            let min = self.read_uleb128()?;
            let max = if limits_type & 1 != 0 {
                Some(self.read_uleb128()?)
            } else {
                None
            };
            
            module.memory_min = Some(min);
            module.memory_max = max;
            
            println!("    Memory: min={}, max={:?}", min, max);
        }
        
        Ok(())
    }
    
    /// 解析导出段
    fn parse_export_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let count = self.read_uleb128()?;
        println!("  Export section: {} exports", count);
        
        for _ in 0..count {
            let name = self.read_string()?;
            let kind = self.read_u8()?;
            let index = self.read_uleb128()?;
            
            let export_kind = match kind {
                0 => ExportKind::Function,
                1 => ExportKind::Table,
                2 => ExportKind::Memory,
                3 => ExportKind::Global,
                _ => return Err(alloc::format!("Unknown export kind: {}", kind)),
            };
            
            println!("    Export: {} -> {:?}[{}]", name, export_kind, index);
            
            module.exports.push(Export {
                name,
                kind: export_kind,
                index,
            });
        }
        
        Ok(())
    }
    
    /// 解析启动段
    fn parse_start_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let start_func = self.read_uleb128()?;
        println!("  Start section: function {}", start_func);
        module.start_function = Some(start_func);
        Ok(())
    }
    
    /// 解析代码段
    fn parse_code_section(&mut self, module: &mut WasmModule) -> Result<(), String> {
        let count = self.read_uleb128()?;
        println!("  Code section: {} function bodies", count);
        
        for i in 0..count {
            let body_size = self.read_uleb128()? as usize;
            let body_start = self.pos;
            
            // 读取局部变量
            let local_count = self.read_uleb128()?;
            let mut locals = Vec::new();
            
            for _ in 0..local_count {
                let count = self.read_uleb128()?;
                let value_type = self.read_value_type()?;
                for _ in 0..count {
                    locals.push(value_type);
                }
            }
            
            // 读取函数体字节码
            let code_start = self.pos;
            let code_size = body_size - (code_start - body_start);
            let body = self.read_bytes(code_size)?;
            
            let type_index = if (i as usize) < module.functions.len() {
                module.functions[i as usize]
            } else {
                0
            };
            
            let locals_len = locals.len();
            module.code.push(Function {
                type_index,
                locals,
                body: body.to_vec(),
            });
            
            println!("    Function {}: {} locals, {} bytes code", i, locals_len, code_size);
        }
        
        Ok(())
    }
    
    /// 读取值类型
    fn read_value_type(&mut self) -> Result<ValueType, String> {
        match self.read_u8()? {
            0x7F => Ok(ValueType::I32),
            0x7E => Ok(ValueType::I64),
            0x7D => Ok(ValueType::F32),
            0x7C => Ok(ValueType::F64),
            t => Err(alloc::format!("Unknown value type: 0x{:02x}", t)),
        }
    }
    
    /// 读取字符串
    fn read_string(&mut self) -> Result<String, String> {
        let len = self.read_uleb128()? as usize;
        let bytes = self.read_bytes(len)?;
        
        match core::str::from_utf8(bytes) {
            Ok(s) => Ok(s.to_string()),
            Err(_) => Err("Invalid UTF-8 string".to_string()),
        }
    }
    
    /// 读取LEB128无符号整数
    fn read_uleb128(&mut self) -> Result<u32, String> {
        let mut result = 0u32;
        let mut shift = 0;
        
        loop {
            if shift >= 32 {
                return Err("LEB128 integer too large".to_string());
            }
            
            let byte = self.read_u8()?;
            result |= ((byte & 0x7F) as u32) << shift;
            
            if byte & 0x80 == 0 {
                break;
            }
            
            shift += 7;
        }
        
        Ok(result)
    }
    
    /// 读取字节数组
    fn read_bytes(&mut self, len: usize) -> Result<&[u8], String> {
        if self.pos + len > self.data.len() {
            return Err("Unexpected end of data".to_string());
        }
        
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(bytes)
    }
    
    /// 读取单个字节
    fn read_u8(&mut self) -> Result<u8, String> {
        if self.pos >= self.data.len() {
            return Err("Unexpected end of data".to_string());
        }
        
        let byte = self.data[self.pos];
        self.pos += 1;
        Ok(byte)
    }
    
    /// 读取32位整数(小端序)
    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    
    /// 检查是否到达末尾
    fn is_at_end(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// WASM引擎
pub struct WasmEngine {
    module: Option<WasmModule>,
    memory: Vec<u8>,
    stack: Vec<WasmValue>,
}

/// WASM值
#[derive(Debug, Clone)]
pub enum WasmValue {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
}

/// WASM指令
#[derive(Debug, Clone)]
pub enum WasmInstruction {
    // 控制流指令
    Unreachable,
    Nop,
    Block,
    Loop,
    If,
    Else,
    End,
    Br(u32),
    BrIf(u32),
    BrTable,
    Return,
    Call(u32),
    CallIndirect,
    
    // 局部变量指令
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    
    // 全局变量指令
    GlobalGet(u32),
    GlobalSet(u32),
    
    // 内存指令
    I32Load,
    I64Load,
    I32Store,
    I64Store,
    
    // 常量指令
    I32Const(i32),
    I64Const(i64),
    F32Const(f32),
    F64Const(f64),
    
    // 算术指令
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32DivU,
    I32RemS,
    I32RemU,
    I32And,
    I32Or,
    I32Xor,
    I32Shl,
    I32ShrS,
    I32ShrU,
    I32Rotl,
    I32Rotr,
    
    // 比较指令
    I32Eqz,
    I32Eq,
    I32Ne,
    I32LtS,
    I32LtU,
    I32GtS,
    I32GtU,
    I32LeS,
    I32LeU,
    I32GeS,
    I32GeU,
    
    // 其他指令
    Drop,
    Select,
}

impl WasmEngine {
    /// 创建新的WASM引擎
    pub fn new() -> Self {
        Self {
            module: None,
            memory: Vec::new(),
            stack: Vec::new(),
        }
    }
    
    /// 加载WASM模块
    pub fn load_module(&mut self, wasm_data: &[u8]) -> Result<(), String> {
        let mut parser = WasmParser::new(wasm_data);
        let module = parser.parse_module()?;
        
        // 初始化内存
        if let Some(min_pages) = module.memory_min {
            let memory_size = min_pages as usize * 65536; // 64KB per page
            self.memory = vec![0; memory_size];
            println!("Initialized WASM memory: {} pages ({} bytes)", min_pages, memory_size);
        }
        
        self.module = Some(module);
        Ok(())
    }
    
    /// 查找导出函数
    pub fn find_export_function(&self, name: &str) -> Result<u32, String> {
        let module = self.module.as_ref().ok_or("No module loaded")?;
        
        for export in &module.exports {
            if export.name == name {
                match export.kind {
                    ExportKind::Function => return Ok(export.index),
                    _ => return Err(alloc::format!("Export '{}' is not a function", name)),
                }
            }
        }
        
        Err(alloc::format!("Export function '{}' not found", name))
    }
    
    /// 执行函数
    pub fn call_function(&mut self, func_index: u32) -> Result<Vec<WasmValue>, String> {
        // 首先获取需要的数据，然后释放对module的引用
        let (body, locals) = {
            let module = self.module.as_ref().ok_or("No module loaded")?;
            
            println!("Calling function {}", func_index);
            
            // 查找函数
            let adjusted_index = func_index - module.imports.len() as u32;
            if adjusted_index >= module.code.len() as u32 {
                return Err(alloc::format!("Function {} not found", func_index));
            }
            
            let function = &module.code[adjusted_index as usize];
            println!("Executing function with {} locals, {} bytes code", 
                    function.locals.len(), function.body.len());
            
            // 克隆需要的数据
            (function.body.clone(), function.locals.clone())
        };
        
        // 现在可以安全地调用mutable方法
        self.execute_function_body(&body, &locals)
    }
    
    /// 执行函数体字节码
    fn execute_function_body(&mut self, body: &[u8], locals: &[ValueType]) -> Result<Vec<WasmValue>, String> {
        // 初始化局部变量栈
        let mut local_vars = Vec::new();
        for &local_type in locals {
            local_vars.push(match local_type {
                ValueType::I32 => WasmValue::I32(0),
                ValueType::I64 => WasmValue::I64(0),
                ValueType::F32 => WasmValue::F32(0.0),
                ValueType::F64 => WasmValue::F64(0.0),
            });
        }
        
        // 清空栈
        self.stack.clear();
        
        // 解析并执行指令
        let mut pc = 0; // 程序计数器
        
        while pc < body.len() {
            let opcode = body[pc];
            pc += 1;
            
            match self.execute_instruction(opcode, body, &mut pc, &mut local_vars)? {
                Some(result) => return Ok(result),
                None => continue,
            }
        }
        
        println!("Function execution completed");
        Ok(vec![])
    }
    
    /// 执行单个指令
    fn execute_instruction(
        &mut self,
        opcode: u8,
        body: &[u8],
        pc: &mut usize,
        local_vars: &mut Vec<WasmValue>,
    ) -> Result<Option<Vec<WasmValue>>, String> {
        match opcode {
            // 控制流指令
            0x00 => return Err("Unreachable instruction executed".to_string()),
            0x01 => {} // nop
            
            // 局部变量指令
            0x20 => { // local.get
                let local_idx = self.read_uleb128(body, pc)? as usize;
                if local_idx >= local_vars.len() {
                    return Err(alloc::format!("Local variable {} out of bounds", local_idx));
                }
                self.stack.push(local_vars[local_idx].clone());
            }
            
            0x21 => { // local.set
                let local_idx = self.read_uleb128(body, pc)? as usize;
                if local_idx >= local_vars.len() {
                    return Err(alloc::format!("Local variable {} out of bounds", local_idx));
                }
                if self.stack.is_empty() {
                    return Err("Stack underflow on local.set".to_string());
                }
                local_vars[local_idx] = self.stack.pop().unwrap();
            }
            
            // 常量指令
            0x41 => { // i32.const
                let value = self.read_sleb128(body, pc)? as i32;
                self.stack.push(WasmValue::I32(value));
            }
            
            0x42 => { // i64.const
                let value = self.read_sleb128(body, pc)?;
                self.stack.push(WasmValue::I64(value));
            }
            
            // 算术指令 - I32
            0x6a => { // i32.add
                let b = self.pop_i32()?;
                let a = self.pop_i32()?;
                self.stack.push(WasmValue::I32(a.wrapping_add(b)));
            }
            
            0x6b => { // i32.sub
                let b = self.pop_i32()?;
                let a = self.pop_i32()?;
                self.stack.push(WasmValue::I32(a.wrapping_sub(b)));
            }
            
            0x6c => { // i32.mul
                let b = self.pop_i32()?;
                let a = self.pop_i32()?;
                self.stack.push(WasmValue::I32(a.wrapping_mul(b)));
            }
            
            // 比较指令
            0x45 => { // i32.eqz
                let a = self.pop_i32()?;
                self.stack.push(WasmValue::I32(if a == 0 { 1 } else { 0 }));
            }
            
            0x46 => { // i32.eq
                let b = self.pop_i32()?;
                let a = self.pop_i32()?;
                self.stack.push(WasmValue::I32(if a == b { 1 } else { 0 }));
            }
            
            // 其他指令
            0x1a => { // drop
                if self.stack.is_empty() {
                    return Err("Stack underflow on drop".to_string());
                }
                self.stack.pop();
            }
            
            // 函数返回
            0x0f => { // return
                let mut result = Vec::new();
                if !self.stack.is_empty() {
                    result.push(self.stack.pop().unwrap());
                }
                return Ok(Some(result));
            }
            
            // 块结束
            0x0b => { // end
                // 函数/块结束
                let mut result = Vec::new();
                if !self.stack.is_empty() {
                    result.push(self.stack.pop().unwrap());
                }
                return Ok(Some(result));
            }
            
            _ => {
                println!("Unimplemented opcode: 0x{:02x}", opcode);
                // 对于未实现的指令，继续执行而不是报错
            }
        }
        
        Ok(None)
    }
    
    /// 从栈中弹出i32值
    fn pop_i32(&mut self) -> Result<i32, String> {
        match self.stack.pop() {
            Some(WasmValue::I32(val)) => Ok(val),
            Some(_) => Err("Type mismatch: expected i32".to_string()),
            None => Err("Stack underflow".to_string()),
        }
    }
    
    /// 从字节码中读取LEB128无符号整数
    fn read_uleb128(&self, data: &[u8], pc: &mut usize) -> Result<u32, String> {
        let mut result = 0u32;
        let mut shift = 0;
        
        loop {
            if *pc >= data.len() {
                return Err("Unexpected end of bytecode".to_string());
            }
            
            let byte = data[*pc];
            *pc += 1;
            
            result |= ((byte & 0x7F) as u32) << shift;
            
            if byte & 0x80 == 0 {
                break;
            }
            
            shift += 7;
            if shift >= 32 {
                return Err("LEB128 integer too large".to_string());
            }
        }
        
        Ok(result)
    }
    
    /// 从字节码中读取LEB128有符号整数
    fn read_sleb128(&self, data: &[u8], pc: &mut usize) -> Result<i64, String> {
        let mut result = 0i64;
        let mut shift = 0;
        let mut byte;
        
        loop {
            if *pc >= data.len() {
                return Err("Unexpected end of bytecode".to_string());
            }
            
            byte = data[*pc];
            *pc += 1;
            
            result |= ((byte & 0x7F) as i64) << shift;
            shift += 7;
            
            if byte & 0x80 == 0 {
                break;
            }
            
            if shift >= 64 {
                return Err("LEB128 integer too large".to_string());
            }
        }
        
        // 符号扩展
        if shift < 64 && (byte & 0x40) != 0 {
            result |= !0i64 << shift;
        }
        
        Ok(result)
    }
    
    /// 获取内存
    pub fn get_memory(&self) -> &[u8] {
        &self.memory
    }
    
    /// 获取可变内存
    pub fn get_memory_mut(&mut self) -> &mut [u8] {
        &mut self.memory
    }
}

impl Default for WasmEngine {
    fn default() -> Self {
        Self::new()
    }
}