//! VMO 构造：准备唯一页框/树 owner，并在发布 Arc 前配对物理页记账。
use super::*;

impl Vmo {
    pub(super) fn new_in(
        tree: Arc<Tree>,
        kind: Kind,
        pages: usize,
        relation: Option<Relation>,
    ) -> Result<Arc<Self>, VmError> {
        let vmo = Self {
            id: crate::id::next_koid(),
            kind,
            tree,
            state: Mutex::new(State {
                pages,
                committed: PageList::new(),
                relation,
                children: Vec::new(),
                mappings: Vec::new(),
                requested: PageList::new(),
                detached: false,
                zero_mapped: false,
                reclaim_cursor: 0,
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
}
