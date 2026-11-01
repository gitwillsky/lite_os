//! 实际 open admission、cyclic wd 与 watch membership；所有 node allocation 先于短 owner guard。
use super::*;

/// 同一实例的 actual watch 集合；inode/wd 都是同一 membership 的查找投影。
pub(super) struct WatchMembership {
    watches: FallibleMap<i32, Arc<Watch>>,
    identities: FallibleMap<(usize, u64), i32>,
    next_wd: i32,
    admission: UserResourceCharge,
}
impl WatchMembership {
    fn existing(&self, identity: &(usize, u64), flags: u32) -> Result<Option<i32>, WatchError> {
        let Some(wd) = self.identities.get(identity) else {
            return Ok(None);
        };
        if flags & IN_MASK_CREATE != 0 {
            return Err(WatchError::AlreadyExists);
        }
        let watch = self
            .watches
            .get(wd)
            .expect("watch inode index lost actual wd");
        let events = flags & (ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK);
        let mask = if flags & IN_MASK_ADD != 0 {
            watch.mask.load(Ordering::Relaxed) | events
        } else {
            events
        };
        watch.mask.store(mask, Ordering::Relaxed);
        Ok(Some(*wd))
    }
    pub(super) fn contains(&self, watch: &Arc<Watch>) -> bool {
        self.watches
            .get(&watch.wd)
            .is_some_and(|actual| Arc::ptr_eq(actual, watch))
    }
    pub(super) fn take(&mut self, wd: i32, expected: Option<&Arc<Watch>>) -> Option<Arc<Watch>> {
        if expected.is_some_and(|watch| !self.contains(watch)) {
            return None;
        }
        let watch = self.watches.remove(&wd)?;
        assert_eq!(self.identities.remove(&watch.identity), Some(wd));
        Some(watch)
    }
    fn available_wd(&self) -> i32 {
        let mut wd = self.next_wd;
        while self.watches.contains_key(&wd) {
            wd = if wd == i32::MAX { 1 } else { wd + 1 };
        }
        wd
    }
}

impl Inotify {
    pub(crate) const ANONYMOUS_NAME: &'static [u8] = b"anon_inode:inotify";

    /// 先准备全部 storage/overflow record，最后登记名额；失败只退还已经存在的 RAII admission。
    pub(crate) fn new(namespace: Arc<UserNamespace>, uid: u32) -> Result<Arc<Self>, InitError> {
        let mut allocated = Arc::<Self>::try_new_uninit().map_err(|_| InitError::OutOfMemory)?;
        let queue = EventQueue::try_new(QUEUED_EVENT_LIMIT.load(Ordering::Relaxed) as usize)
            .map_err(|_| InitError::OutOfMemory)?;
        let notification = Pipe::notification_pair().map_err(|_| InitError::OutOfMemory)?;
        let admission =
            UserResourceCharge::acquire(&namespace, uid, UserResource::InotifyInstances)
                .map_err(|_| InitError::TooManyInstances)?;
        Arc::get_mut(&mut allocated)
            .expect("unpublished inotify instance is unique")
            .write(Self {
                id: crate::id::next_runtime_object_id(),
                queue: Mutex::new(Some(queue)),
                watches: Mutex::new(Some(WatchMembership {
                    watches: FallibleMap::new(),
                    identities: FallibleMap::new(),
                    next_wd: 1,
                    admission,
                })),
                notification,
                this: Mutex::new(Weak::new()),
            });
        // SAFETY: 未 clone/Weak 的唯一 storage 已完整初始化；admission 之后无 fallible 操作。
        let instance = unsafe { allocated.assume_init() };
        *instance.this.lock() = Arc::downgrade(&instance);
        Ok(instance)
    }

    /// 添加/更新实际 watch；同 inode 重复更新不分配，wd 按 Linux 正 signed-int 范围 cyclic 分配。
    ///
    /// # Errors
    /// InvalidMask、Closed、TooManyWatches 或锁外 storage OutOfMemory；没有部分发布。
    pub(crate) fn add_watch(&self, identity: (usize, u64), flags: u32) -> Result<i32, WatchError> {
        if flags & ALL_INOTIFY_BITS == 0
            || flags & !ALL_INOTIFY_BITS != 0
            || flags & (IN_MASK_CREATE | IN_MASK_ADD) == (IN_MASK_CREATE | IN_MASK_ADD)
        {
            return Err(WatchError::InvalidMask);
        }
        {
            let membership = self.watches.lock();
            let membership = membership.as_ref().ok_or(WatchError::Closed)?;
            if let Some(wd) = membership.existing(&identity, flags)? {
                return Ok(wd);
            }
        }
        // 固定 instance 的原 real UID/UserNS；新 recipient 或当前 credentials 不重新决定收费起点。
        let (namespace, uid) = {
            let guard = self.watches.lock();
            guard.as_ref().ok_or(WatchError::Closed)?.admission.owner()
        };
        let admission = UserResourceCharge::acquire(&namespace, uid, UserResource::InotifyWatches)
            .map_err(|_| WatchError::TooManyWatches)?;
        // 1. 全部 Arc/三个实际索引及 optional outer inode node 的 storage 在 guard 外准备。
        let mut watch_storage =
            Arc::<Watch>::try_new_uninit().map_err(|_| WatchError::OutOfMemory)?;
        let descriptor = FallibleMap::<i32, Arc<Watch>>::try_reserve_node()
            .map_err(|_| WatchError::OutOfMemory)?;
        let inode = FallibleMap::<(usize, u64), i32>::try_reserve_node()
            .map_err(|_| WatchError::OutOfMemory)?;
        let global_watch = FallibleMap::<WatchKey, Arc<Watch>>::try_reserve_node()
            .map_err(|_| WatchError::OutOfMemory)?;
        let mut global_inode = Some(
            FallibleMap::<(usize, u64), InodeWatches>::try_reserve_node()
                .map_err(|_| WatchError::OutOfMemory)?,
        );
        let instance = self.this.lock().clone();

        let mut guard = self.watches.lock();
        let membership = guard.as_mut().ok_or(WatchError::Closed)?;
        // 2. 锁外准备期间可能有 competing add；先重新投影 existing/limit，再消费 prepared storage。
        if let Some(wd) = membership.existing(&identity, flags)? {
            drop(guard);
            return Ok(wd);
        }
        let wd = membership.available_wd();
        let key = (self.id, wd);
        Arc::get_mut(&mut watch_storage)
            .expect("unpublished watch Arc is unique")
            .write(Watch {
                key,
                identity,
                instance,
                wd,
                mask: AtomicU32::new(flags & (ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK)),
                admission: Mutex::new(Some(admission)),
            });
        // SAFETY: 唯一未发表 Arc 已完整写入；之后只执行无分配 AVL commit。
        let watch = unsafe { watch_storage.assume_init() };
        let mut registry = REGISTRY.lock();
        if let Some(list) = registry.by_inode.get_mut(&identity) {
            list.watches
                .commit_vacant(global_watch.fill(key, watch.clone()));
        } else {
            let mut list = InodeWatches {
                watches: FallibleMap::new(),
            };
            list.watches
                .commit_vacant(global_watch.fill(key, watch.clone()));
            registry
                .by_inode
                .commit_vacant(global_inode.take().unwrap().fill(identity, list));
        }
        membership.watches.commit_vacant(descriptor.fill(wd, watch));
        membership
            .identities
            .commit_vacant(inode.fill(identity, wd));
        membership.next_wd = if wd == i32::MAX { 1 } else { wd + 1 };
        WATCH_COUNT.fetch_add(1, Ordering::AcqRel);
        drop(registry);
        drop(guard);
        drop(global_inode);
        Ok(wd)
    }

    /// 移出唯一 actual membership，然后锁外撤销全局 watch 并交付 IN_IGNORED。
    pub(crate) fn remove_watch(&self, wd: i32) -> bool {
        self.remove_watch_exact(wd, None)
    }
    pub(super) fn remove_watch_exact(&self, wd: i32, expected: Option<&Arc<Watch>>) -> bool {
        let removed = {
            let mut guard = self.watches.lock();
            let Some(membership) = guard.as_mut() else {
                return false;
            };
            let Some(watch) = membership.take(wd, expected) else {
                return false;
            };
            watch
        };
        unregister(&removed);
        self.enqueue(wd, IN_IGNORED, 0, b"", None);
        true
    }

    /// actual fput 取走唯一 admission；重复 release/observer Drop 不延长或重复归还资源。
    pub(super) fn release_instance(&self) {
        let membership = self.watches.lock().take();
        let Some(membership) = membership else {
            return;
        };
        let queue = self.queue.lock().take();
        for watch in membership.watches.values() {
            unregister(watch);
        }
        self.notification.0.close();
        self.notification.1.close();
        drop(queue);
        drop(membership);
    }
}
