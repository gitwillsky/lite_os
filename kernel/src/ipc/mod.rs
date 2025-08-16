pub mod pipe;
pub mod unix_socket;

pub use pipe::{create_pipe, create_fifo, open_fifo, remove_fifo};
pub use unix_socket::{uds_listen, uds_accept, uds_connect};