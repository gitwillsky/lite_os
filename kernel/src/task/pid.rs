pub const INIT_PID: usize = 1;

#[derive(Debug)]
pub struct PidHandle(pub usize);

impl From<usize> for PidHandle {
    fn from(pid: usize) -> Self {
        Self(pid)
    }
}
