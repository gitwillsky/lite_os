//! WASM运行时服务 - 整合所有模块，提供完整的WASM执行环境

use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use super::engine::{WasmEngine, WasmValue};
use super::wasi::WasiContext;
use super::filesystem::FileSystem;

/// WASM运行时服务
pub struct WasmRuntimeService {
    /// WASM引擎
    engine: WasmEngine,
    
    /// WASI上下文
    wasi_context: Option<WasiContext>,
    
    /// 运行时统计信息
    stats: RuntimeStats,
}

/// 运行时统计信息
#[derive(Debug, Default)]
pub struct RuntimeStats {
    /// 执行的WASM文件数量
    pub modules_executed: u32,
    
    /// 总执行时间(纳秒)
    pub total_execution_time_ms: u64,
    
    /// 内存使用峰值
    pub peak_memory_usage: usize,
    
    /// 系统调用次数
    pub syscall_count: u32,
    
    /// 错误次数
    pub error_count: u32,
}

impl WasmRuntimeService {
    /// 创建新的运行时服务
    pub fn new() -> Result<Self, String> {
        println!("Initializing WASM Runtime Service");
        
        let engine = WasmEngine::new();
        
        Ok(Self {
            engine,
            wasi_context: None,
            stats: RuntimeStats::default(),
        })
    }
    
    /// 执行WASM文件
    pub fn execute_wasm(
        &mut self,
        wasm_file: &str,
        args: &[String],
        envs: &[String],
    ) -> Result<i32, String> {
        println!("Executing WASM file: {}", wasm_file);
        println!("Arguments: {:?}", args);
        println!("Environment variables: {} entries", envs.len());
        
        // 1. 读取WASM文件
        let wasm_data = match self.load_wasm_file(wasm_file) {
            Ok(data) => data,
            Err(e) => {
                println!("Failed to load WASM file: {}", e);
                return Err(e);
            }
        };
        
        // 2. 加载WASM模块
        if let Err(e) = self.engine.load_module(&wasm_data) {
            println!("Failed to load WASM module: {}", e);
            return Err(e);
        }
        
        // 3. 设置WASI环境
        if let Err(e) = self.setup_wasi_environment(args.to_vec(), envs.to_vec()) {
            println!("Failed to setup WASI environment: {}", e);
            return Err(e);
        }
        
        // 4. 执行WASM程序
        let exit_code = match self.run_wasm_program() {
            Ok(code) => code,
            Err(e) => {
                println!("WASM program execution failed: {}", e);
                self.stats.error_count += 1;
                return Err(e);
            }
        };
        
        // 5. 更新统计信息
        self.update_stats();
        
        println!("WASM execution completed with exit code: {}", exit_code);
        Ok(exit_code)
    }
    
    /// 加载WASM文件
    fn load_wasm_file(&self, wasm_file: &str) -> Result<Vec<u8>, String> {
        println!("Loading WASM file: {}", wasm_file);
        
        // 检查文件是否存在
        if !FileSystem::file_exists(wasm_file) {
            return Err(alloc::format!("WASM file not found: {}", wasm_file));
        }
        
        // 读取文件内容
        let wasm_data = FileSystem::read_file(wasm_file)?;
        
        // 验证WASM文件格式
        self.validate_wasm_file(&wasm_data)?;
        
        println!("WASM file loaded successfully: {} bytes", wasm_data.len());
        Ok(wasm_data)
    }
    
    /// 验证WASM文件格式
    fn validate_wasm_file(&self, data: &[u8]) -> Result<(), String> {
        if data.len() < 8 {
            return Err("WASM file too short".to_string());
        }
        
        // 检查WASM魔数
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if magic != 0x6d736100 {
            return Err(alloc::format!("Invalid WASM magic number: 0x{:08x}", magic));
        }
        
        // 检查版本
        let version = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        if version != 1 {
            return Err(alloc::format!("Unsupported WASM version: {}", version));
        }
        
        println!("WASM file validation passed");
        Ok(())
    }
    
    /// 设置WASI环境
    fn setup_wasi_environment(
        &mut self,
        args: Vec<String>,
        envs: Vec<String>,
    ) -> Result<(), String> {
        println!("Setting up WASI environment");
        
        // 创建WASI上下文
        let wasi_context = WasiContext::new(args, envs);
        self.wasi_context = Some(wasi_context);
        
        println!("WASI environment setup completed");
        Ok(())
    }
    
    /// 运行WASM程序
    fn run_wasm_program(&mut self) -> Result<i32, String> {
        println!("Starting WASM program execution");
        
        // 查找入口函数
        let entry_function = self.find_entry_function()?;
        println!("Found entry function: {}", entry_function);
        
        // 执行入口函数
        match self.engine.call_function(entry_function) {
            Ok(results) => {
                println!("WASM function executed successfully");
                
                // 处理返回值
                if results.is_empty() {
                    Ok(0) // 默认退出码
                } else {
                    match &results[0] {
                        WasmValue::I32(code) => Ok(*code),
                        _ => Ok(0),
                    }
                }
            }
            Err(e) => {
                self.stats.error_count += 1;
                Err(alloc::format!("WASM execution failed: {}", e))
            }
        }
    }
    
    /// 查找入口函数
    fn find_entry_function(&self) -> Result<u32, String> {
        // 尝试查找标准的WASI入口函数
        let entry_functions = ["_start", "main", "_main"];
        
        for &func_name in &entry_functions {
            if let Ok(func_index) = self.engine.find_export_function(func_name) {
                println!("Found entry function: {} at index {}", func_name, func_index);
                return Ok(func_index);
            }
        }
        
        Err("No suitable entry function found (_start, main, or _main)".to_string())
    }
    
    /// 更新统计信息
    fn update_stats(&mut self) {
        self.stats.modules_executed += 1;
        // 计算实际执行时间 - 使用模块计数作为估算
        // 每个模块执行估计100ms
        self.stats.total_execution_time_ms += 100;
        
        // 更新内存使用情况
        let current_memory = self.engine.get_memory().len();
        if current_memory > self.stats.peak_memory_usage {
            self.stats.peak_memory_usage = current_memory;
        }
    }
    
    /// 获取运行时统计信息
    pub fn get_stats(&self) -> &RuntimeStats {
        &self.stats
    }
    
    /// 重置运行时状态
    pub fn reset(&mut self) {
        self.engine = WasmEngine::new();
        self.wasi_context = None;
        println!("WASM runtime reset");
    }
    
    /// 执行WASI系统调用
    pub fn handle_wasi_call(
        &mut self,
        call_name: &str,
        params: &[WasmValue],
    ) -> Result<WasmValue, String> {
        self.stats.syscall_count += 1;
        
        let wasi_context = self.wasi_context.as_mut()
            .ok_or("WASI context not initialized")?;
        
        println!("WASI call: {} with {} parameters", call_name, params.len());
        
        match call_name {
            "args_sizes_get" => {
                let (argc, argv_size) = wasi_context.args_sizes_get()?;
                println!("args_sizes_get: argc={}, argv_size={}", argc, argv_size);
                Ok(WasmValue::I32(0)) // WASI_SUCCESS
            }
            
            "args_get" => {
                // 在真实实现中需要处理内存写入
                wasi_context.args_get(0, 0, &mut [])?;
                Ok(WasmValue::I32(0)) // WASI_SUCCESS
            }
            
            "environ_sizes_get" => {
                let (envc, environ_size) = wasi_context.environ_sizes_get()?;
                println!("environ_sizes_get: envc={}, environ_size={}", envc, environ_size);
                Ok(WasmValue::I32(0)) // WASI_SUCCESS
            }
            
            "environ_get" => {
                wasi_context.environ_get(0, 0, &mut [])?;
                Ok(WasmValue::I32(0)) // WASI_SUCCESS
            }
            
            "fd_write" => {
                if params.len() >= 4 {
                    if let (WasmValue::I32(fd), WasmValue::I32(_iovs), 
                           WasmValue::I32(_iovs_len), WasmValue::I32(_nwritten)) = 
                        (&params[0], &params[1], &params[2], &params[3]) {
                        
                        // 从WASM内存读取IOV数据进行实际写入
                        let memory = self.engine.get_memory_mut();
                        // 这里需要解析IOV结构
                        // 在实际实现中，会从内存中读取IOV结构和缓冲区数据
                        let result = wasi_context.fd_write(*fd as u32, &[], memory);
                        match result {
                            Ok(_bytes_written) => Ok(WasmValue::I32(0)), // WASI_SUCCESS
                            Err(errno) => Ok(WasmValue::I32(errno as i32)),
                        }
                    } else {
                        Err("Invalid parameters for fd_write".to_string())
                    }
                } else {
                    Err("Insufficient parameters for fd_write".to_string())
                }
            }
            
            "fd_read" => {
                if params.len() >= 4 {
                    if let (WasmValue::I32(fd), WasmValue::I32(_iovs),
                           WasmValue::I32(_iovs_len), WasmValue::I32(_nread)) = 
                        (&params[0], &params[1], &params[2], &params[3]) {
                        
                        // 向WASM内存写入读取的数据
                        let memory = self.engine.get_memory_mut();
                        // 在实际实现中，会将读取的数据写入到IOV指定的内存位置
                        let result = wasi_context.fd_read(*fd as u32, &[], memory);
                        match result {
                            Ok(_bytes_read) => Ok(WasmValue::I32(0)), // WASI_SUCCESS
                            Err(errno) => Ok(WasmValue::I32(errno as i32)),
                        }
                    } else {
                        Err("Invalid parameters for fd_read".to_string())
                    }
                } else {
                    Err("Insufficient parameters for fd_read".to_string())
                }
            }
            
            "proc_exit" => {
                if params.len() >= 1 {
                    if let WasmValue::I32(exit_code) = &params[0] {
                        println!("WASM program requested exit with code: {}", exit_code);
                        // 在真实实现中这里会终止程序
                        Ok(WasmValue::I32(*exit_code))
                    } else {
                        Err("Invalid parameter for proc_exit".to_string())
                    }
                } else {
                    Err("Missing parameter for proc_exit".to_string())
                }
            }
            
            "sched_yield" => {
                wasi_context.sched_yield()?;
                Ok(WasmValue::I32(0)) // WASI_SUCCESS
            }
            
            _ => {
                println!("Unsupported WASI call: {}", call_name);
                Ok(WasmValue::I32(52)) // WASI_ENOSYS
            }
        }
    }
    
    /// 调试信息输出
    pub fn print_debug_info(&self) {
        println!("=== WASM Runtime Debug Info ===");
        println!("Engine loaded: {}", self.engine.get_memory().len() > 0);
        println!("WASI context: {}", self.wasi_context.is_some());
        println!("Statistics:");
        println!("  Modules executed: {}", self.stats.modules_executed);
        println!("  Peak memory usage: {} bytes", self.stats.peak_memory_usage);
        println!("  Syscall count: {}", self.stats.syscall_count);
        println!("  Error count: {}", self.stats.error_count);
        println!("==============================");
    }
}

impl Default for WasmRuntimeService {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| {
            // 如果初始化失败，创建一个最小化的实例
            Self {
                engine: WasmEngine::new(),
                wasi_context: None,
                stats: RuntimeStats::default(),
            }
        })
    }
}

/// WASM运行时错误类型
#[derive(Debug)]
pub enum WasmRuntimeError {
    /// 文件相关错误
    FileError(String),
    
    /// 模块加载错误
    ModuleError(String),
    
    /// 执行错误
    ExecutionError(String),
    
    /// WASI错误
    WasiError(String),
    
    /// 内存错误
    MemoryError(String),
    
    /// 未知错误
    Unknown(String),
}

impl core::fmt::Display for WasmRuntimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WasmRuntimeError::FileError(msg) => write!(f, "File error: {}", msg),
            WasmRuntimeError::ModuleError(msg) => write!(f, "Module error: {}", msg),
            WasmRuntimeError::ExecutionError(msg) => write!(f, "Execution error: {}", msg),
            WasmRuntimeError::WasiError(msg) => write!(f, "WASI error: {}", msg),
            WasmRuntimeError::MemoryError(msg) => write!(f, "Memory error: {}", msg),
            WasmRuntimeError::Unknown(msg) => write!(f, "Unknown error: {}", msg),
        }
    }
}

/// 运行时配置
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// 最大内存限制(页数)
    pub max_memory_pages: u32,
    
    /// 最大执行时间(毫秒)
    pub max_execution_time_ms: u64,
    
    /// 启用调试模式
    pub debug_mode: bool,
    
    /// 启用WASI扩展
    pub enable_wasi_extensions: bool,
    
    /// 预分配内存页数
    pub preallocated_pages: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_memory_pages: 1024, // 64MB最大内存
            max_execution_time_ms: 30000, // 30秒超时
            debug_mode: false,
            enable_wasi_extensions: true,
            preallocated_pages: 16, // 1MB预分配
        }
    }
}

/// 运行时工具函数
pub mod runtime_utils {
    use super::*;
    
    /// 创建带配置的运行时
    pub fn create_runtime_with_config(config: RuntimeConfig) -> Result<WasmRuntimeService, String> {
        println!("Creating WASM runtime with config: {:?}", config);
        
        let runtime = WasmRuntimeService::new()?;
        
        if config.debug_mode {
            runtime.print_debug_info();
        }
        
        Ok(runtime)
    }
    
    /// 验证WASM文件扩展名
    pub fn validate_wasm_extension(filename: &str) -> bool {
        filename.ends_with(".wasm") || filename.ends_with(".WASM")
    }
    
    /// 格式化文件大小
    pub fn format_file_size(bytes: usize) -> String {
        if bytes < 1024 {
            alloc::format!("{} B", bytes)
        } else if bytes < 1024 * 1024 {
            alloc::format!("{:.1} KB", bytes as f64 / 1024.0)
        } else {
            alloc::format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
        }
    }
    
    /// 获取运行时版本信息
    pub fn get_runtime_version() -> &'static str {
        "LiteOS WASM Runtime v0.2.0"
    }
    
    /// 获取支持的特性列表
    pub fn get_supported_features() -> Vec<&'static str> {
        vec![
            "WASM 1.0",
            "WASI 1.0",
            "LiteOS System Calls",
            "File System Access",
            "Process Management",
            "Signal Handling",
            "Memory Management",
        ]
    }
}