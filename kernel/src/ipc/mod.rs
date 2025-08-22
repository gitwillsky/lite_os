pub mod pipe;
pub mod unix_socket;

pub use pipe::{create_fifo, create_pipe, open_fifo, remove_fifo};
pub use unix_socket::{uds_accept, uds_connect, uds_listen};
