pub mod address;
pub mod config;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod page_table;

pub fn init() {
    println!("Memory module initialized");
}
