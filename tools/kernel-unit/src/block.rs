// 块层的纯叶子：trait、分区表、范围 I/O 与身份文本（注册表依赖 spin，不在 host 测试范围）。

#[path = "../../../kernel/src/storage/block/device.rs"]
mod device;
pub(crate) use device::{BLOCK_SIZE, BlockDevice, BlockError};

#[path = "../../../kernel/src/storage/block/identity.rs"]
pub(crate) mod identity;

#[path = "tests/block_identity.rs"]
mod identity_tests;

#[path = "../../../kernel/src/storage/block/partition_table.rs"]
pub(crate) mod partition_table;

#[path = "tests/partition_table.rs"]
mod partition_table_tests;

#[path = "../../../kernel/src/storage/block/range.rs"]
pub(crate) mod range;

#[path = "tests/block_range.rs"]
mod range_tests;
