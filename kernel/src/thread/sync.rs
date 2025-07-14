use alloc::{sync::Arc, vec::Vec, collections::{VecDeque, BTreeMap}};
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use crate::{
    sync::UPSafeCell,
    thread::{ThreadId, ThreadControlBlock},
    task::current_task,
    timer::get_time_us,
};

/// 同步对象ID类型
pub type SyncObjectId = usize;

/// 同步对象ID分配器
static SYNC_ID_ALLOCATOR: AtomicUsize = AtomicUsize::new(1);

fn alloc_sync_id() -> SyncObjectId {
    SYNC_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed)
}

/// 等待队列项
#[derive(Debug, Clone)]
struct WaitQueueItem {
    thread_id: ThreadId,
    enqueue_time: u64,
    priority: i32,
}

impl WaitQueueItem {
    fn new(thread_id: ThreadId, priority: i32) -> Self {
        Self {
            thread_id,
            enqueue_time: get_time_us(),
            priority,
        }
    }
}

/// 等待队列 - 支持优先级调度
#[derive(Debug)]
struct WaitQueue {
    queue: VecDeque<WaitQueueItem>,
}

impl WaitQueue {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// 加入等待队列（按优先级插入）
    fn enqueue(&mut self, thread_id: ThreadId, priority: i32) {
        let item = WaitQueueItem::new(thread_id, priority);
        
        // 按优先级插入（优先级高的在前面）
        let mut insert_pos = 0;
        for (i, existing) in self.queue.iter().enumerate() {
            if existing.priority < priority {
                insert_pos = i;
                break;
            }
            insert_pos = i + 1;
        }
        
        self.queue.insert(insert_pos, item);
    }

    /// 从等待队列中取出第一个线程
    fn dequeue(&mut self) -> Option<ThreadId> {
        self.queue.pop_front().map(|item| item.thread_id)
    }

    /// 移除特定线程
    fn remove(&mut self, thread_id: ThreadId) -> bool {
        if let Some(pos) = self.queue.iter().position(|item| item.thread_id == thread_id) {
            self.queue.remove(pos);
            true
        } else {
            false
        }
    }

    /// 检查是否为空
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// 获取等待线程数
    fn len(&self) -> usize {
        self.queue.len()
    }
}

/// 互斥锁实现
#[derive(Debug)]
pub struct Mutex<T> {
    id: SyncObjectId,
    /// 锁状态 
    locked: AtomicBool,
    /// 当前持有锁的线程
    owner: UPSafeCell<Option<ThreadId>>,
    /// 等待队列
    wait_queue: UPSafeCell<WaitQueue>,
    /// 保护的数据
    data: UPSafeCell<T>,
    /// 递归锁计数（支持递归锁）
    recursive_count: UPSafeCell<usize>,
}

impl<T> Mutex<T> {
    /// 创建新的互斥锁
    pub fn new(data: T) -> Self {
        Self {
            id: alloc_sync_id(),
            locked: AtomicBool::new(false),
            owner: UPSafeCell::new(None),
            wait_queue: UPSafeCell::new(WaitQueue::new()),
            data: UPSafeCell::new(data),
            recursive_count: UPSafeCell::new(0),
        }
    }

    /// 获取锁ID
    pub fn id(&self) -> SyncObjectId {
        self.id
    }

    /// 加锁（阻塞式）
    pub fn lock(&self) -> MutexGuard<T> {
        let current_thread_id = self.get_current_thread_id();
        
        loop {
            // 尝试获取锁
            if self.try_lock_internal(current_thread_id) {
                break;
            }
            
            // 加入等待队列并阻塞
            {
                let mut wait_queue = self.wait_queue.exclusive_access();
                wait_queue.enqueue(current_thread_id, self.get_thread_priority(current_thread_id));
            }
            
            // 阻塞当前线程
            self.block_current_thread();
        }

        MutexGuard {
            mutex: self,
        }
    }

    /// 尝试加锁（非阻塞）
    pub fn try_lock(&self) -> Option<MutexGuard<T>> {
        let current_thread_id = self.get_current_thread_id();
        
        if self.try_lock_internal(current_thread_id) {
            Some(MutexGuard {
                mutex: self,
            })
        } else {
            None
        }
    }

    /// 内部尝试加锁实现
    fn try_lock_internal(&self, thread_id: ThreadId) -> bool {
        let mut owner = self.owner.exclusive_access();
        
        // 检查递归锁
        if let Some(current_owner) = *owner {
            if current_owner == thread_id {
                // 递归锁
                let mut count = self.recursive_count.exclusive_access();
                *count += 1;
                return true;
            }
        }
        
        // 尝试获取锁
        if !self.locked.load(Ordering::Acquire) {
            if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                *owner = Some(thread_id);
                *self.recursive_count.exclusive_access() = 1;
                return true;
            }
        }
        
        false
    }

    /// 解锁
    pub fn unlock(&self) {
        let current_thread_id = self.get_current_thread_id();
        let mut owner = self.owner.exclusive_access();
        
        // 检查是否是锁的拥有者
        if let Some(lock_owner) = *owner {
            if lock_owner != current_thread_id {
                panic!("Attempting to unlock mutex from non-owner thread");
            }
        } else {
            panic!("Attempting to unlock unlocked mutex");
        }
        
        // 处理递归锁
        let mut count = self.recursive_count.exclusive_access();
        *count -= 1;
        
        if *count > 0 {
            // 仍然持有递归锁
            return;
        }
        
        // 释放锁
        *owner = None;
        drop(count);
        drop(owner);
        
        self.locked.store(false, Ordering::Release);
        
        // 唤醒等待队列中的下一个线程
        self.wakeup_next_waiter();
    }

    /// 使用锁保护的数据执行操作
    pub fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let _guard = self.lock();
        let mut data = self.data.exclusive_access();
        f(&mut *data)
    }

    /// 唤醒下一个等待者
    fn wakeup_next_waiter(&self) {
        let mut wait_queue = self.wait_queue.exclusive_access();
        
        if let Some(thread_id) = wait_queue.dequeue() {
            drop(wait_queue);
            self.wakeup_thread(thread_id);
        }
    }

    /// 阻塞当前线程
    fn block_current_thread(&self) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    current_thread.set_status(crate::thread::ThreadStatus::Blocked);
                }
                // 调度下一个线程
                thread_manager.schedule_next();
            }
        }
    }

    /// 唤醒线程
    fn wakeup_thread(&self, thread_id: ThreadId) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                thread_manager.wakeup_thread(thread_id);
            }
        }
    }

    /// 获取当前线程ID
    fn get_current_thread_id(&self) -> ThreadId {
        if let Some(current_task) = current_task() {
            let task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    return current_thread.get_thread_id();
                }
            }
        }
        ThreadId(0) // 默认线程ID
    }

    /// 获取线程优先级
    fn get_thread_priority(&self, _thread_id: ThreadId) -> i32 {
        // 这里可以从线程控制块中获取实际优先级
        // 暂时返回默认优先级
        0
    }
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

/// 互斥锁守卫
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T> core::ops::Deref for MutexGuard<'_, T> {
    type Target = UPSafeCell<T>;

    fn deref(&self) -> &Self::Target {
        &self.mutex.data
    }
}

/// 条件变量实现
#[derive(Debug)]
pub struct Condvar {
    id: SyncObjectId,
    /// 等待队列
    wait_queue: UPSafeCell<WaitQueue>,
}

impl Condvar {
    /// 创建新的条件变量
    pub fn new() -> Self {
        Self {
            id: alloc_sync_id(),
            wait_queue: UPSafeCell::new(WaitQueue::new()),
        }
    }

    /// 获取条件变量ID
    pub fn id(&self) -> SyncObjectId {
        self.id
    }

    /// 等待条件（必须在持有互斥锁时调用）
    pub fn wait<T>(&self, mutex_guard: MutexGuard<T>) -> MutexGuard<T> {
        let mutex = mutex_guard.mutex;
        let current_thread_id = mutex.get_current_thread_id();
        
        // 加入等待队列
        {
            let mut wait_queue = self.wait_queue.exclusive_access();
            wait_queue.enqueue(current_thread_id, mutex.get_thread_priority(current_thread_id));
        }
        
        // 释放互斥锁
        drop(mutex_guard);
        
        // 阻塞当前线程
        self.block_current_thread();
        
        // 被唤醒后重新获取互斥锁
        mutex.lock()
    }

    /// 等待条件（带超时）
    pub fn wait_timeout<T>(&self, mutex_guard: MutexGuard<T>, timeout_us: u64) -> (MutexGuard<T>, bool) {
        let mutex = mutex_guard.mutex;
        let current_thread_id = mutex.get_current_thread_id();
        let start_time = get_time_us();
        
        // 加入等待队列
        {
            let mut wait_queue = self.wait_queue.exclusive_access();
            wait_queue.enqueue(current_thread_id, mutex.get_thread_priority(current_thread_id));
        }
        
        // 释放互斥锁
        drop(mutex_guard);
        
        // 带超时的阻塞
        let timed_out = self.block_current_thread_with_timeout(timeout_us);
        
        // 如果超时，从等待队列中移除
        if timed_out {
            let mut wait_queue = self.wait_queue.exclusive_access();
            wait_queue.remove(current_thread_id);
        }
        
        // 重新获取互斥锁
        let guard = mutex.lock();
        (guard, timed_out)
    }

    /// 通知一个等待的线程
    pub fn notify_one(&self) {
        let mut wait_queue = self.wait_queue.exclusive_access();
        
        if let Some(thread_id) = wait_queue.dequeue() {
            drop(wait_queue);
            self.wakeup_thread(thread_id);
        }
    }

    /// 通知所有等待的线程
    pub fn notify_all(&self) {
        let mut wait_queue = self.wait_queue.exclusive_access();
        let mut threads_to_wakeup = Vec::new();
        
        while let Some(thread_id) = wait_queue.dequeue() {
            threads_to_wakeup.push(thread_id);
        }
        
        drop(wait_queue);
        
        // 唤醒所有线程
        for thread_id in threads_to_wakeup {
            self.wakeup_thread(thread_id);
        }
    }

    /// 阻塞当前线程
    fn block_current_thread(&self) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    current_thread.set_status(crate::thread::ThreadStatus::Blocked);
                }
                thread_manager.schedule_next();
            }
        }
    }

    /// 带超时的阻塞当前线程
    fn block_current_thread_with_timeout(&self, _timeout_us: u64) -> bool {
        // 这里应该实现带超时的阻塞
        // 暂时简化为普通阻塞
        self.block_current_thread();
        false // 假设没有超时
    }

    /// 唤醒线程
    fn wakeup_thread(&self, thread_id: ThreadId) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                thread_manager.wakeup_thread(thread_id);
            }
        }
    }
}

/// 读写锁实现
#[derive(Debug)]
pub struct RwLock<T> {
    id: SyncObjectId,
    /// 读者计数
    readers: AtomicUsize,
    /// 写者状态
    writer: AtomicBool,
    /// 当前写者线程ID
    writer_thread: UPSafeCell<Option<ThreadId>>,
    /// 读者等待队列
    reader_queue: UPSafeCell<WaitQueue>,
    /// 写者等待队列
    writer_queue: UPSafeCell<WaitQueue>,
    /// 保护的数据
    data: UPSafeCell<T>,
}

impl<T> RwLock<T> {
    /// 创建新的读写锁
    pub fn new(data: T) -> Self {
        Self {
            id: alloc_sync_id(),
            readers: AtomicUsize::new(0),
            writer: AtomicBool::new(false),
            writer_thread: UPSafeCell::new(None),
            reader_queue: UPSafeCell::new(WaitQueue::new()),
            writer_queue: UPSafeCell::new(WaitQueue::new()),
            data: UPSafeCell::new(data),
        }
    }

    /// 获取读写锁ID
    pub fn id(&self) -> SyncObjectId {
        self.id
    }

    /// 获取读锁
    pub fn read(&self) -> RwLockReadGuard<T> {
        let current_thread_id = self.get_current_thread_id();
        
        loop {
            let readers = self.readers.load(Ordering::Acquire);
            let has_writer = self.writer.load(Ordering::Acquire);
            
            // 如果没有写者且写者队列为空，可以获取读锁
            if !has_writer && self.writer_queue.exclusive_access().is_empty() {
                if self.readers.compare_exchange(readers, readers + 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    break;
                }
            } else {
                // 加入读者等待队列
                {
                    let mut reader_queue = self.reader_queue.exclusive_access();
                    reader_queue.enqueue(current_thread_id, self.get_thread_priority(current_thread_id));
                }
                
                // 阻塞当前线程
                self.block_current_thread();
            }
        }

        RwLockReadGuard {
            rwlock: self,
        }
    }

    /// 获取写锁
    pub fn write(&self) -> RwLockWriteGuard<T> {
        let current_thread_id = self.get_current_thread_id();
        
        loop {
            let readers = self.readers.load(Ordering::Acquire);
            let has_writer = self.writer.load(Ordering::Acquire);
            
            // 如果没有读者和写者，可以获取写锁
            if readers == 0 && !has_writer {
                if self.writer.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    *self.writer_thread.exclusive_access() = Some(current_thread_id);
                    break;
                }
            } else {
                // 加入写者等待队列
                {
                    let mut writer_queue = self.writer_queue.exclusive_access();
                    writer_queue.enqueue(current_thread_id, self.get_thread_priority(current_thread_id));
                }
                
                // 阻塞当前线程
                self.block_current_thread();
            }
        }

        RwLockWriteGuard {
            rwlock: self,
        }
    }

    /// 释放读锁
    fn unlock_read(&self) {
        let readers = self.readers.fetch_sub(1, Ordering::Release);
        
        if readers == 1 {
            // 最后一个读者释放锁，唤醒等待的写者
            self.wakeup_next_writer();
        }
    }

    /// 释放写锁
    fn unlock_write(&self) {
        *self.writer_thread.exclusive_access() = None;
        self.writer.store(false, Ordering::Release);
        
        // 优先唤醒所有等待的读者
        self.wakeup_all_readers();
        
        // 如果没有读者被唤醒，唤醒下一个写者
        if self.reader_queue.exclusive_access().is_empty() {
            self.wakeup_next_writer();
        }
    }

    /// 唤醒所有等待的读者
    fn wakeup_all_readers(&self) {
        let mut reader_queue = self.reader_queue.exclusive_access();
        let mut threads_to_wakeup = Vec::new();
        
        while let Some(thread_id) = reader_queue.dequeue() {
            threads_to_wakeup.push(thread_id);
        }
        
        drop(reader_queue);
        
        for thread_id in threads_to_wakeup {
            self.wakeup_thread(thread_id);
        }
    }

    /// 唤醒下一个等待的写者
    fn wakeup_next_writer(&self) {
        let mut writer_queue = self.writer_queue.exclusive_access();
        
        if let Some(thread_id) = writer_queue.dequeue() {
            drop(writer_queue);
            self.wakeup_thread(thread_id);
        }
    }

    /// 使用读锁访问数据
    pub fn with_read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        let _guard = self.read();
        let data = self.data.exclusive_access();
        f(&*data)
    }

    /// 使用写锁访问数据
    pub fn with_write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let _guard = self.write();
        let mut data = self.data.exclusive_access();
        f(&mut *data)
    }

    /// 阻塞当前线程
    fn block_current_thread(&self) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    current_thread.set_status(crate::thread::ThreadStatus::Blocked);
                }
                thread_manager.schedule_next();
            }
        }
    }

    /// 唤醒线程
    fn wakeup_thread(&self, thread_id: ThreadId) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                thread_manager.wakeup_thread(thread_id);
            }
        }
    }

    /// 获取当前线程ID
    fn get_current_thread_id(&self) -> ThreadId {
        if let Some(current_task) = current_task() {
            let task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    return current_thread.get_thread_id();
                }
            }
        }
        ThreadId(0)
    }

    /// 获取线程优先级
    fn get_thread_priority(&self, _thread_id: ThreadId) -> i32 {
        0 // 默认优先级
    }
}

/// 读锁守卫
pub struct RwLockReadGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.rwlock.unlock_read();
    }
}

impl<T> core::ops::Deref for RwLockReadGuard<'_, T> {
    type Target = UPSafeCell<T>;

    fn deref(&self) -> &Self::Target {
        &self.rwlock.data
    }
}

/// 写锁守卫
pub struct RwLockWriteGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.rwlock.unlock_write();
    }
}

impl<T> core::ops::Deref for RwLockWriteGuard<'_, T> {
    type Target = UPSafeCell<T>;

    fn deref(&self) -> &Self::Target {
        &self.rwlock.data
    }
}

/// 信号量实现
#[derive(Debug)]
pub struct Semaphore {
    id: SyncObjectId,
    /// 信号量计数
    count: AtomicUsize,
    /// 等待队列
    wait_queue: UPSafeCell<WaitQueue>,
}

impl Semaphore {
    /// 创建新的信号量
    pub fn new(initial_count: usize) -> Self {
        Self {
            id: alloc_sync_id(),
            count: AtomicUsize::new(initial_count),
            wait_queue: UPSafeCell::new(WaitQueue::new()),
        }
    }

    /// 获取信号量ID
    pub fn id(&self) -> SyncObjectId {
        self.id
    }

    /// P操作（等待）
    pub fn wait(&self) {
        loop {
            let current_count = self.count.load(Ordering::Acquire);
            if current_count > 0 {
                if self.count.compare_exchange(current_count, current_count - 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    break;
                }
            } else {
                // 加入等待队列并阻塞
                let current_thread_id = self.get_current_thread_id();
                {
                    let mut wait_queue = self.wait_queue.exclusive_access();
                    wait_queue.enqueue(current_thread_id, self.get_thread_priority(current_thread_id));
                }
                
                self.block_current_thread();
            }
        }
    }

    /// V操作（释放）
    pub fn signal(&self) {
        self.count.fetch_add(1, Ordering::Release);
        
        // 唤醒一个等待的线程
        let mut wait_queue = self.wait_queue.exclusive_access();
        if let Some(thread_id) = wait_queue.dequeue() {
            drop(wait_queue);
            self.wakeup_thread(thread_id);
        }
    }

    /// 获取当前计数
    pub fn get_count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// 阻塞当前线程
    fn block_current_thread(&self) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    current_thread.set_status(crate::thread::ThreadStatus::Blocked);
                }
                thread_manager.schedule_next();
            }
        }
    }

    /// 唤醒线程
    fn wakeup_thread(&self, thread_id: ThreadId) {
        if let Some(current_task) = current_task() {
            let mut task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
                thread_manager.wakeup_thread(thread_id);
            }
        }
    }

    /// 获取当前线程ID
    fn get_current_thread_id(&self) -> ThreadId {
        if let Some(current_task) = current_task() {
            let task_inner = current_task.inner_exclusive_access();
            if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
                if let Some(current_thread) = thread_manager.get_current_thread() {
                    return current_thread.get_thread_id();
                }
            }
        }
        ThreadId(0)
    }

    /// 获取线程优先级
    fn get_thread_priority(&self, _thread_id: ThreadId) -> i32 {
        0
    }
}

/// 全局同步对象管理器
#[derive(Debug)]
pub struct SyncObjectManager {
    mutexes: BTreeMap<SyncObjectId, Arc<Mutex<()>>>,
    condvars: BTreeMap<SyncObjectId, Arc<Condvar>>,
    rwlocks: BTreeMap<SyncObjectId, Arc<RwLock<()>>>,
    semaphores: BTreeMap<SyncObjectId, Arc<Semaphore>>,
}

impl SyncObjectManager {
    pub fn new() -> Self {
        Self {
            mutexes: BTreeMap::new(),
            condvars: BTreeMap::new(),
            rwlocks: BTreeMap::new(),
            semaphores: BTreeMap::new(),
        }
    }

    /// 创建互斥锁
    pub fn create_mutex(&mut self) -> SyncObjectId {
        let mutex = Arc::new(Mutex::new(()));
        let id = mutex.id();
        self.mutexes.insert(id, mutex);
        id
    }

    /// 创建条件变量
    pub fn create_condvar(&mut self) -> SyncObjectId {
        let condvar = Arc::new(Condvar::new());
        let id = condvar.id();
        self.condvars.insert(id, condvar);
        id
    }

    /// 创建读写锁
    pub fn create_rwlock(&mut self) -> SyncObjectId {
        let rwlock = Arc::new(RwLock::new(()));
        let id = rwlock.id();
        self.rwlocks.insert(id, rwlock);
        id
    }

    /// 创建信号量
    pub fn create_semaphore(&mut self, initial_count: usize) -> SyncObjectId {
        let semaphore = Arc::new(Semaphore::new(initial_count));
        let id = semaphore.id();
        self.semaphores.insert(id, semaphore);
        id
    }

    /// 获取互斥锁
    pub fn get_mutex(&self, id: SyncObjectId) -> Option<Arc<Mutex<()>>> {
        self.mutexes.get(&id).cloned()
    }

    /// 获取条件变量
    pub fn get_condvar(&self, id: SyncObjectId) -> Option<Arc<Condvar>> {
        self.condvars.get(&id).cloned()
    }

    /// 获取读写锁
    pub fn get_rwlock(&self, id: SyncObjectId) -> Option<Arc<RwLock<()>>> {
        self.rwlocks.get(&id).cloned()
    }

    /// 获取信号量
    pub fn get_semaphore(&self, id: SyncObjectId) -> Option<Arc<Semaphore>> {
        self.semaphores.get(&id).cloned()
    }

    /// 销毁同步对象
    pub fn destroy_mutex(&mut self, id: SyncObjectId) -> bool {
        self.mutexes.remove(&id).is_some()
    }

    pub fn destroy_condvar(&mut self, id: SyncObjectId) -> bool {
        self.condvars.remove(&id).is_some()
    }

    pub fn destroy_rwlock(&mut self, id: SyncObjectId) -> bool {
        self.rwlocks.remove(&id).is_some()
    }

    pub fn destroy_semaphore(&mut self, id: SyncObjectId) -> bool {
        self.semaphores.remove(&id).is_some()
    }
}