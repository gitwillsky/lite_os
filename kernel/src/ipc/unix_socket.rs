use alloc::{
    collections::{BTreeMap, VecDeque},
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

use crate::fs::{
    FileSystemError,
    inode::{Inode, InodeType},
};
use crate::task::{TaskControlBlock, block_current_and_run_next, current_task};

const POLLIN: u32 = 0x0001;
const POLLOUT: u32 = 0x0004;
const POLLHUP: u32 = 0x0010;

// ================= Stream Socket =================

pub struct UnixStreamSocket {
    inner: Mutex<UnixStreamInner>,
}

struct UnixStreamInner {
    recv: VecDeque<u8>,
    peer: Option<Weak<UnixStreamSocket>>,
    closed: bool,
    poll_waiters: BTreeMap<usize, (Weak<TaskControlBlock>, u32)>,
}

impl UnixStreamSocket {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(UnixStreamInner {
                recv: VecDeque::with_capacity(4096),
                peer: None,
                closed: false,
                poll_waiters: BTreeMap::new(),
            }),
        })
    }
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        let a = Self::new();
        let b = Self::new();
        {
            let mut ia = a.inner.lock();
            ia.peer = Some(Arc::downgrade(&b));
        }
        {
            let mut ib = b.inner.lock();
            ib.peer = Some(Arc::downgrade(&a));
        }
        (a, b)
    }

    fn wake_pollers(&self, mask: u32) {
        let mut to_wakeup: Vec<Weak<TaskControlBlock>> = Vec::new();
        {
            let mut inner = self.inner.lock();
            let mut dead = Vec::new();
            for (pid, (w, interests)) in inner.poll_waiters.iter() {
                if (mask & *interests) != 0 {
                    to_wakeup.push(w.clone());
                }
                if w.upgrade().is_none() {
                    dead.push(*pid);
                }
            }
            for pid in dead {
                inner.poll_waiters.remove(&pid);
            }
        }
        for w in to_wakeup {
            if let Some(t) = w.upgrade() {
                t.wakeup();
            }
        }
    }
}

impl Inode for UnixStreamSocket {
    fn inode_type(&self) -> InodeType {
        InodeType::Device
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut got = 0usize;
            {
                let mut inner = self.inner.lock();
                while got < buf.len() {
                    if let Some(b) = inner.recv.pop_front() {
                        buf[got] = b;
                        got += 1;
                    } else {
                        break;
                    }
                }
                if got > 0 {
                    // 可写
                    drop(inner);
                    self.wake_pollers(POLLOUT);
                    return Ok(got);
                }
                if inner.closed {
                    return Ok(0);
                }
                // 无数据，阻塞
                if let Some(task) = current_task() {
                    inner
                        .poll_waiters
                        .insert(task.pid(), (Arc::downgrade(&task), POLLIN));
                } else {
                    return Err(FileSystemError::IoError);
                }
            }
            block_current_and_run_next();
        }
    }
    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let peer = {
            let inner = self.inner.lock();
            inner.peer.as_ref().and_then(|w| w.upgrade())
        };
        if let Some(peer) = peer {
            let mut pinner = peer.inner.lock();
            let mut written = 0usize;
            const CAP: usize = 4096;
            while written < buf.len() && pinner.recv.len() < CAP {
                pinner.recv.push_back(buf[written]);
                written += 1;
            }
            drop(pinner);
            peer.wake_pollers(POLLIN);
            Ok(written)
        } else {
            Err(FileSystemError::PermissionDenied)
        }
    }
    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn poll_mask(&self) -> u32 {
        let inner = self.inner.lock();
        let mut m = 0u32;
        if !inner.recv.is_empty() {
            m |= POLLIN;
        }
        if !inner.closed {
            m |= POLLOUT;
        }
        if inner.closed {
            m |= POLLHUP;
        }
        m
    }
    fn register_poll_waiter(&self, interests: u32, task: Arc<TaskControlBlock>) {
        let mut inner = self.inner.lock();
        inner
            .poll_waiters
            .insert(task.pid(), (Arc::downgrade(&task), interests));
    }
    fn clear_poll_waiter(&self, task_pid: usize) {
        let mut inner = self.inner.lock();
        inner.poll_waiters.remove(&task_pid);
    }
}

// ================= Listener =================

pub struct UnixListener {
    path: String,
    backlog: usize,
    queue: Mutex<Vec<Arc<UnixStreamSocket>>>,
    poll_waiters: Mutex<BTreeMap<usize, (Weak<TaskControlBlock>, u32)>>,
}

impl UnixListener {
    pub fn new(path: &str, backlog: usize) -> Arc<Self> {
        Arc::new(Self {
            path: path.to_string(),
            backlog,
            queue: Mutex::new(Vec::new()),
            poll_waiters: Mutex::new(BTreeMap::new()),
        })
    }
    fn push_conn(&self, s: Arc<UnixStreamSocket>) {
        let mut q = self.queue.lock();
        if q.len() < self.backlog.max(1) {
            q.push(s);
        }
        drop(q);
        let waiters = self.poll_waiters.lock().clone();
        for (_, (w, interests)) in waiters {
            if (interests & POLLIN) != 0 {
                if let Some(t) = w.upgrade() {
                    t.wakeup();
                }
            }
        }
    }
    fn pop_conn(&self) -> Option<Arc<UnixStreamSocket>> {
        self.queue.lock().pop()
    }
}

pub struct UnixListenerInode {
    listener: Arc<UnixListener>,
}

impl Inode for UnixListenerInode {
    fn inode_type(&self) -> InodeType {
        InodeType::Device
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn poll_mask(&self) -> u32 {
        let has = !self.listener.queue.lock().is_empty();
        if has { POLLIN } else { 0 }
    }
    fn register_poll_waiter(&self, interests: u32, task: Arc<TaskControlBlock>) {
        self.listener
            .poll_waiters
            .lock()
            .insert(task.pid(), (Arc::downgrade(&task), interests));
    }
    fn clear_poll_waiter(&self, task_pid: usize) {
        self.listener.poll_waiters.lock().remove(&task_pid);
    }
}

// ================ Registry and API =================

static UDS_REGISTRY: Mutex<BTreeMap<String, Arc<UnixListener>>> = Mutex::new(BTreeMap::new());

pub fn uds_listen(path: &str, backlog: usize) -> Result<Arc<UnixListenerInode>, FileSystemError> {
    let mut reg = UDS_REGISTRY.lock();
    if reg.contains_key(path) {
        return Err(FileSystemError::AlreadyExists);
    }
    let l = UnixListener::new(path, backlog);
    reg.insert(path.to_string(), l.clone());
    Ok(Arc::new(UnixListenerInode { listener: l }))
}

pub fn uds_accept(path: &str) -> Result<Arc<UnixStreamSocket>, FileSystemError> {
    let reg = UDS_REGISTRY.lock();
    let l = reg.get(path).ok_or(FileSystemError::NotFound)?.clone();
    drop(reg);
    loop {
        if let Some(conn) = l.pop_conn() {
            return Ok(conn);
        }
        if let Some(task) = current_task() {
            l.poll_waiters
                .lock()
                .insert(task.pid(), (Arc::downgrade(&task), POLLIN));
            drop(l.poll_waiters.lock());
            block_current_and_run_next();
        } else {
            return Err(FileSystemError::IoError);
        }
    }
}

pub fn uds_connect(path: &str) -> Result<Arc<UnixStreamSocket>, FileSystemError> {
    let reg = UDS_REGISTRY.lock();
    let l = reg.get(path).ok_or(FileSystemError::NotFound)?.clone();
    drop(reg);
    let (client, server) = UnixStreamSocket::pair();
    l.push_conn(server);
    Ok(client)
}
