//! VMO 构造：准备唯一页框/树 owner，并在发布 Arc 前配对物理页记账。
use super::*;

impl Vmo {
    pub(super) fn new_in(
        tree: Arc<Tree>,
        kind: Kind,
        pages: usize,
        relation: Option<Relation>,
    ) -> Result<Arc<Self>, VmError> {
        let committed = if matches!(kind, Kind::Paged { pager: Some(_), .. }) {
            PageList::try_reclaimable().map_err(|_| VmError::NoMemory)?
        } else {
            PageList::new()
        };
        let vmo = Self {
            id: crate::id::next_koid(),
            kind,
            tree,
            state: Mutex::new(State {
                pages,
                committed,
                relation,
                children: Vec::new(),
                mappings: Vec::new(),
                requested: PageList::new(),
                detached: false,
                zero_mapped: false,
            }),
            supplied: WaitQueue::new(),
        };
        // Arc 分配失败也会析构 vmo，必须在交给 Arc 前登记，和 Drop 严格配对。
        if let Kind::Contiguous(frames) = &vmo.kind {
            residency::add_contiguous(frames.pages);
        }
        Arc::try_new(vmo).map_err(|_| VmError::NoMemory)
    }

    fn new_tree() -> Result<Arc<Tree>, VmError> {
        Arc::try_new(Tree {
            lock: TaskMutex::new(()),
        })
        .map_err(|_| VmError::NoMemory)
    }

    /// 创建 `size` 字节（向上取整到页）的匿名零内容 VMO。
    ///
    /// # Errors
    /// 大小溢出返回 `InvalidArgs`，分配失败返回 `NoMemory`。
    pub(crate) fn create(size: u64, options: VmoOptions) -> Result<Arc<Self>, VmError> {
        Self::create_content(size, options, PageContent::Anonymous)
    }

    /// 创建启动归档的 backing；只由 bootstrap 在归还固件保留页之前调用。
    ///
    /// # Errors
    /// 同 [`Self::create`]。
    pub(crate) fn create_bootfs(size: u64) -> Result<Arc<Self>, VmError> {
        Self::create_content(size, VmoOptions::default(), PageContent::Bootfs)
    }

    fn create_content(
        size: u64,
        options: VmoOptions,
        content: PageContent,
    ) -> Result<Arc<Self>, VmError> {
        Self::new_in(
            Self::new_tree()?,
            Kind::Paged {
                resizable: options.resizable,
                pager: None,
                content,
            },
            pages_for(size)?,
            None,
        )
    }

    /// 创建 pager 支撑的 VMO。
    ///
    /// # Parameters
    /// - `size`/`options`: 字节大小与可调整大小选项。
    /// - `pager`: 补页请求的唯一出口与 key。
    /// - `anonymous`: 内容没有存储写回目标；修改仍阻止淘汰，但不属于待落盘页。
    ///
    /// # Returns
    /// 已登记到回收 owner 的 VMO；失败不发布对象。
    ///
    /// # Errors
    /// 同 [`Self::create`]。
    pub(crate) fn create_paged(
        size: u64,
        options: VmoOptions,
        pager: PagerBinding,
        anonymous: bool,
    ) -> Result<Arc<Self>, VmError> {
        let content = if anonymous {
            PageContent::AnonymousPager
        } else {
            PageContent::Pager
        };
        let vmo = Self::new_in(
            Self::new_tree()?,
            Kind::Paged {
                resizable: options.resizable,
                pager: Some(pager),
                content,
            },
            pages_for(size)?,
            None,
        )?;
        vmo.register_reclaimable()?;
        Ok(vmo)
    }

    /// 创建物理连续、常驻的 VMO。
    ///
    /// # Errors
    /// 连续页不足返回 `NoMemory`。
    pub(crate) fn create_contiguous(size: u64) -> Result<Arc<Self>, VmError> {
        let pages = pages_for(size)?;
        if pages == 0 {
            return Err(VmError::InvalidArgs);
        }
        let frames = crate::memory::alloc_contiguous(pages, FrameAllocationClass::Reclaimable)
            .ok_or(VmError::NoMemory)?;
        Self::new_in(Self::new_tree()?, Kind::Contiguous(frames), pages, None)
    }

    /// 创建引用物理区间的 VMO；调用者已验证该区间属于授予的 MMIO resource。
    ///
    /// # Errors
    /// 地址未对齐返回 `InvalidArgs`。
    pub(crate) fn create_physical(
        physical: u64,
        size: u64,
        cache: CachePolicy,
    ) -> Result<Arc<Self>, VmError> {
        if !physical.is_multiple_of(PAGE_SIZE as u64) {
            return Err(VmError::InvalidArgs);
        }
        let base = PhysicalPageNumber::from(
            usize::try_from(physical / PAGE_SIZE as u64).map_err(|_| VmError::InvalidArgs)?,
        );
        Self::new_in(
            Self::new_tree()?,
            Kind::Physical { base, cache },
            pages_for(size)?,
            None,
        )
    }

    /// 从物理 Device VMO 投影不可变寄存器字节范围；不解析分页/COW/slice，不触发 pager。
    ///
    /// # Parameters
    /// - `offset`/`length`: VMO 内非空字节范围；调用方仍负责寄存器宽度与对齐检查。
    ///
    /// # Returns
    /// 对应物理地址区间；物理 VMO 不可调整大小，不拥有可回收页框。
    ///
    /// # Errors
    /// 非物理 Device 对象返回 InvalidArgs，越界/溢出返回 OutOfRange。
    pub(crate) fn device_range(
        &self,
        offset: usize,
        length: usize,
    ) -> Result<core::ops::Range<usize>, VmError> {
        let Kind::Physical {
            base,
            cache: CachePolicy::Device,
        } = self.kind
        else {
            return Err(VmError::InvalidArgs);
        };
        let end = offset.checked_add(length).ok_or(VmError::OutOfRange)?;
        if length == 0 || end as u64 > self.size() {
            return Err(VmError::OutOfRange);
        }
        let physical = base
            .as_usize()
            .checked_mul(PAGE_SIZE)
            .ok_or(VmError::OutOfRange)?;
        let start = physical.checked_add(offset).ok_or(VmError::OutOfRange)?;
        Ok(start..physical.checked_add(end).ok_or(VmError::OutOfRange)?)
    }
    /// 创建子对象。
    ///
    /// # Parameters
    /// - `offset`/`size`: 页对齐的字节区间（slice 必须落在父对象内）。
    /// - `kind`: 子对象语义。
    ///
    /// # Errors
    /// 参数未对齐或越界返回 `InvalidArgs`/`OutOfRange`；连续/物理对象不支持 COW 子对象。
    pub(crate) fn create_child(
        self: &Arc<Self>,
        offset: u64,
        size: u64,
        kind: ChildKind,
    ) -> Result<Arc<Self>, VmError> {
        if !offset.is_multiple_of(PAGE_SIZE as u64) {
            return Err(VmError::InvalidArgs);
        }
        let offset_pages =
            usize::try_from(offset / PAGE_SIZE as u64).map_err(|_| VmError::InvalidArgs)?;
        let pages = pages_for(size)?;
        let _tree = self.lock_tree()?;
        let parent_pages = self.state.lock().pages;
        match kind {
            ChildKind::Slice => {
                if offset_pages
                    .checked_add(pages)
                    .is_none_or(|end| end > parent_pages)
                {
                    return Err(VmError::OutOfRange);
                }
                let child = Self::new_in(
                    self.tree.clone(),
                    Kind::Slice,
                    pages,
                    Some(Relation::Slice {
                        parent: self.clone(),
                        offset: offset_pages,
                    }),
                )?;
                let mut state = self.state.lock();
                state.children.retain(|child| child.strong_count() != 0);
                state
                    .children
                    .try_reserve(1)
                    .map_err(|_| VmError::NoMemory)?;
                state.children.push(Arc::downgrade(&child));
                Ok(child)
            }
            ChildKind::AtLeastOnWrite => {
                self.require_cow_capable()?;
                let child = Self::new_in(
                    self.tree.clone(),
                    Kind::Paged {
                        resizable: false,
                        pager: None,
                        content: PageContent::Anonymous,
                    },
                    pages,
                    Some(Relation::Cow {
                        parent: self.clone(),
                        offset: offset_pages,
                        limit: pages.min(parent_pages.saturating_sub(offset_pages)),
                    }),
                )?;
                let mut state = self.state.lock();
                if state.children.try_reserve(1).is_err() {
                    return Err(VmError::NoMemory);
                }
                state.children.retain(|child| child.strong_count() != 0);
                state.children.push(Arc::downgrade(&child));
                Ok(child)
            }
            ChildKind::Snapshot => {
                self.require_cow_capable()?;
                if matches!(self.kind, Kind::Paged { pager: Some(_), .. }) {
                    // 被 pager 支撑的页可能被 pager 改写，无法冻结为不可变快照。
                    return Err(VmError::NotSupported);
                }
                self.snapshot(offset_pages, pages)
            }
        }
    }

    fn require_cow_capable(&self) -> Result<(), VmError> {
        match self.kind {
            Kind::Paged { .. } => Ok(()),
            Kind::Contiguous(_) | Kind::Physical { .. } | Kind::Slice | Kind::Hidden => {
                Err(VmError::NotSupported)
            }
        }
    }

    /// 调用者持有树锁。
    fn snapshot(self: &Arc<Self>, offset: usize, pages: usize) -> Result<Arc<Self>, VmError> {
        // 退出通知可先于兄弟映射退休：下一次快照时，已死亡兄弟可能落在祖先链中间。
        // 仅在快照慢路径遍历祖先，普通页访问仍只折叠直接父节点。
        let mut ancestor = match &self.state.lock().relation {
            Some(Relation::Cow { parent, .. }) => Some(parent.clone()),
            _ => None,
        };
        while let Some(node) = ancestor {
            node.collapse_hidden_parents();
            ancestor = match &node.state.lock().relation {
                Some(Relation::Cow { parent, .. }) => Some(parent.clone()),
                _ => None,
            };
        }
        let hidden = Self::new_in(self.tree.clone(), Kind::Hidden, 0, None)?;
        let child = Self::new_in(
            self.tree.clone(),
            Kind::Paged {
                resizable: false,
                pager: None,
                content: PageContent::Anonymous,
            },
            pages,
            Some(Relation::Cow {
                parent: hidden.clone(),
                offset,
                limit: pages.min(self.state.lock().pages.saturating_sub(offset)),
            }),
        )?;
        {
            let mut state = self.state.lock();
            if state.committed.values().any(|page| page.pins() != 0) {
                // 固定页正被 DMA 使用，移交给隐藏节点后写保护会破坏设备写入的可见性。
                return Err(VmError::BadState);
            }
            let mut hidden_state = hidden.state.lock();
            hidden_state
                .children
                .try_reserve_exact(2)
                .map_err(|_| VmError::NoMemory)?;
            hidden_state.children.push(Arc::downgrade(self));
            hidden_state.children.push(Arc::downgrade(&child));
            if let Some(Relation::Cow { parent, .. }) = &state.relation {
                let mut parent_state = parent.state.lock();
                let entry = parent_state
                    .children
                    .iter_mut()
                    .find(|entry| core::ptr::eq(entry.as_ptr(), Arc::as_ptr(self)))
                    .expect("snapshot source absent from COW tree");
                *entry = Arc::downgrade(&hidden);
            }
            hidden_state.pages = state.pages;
            hidden_state.committed = core::mem::take(&mut state.committed);
            hidden_state.relation = state.relation.take();
            state.relation = Some(Relation::Cow {
                parent: hidden.clone(),
                offset: 0,
                limit: state.pages,
            });
        }
        // 已提交页现属于隐藏节点：父对象的可写翻译必须撤销，否则写入会穿透到共享页。
        let pages_total = self.state.lock().pages;
        self.zap(0..pages_total, ZapMode::RemoveWrite)?;
        Ok(child)
    }
}
