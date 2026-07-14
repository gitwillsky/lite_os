use alloc::boxed::Box;
use core::{cmp::Ordering, fmt, mem::MaybeUninit, ops::Index};

mod iter;
use iter::Iter;

type Link<K, V> = Option<Box<Node<K, V>>>;
type RemoveResult<K, V> = (Link<K, V>, Option<Box<Node<K, V>>>);

/// 一次有序表节点分配失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutOfMemory;

struct Node<K, V> {
    key: K,
    value: V,
    height: u8,
    left: Link<K, V>,
    right: Link<K, V>,
}

impl<K, V> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        Self {
            key,
            value,
            height: 1,
            left: None,
            right: None,
        }
    }
}

/// 节点分配可失败、其余结构变换不分配的确定性 AVL 有序表。
pub(crate) struct FallibleMap<K, V> {
    root: Link<K, V>,
    len: usize,
}

/// 已完成唯一节点分配、尚未发布到有序表的 entry token。
pub(crate) struct VacantEntry<K, V>(Box<Node<K, V>>);

/// 已分配但尚未初始化领域 key/value 的唯一节点 storage。
pub(crate) struct NodeSlot<K, V>(Box<MaybeUninit<Node<K, V>>>);

impl<K, V> NodeSlot<K, V> {
    /// 用完整领域值初始化预留 storage。
    ///
    /// @param key 待发布 key。
    /// @param value 待发布 value。
    /// @return 可无分配提交的 entry token。
    pub(crate) fn fill(mut self, key: K, value: V) -> VacantEntry<K, V> {
        self.0.write(Node::new(key, value));
        // SAFETY: storage 刚由 `write` 完整初始化为一个 Node，且 self 按值消费，
        // 此后不再以 MaybeUninit 读取或析构同一 storage。
        VacantEntry(unsafe { self.0.assume_init() })
    }
}

impl<K, V> VacantEntry<K, V> {
    /// 返回 token 中尚未发布 value 的共享引用。
    pub(crate) fn value(&self) -> &V {
        &self.0.value
    }

    /// 返回 token 中尚未发布 value 的独占引用。
    pub(crate) fn value_mut(&mut self) -> &mut V {
        &mut self.0.value
    }

    /// 修改尚未发布的 key，不执行分配。
    ///
    /// @param key 新 key；调用者必须在提交前维持唯一性。
    pub(crate) fn set_key(&mut self, key: K) {
        self.0.key = key;
    }

    /// 消费 token 并返回领域 value，同时释放节点 storage。
    pub(crate) fn into_value(self) -> V {
        let Node { value, .. } = *self.0;
        value
    }
}

impl<K, V> FallibleMap<K, V> {
    /// 构造空表，不分配内存。
    pub(crate) const fn new() -> Self {
        Self { root: None, len: 0 }
    }

    /// 返回当前 entry 数量。
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// 判断表是否为空。
    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 删除全部 entry；释放节点但不执行新分配。
    pub(crate) fn clear(&mut self) {
        self.root = None;
        self.len = 0;
    }

    /// 以 key 升序迭代，不分配临时栈。
    pub(crate) fn iter(&self) -> Iter<'_, K, V> {
        Iter::new(self.root.as_deref())
    }

    /// 以 key 升序迭代 value，不分配临时栈。
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, value)| value)
    }

    /// 从第一个不小于 `start` 的 key 开始升序迭代。
    ///
    /// @param start inclusive lower bound。
    /// @return 不分配临时栈的有序迭代器。
    pub(crate) fn iter_from(&self, start: &K) -> Iter<'_, K, V>
    where
        K: Ord,
    {
        Iter::from_key(self.root.as_deref(), start)
    }

    /// 从第一个严格大于 `start` 的 key 开始升序迭代。
    pub(crate) fn iter_after(&self, start: &K) -> Iter<'_, K, V>
    where
        K: Ord,
    {
        Iter::after_key(self.root.as_deref(), start)
    }

    /// 对全部 value 按 key 顺序执行 mutation，不改变树结构。
    ///
    /// @param visit 每个 entry 的访问逻辑。
    /// @return 无返回值。
    pub(crate) fn for_each_mut(&mut self, mut visit: impl FnMut(&K, &mut V)) {
        fn walk<K, V>(node: &mut Link<K, V>, visit: &mut impl FnMut(&K, &mut V)) {
            let Some(node) = node else {
                return;
            };
            walk(&mut node.left, visit);
            visit(&node.key, &mut node.value);
            walk(&mut node.right, visit);
        }

        walk(&mut self.root, &mut visit);
    }

    /// 对全部 value 按 key 顺序执行可失败 mutation，不改变树结构。
    ///
    /// @param visit 每个 entry 的访问逻辑；首个错误终止遍历。
    /// @return 全部访问成功时为空值，否则返回原始错误。
    pub(crate) fn try_for_each_mut<E>(
        &mut self,
        mut visit: impl FnMut(&K, &mut V) -> Result<(), E>,
    ) -> Result<(), E> {
        fn walk<K, V, E>(
            node: &mut Link<K, V>,
            visit: &mut impl FnMut(&K, &mut V) -> Result<(), E>,
        ) -> Result<(), E> {
            let Some(node) = node else {
                return Ok(());
            };
            walk(&mut node.left, visit)?;
            visit(&node.key, &mut node.value)?;
            walk(&mut node.right, visit)
        }

        walk(&mut self.root, &mut visit)
    }
}

impl<K: Ord, V> FallibleMap<K, V> {
    /// 查询精确 key。
    ///
    /// @param key 查询 key。
    /// @return 已存在 value 的共享引用。
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        let mut cursor = self.root.as_deref();
        while let Some(node) = cursor {
            match key.cmp(&node.key) {
                Ordering::Less => cursor = node.left.as_deref(),
                Ordering::Greater => cursor = node.right.as_deref(),
                Ordering::Equal => return Some(&node.value),
            }
        }
        None
    }

    /// 可变查询精确 key。
    ///
    /// @param key 查询 key。
    /// @return 已存在 value 的独占引用。
    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let mut cursor = self.root.as_deref_mut();
        while let Some(node) = cursor {
            match key.cmp(&node.key) {
                Ordering::Less => cursor = node.left.as_deref_mut(),
                Ordering::Greater => cursor = node.right.as_deref_mut(),
                Ordering::Equal => return Some(&mut node.value),
            }
        }
        None
    }

    /// 判断精确 key 是否存在。
    ///
    /// @param key 查询 key。
    /// @return key 存在时为 true。
    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// 查询不大于 key 的最大 entry。
    pub(crate) fn floor(&self, key: &K) -> Option<(&K, &V)> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            match key.cmp(&node.key) {
                Ordering::Less => cursor = node.left.as_deref(),
                Ordering::Equal => return Some((&node.key, &node.value)),
                Ordering::Greater => {
                    candidate = Some((&node.key, &node.value));
                    cursor = node.right.as_deref();
                }
            }
        }
        candidate
    }

    /// 查询严格小于 key 的最大 entry。
    pub(crate) fn predecessor(&self, key: &K) -> Option<(&K, &V)> {
        let mut cursor = self.root.as_deref();
        let mut candidate = None;
        while let Some(node) = cursor {
            if node.key < *key {
                candidate = Some((&node.key, &node.value));
                cursor = node.right.as_deref();
            } else {
                cursor = node.left.as_deref();
            }
        }
        candidate
    }

    /// 可变查询不大于 key 的最大 entry。
    pub(crate) fn floor_mut(&mut self, key: &K) -> Option<(&K, &mut V)> {
        fn find<'a, K: Ord, V>(node: &'a mut Link<K, V>, key: &K) -> Option<(&'a K, &'a mut V)> {
            let node = node.as_deref_mut()?;
            match key.cmp(&node.key) {
                Ordering::Less => find(&mut node.left, key),
                Ordering::Equal => Some((&node.key, &mut node.value)),
                Ordering::Greater => {
                    find(&mut node.right, key).or(Some((&node.key, &mut node.value)))
                }
            }
        }

        find(&mut self.root, key)
    }

    /// 查询不小于 key 的最小 entry。
    ///
    /// @param key inclusive lower bound。
    /// @return ceiling entry。
    pub(crate) fn ceiling(&self, key: &K) -> Option<(&K, &V)> {
        self.iter_from(key).next()
    }

    /// 查询全表最小 entry。
    pub(crate) fn first_key_value(&self) -> Option<(&K, &V)> {
        let mut cursor = self.root.as_deref()?;
        while let Some(left) = cursor.left.as_deref() {
            cursor = left;
        }
        Some((&cursor.key, &cursor.value))
    }

    /// 原子插入或替换 entry。
    ///
    /// 新 key 的 node 在任何结构 mutation 前通过 `Box::try_new_uninit` 完成；失败时表保持不变。
    ///
    /// @param key entry key。
    /// @param value entry value。
    /// @return 替换时返回旧 value；新 key 返回 None；节点 OOM 返回 `OutOfMemory`。
    pub(crate) fn try_insert(&mut self, key: K, value: V) -> Result<Option<V>, OutOfMemory> {
        if let Some(current) = self.get_mut(&key) {
            return Ok(Some(core::mem::replace(current, value)));
        }
        let entry = self.try_prepare_vacant(key, value)?;
        self.commit_vacant(entry);
        Ok(None)
    }

    /// 在尚未取得目标 owner lock 时预分配一个 entry node。
    ///
    /// @param key 待发布 key。
    /// @param value 与 token 同寿命的待发布 value。
    /// @return 可无分配提交的 token；节点 OOM 时返回错误。
    pub(crate) fn try_prepare(key: K, value: V) -> Result<VacantEntry<K, V>, OutOfMemory> {
        Ok(Self::try_reserve_node()?.fill(key, value))
    }

    /// 仅预留一个节点 allocation，领域值可在后续 transaction 阶段产生。
    ///
    /// @return 成功返回未初始化 storage；OOM 时无任何状态变化。
    pub(crate) fn try_reserve_node() -> Result<NodeSlot<K, V>, OutOfMemory> {
        Box::<Node<K, V>>::try_new_uninit()
            .map(NodeSlot)
            .map_err(|_| OutOfMemory)
    }

    /// 在任何外部状态提交前分配新 key 的唯一节点。
    ///
    /// @param key 必须尚不存在的 entry key。
    /// @param value 与节点一起保存在 token 中的 value。
    /// @return 可无分配提交的 token；节点 OOM 时原表不变。
    pub(crate) fn try_prepare_vacant(
        &self,
        key: K,
        value: V,
    ) -> Result<VacantEntry<K, V>, OutOfMemory> {
        assert!(!self.contains_key(&key), "prepared AVL key already exists");
        Self::try_prepare(key, value)
    }

    /// 无分配发布一个已准备的新 entry。
    ///
    /// @param entry 同一表在未发生结构 mutation 期间创建的 vacant token。
    /// @return 无返回值；重复 key 表示事务不变量损坏并 fail-stop。
    pub(crate) fn commit_vacant(&mut self, entry: VacantEntry<K, V>) {
        assert!(
            !self.contains_key(&entry.0.key),
            "prepared AVL key became occupied before commit"
        );
        self.root = Some(insert_absent(self.root.take(), entry.0));
        self.len += 1;
    }

    /// 删除精确 key，不执行分配。
    ///
    /// @param key 待删除 key。
    /// @return 原 value；key 不存在时为 None。
    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.take_entry(key)?;
        let Node { value, .. } = *entry.0;
        Some(value)
    }

    /// 删除精确 key 并保留其已分配节点作为未发布 token。
    ///
    /// @param key 待删除 key。
    /// @return 可修改 key/value 后重新提交的 token；不存在时为 None。
    pub(crate) fn take_entry(&mut self, key: &K) -> Option<VacantEntry<K, V>> {
        let (root, removed) = remove_node(self.root.take(), key);
        self.root = root;
        let removed = removed?;
        self.len -= 1;
        Some(VacantEntry(removed))
    }

    /// 原地保留满足 predicate 的 entry，不分配遍历快照。
    ///
    /// @param keep 依次观察 key/value，返回 false 的 entry 会被删除。
    /// @return 无返回值；删除过程复用现有树节点与旋转。
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        fn join_with_root<K: Ord, V>(
            left: Link<K, V>,
            mut root: Box<Node<K, V>>,
            right: Link<K, V>,
        ) -> Link<K, V> {
            let left_height = height(&left);
            let right_height = height(&right);
            if left_height > right_height.saturating_add(1) {
                let mut left = left.expect("left height requires a root");
                left.right = join_with_root(left.right.take(), root, right);
                return Some(rebalance(left));
            }
            if right_height > left_height.saturating_add(1) {
                let mut right = right.expect("right height requires a root");
                right.left = join_with_root(left, root, right.left.take());
                return Some(rebalance(right));
            }
            root.left = left;
            root.right = right;
            Some(rebalance(root))
        }

        fn join<K: Ord, V>(left: Link<K, V>, right: Link<K, V>) -> Link<K, V> {
            let Some(right) = right else {
                return left;
            };
            let (right, root) = extract_min(right);
            join_with_root(left, root, right)
        }

        fn retain_link<K: Ord, V>(
            root: Link<K, V>,
            keep: &mut impl FnMut(&K, &V) -> bool,
            removed: &mut usize,
        ) -> Link<K, V> {
            let mut root = root?;
            root.left = retain_link(root.left.take(), keep, removed);
            root.right = retain_link(root.right.take(), keep, removed);
            if keep(&root.key, &root.value) {
                let left = root.left.take();
                let right = root.right.take();
                join_with_root(left, root, right)
            } else {
                *removed += 1;
                join(root.left.take(), root.right.take())
            }
        }

        let mut removed = 0;
        self.root = retain_link(self.root.take(), &mut keep, &mut removed);
        self.len -= removed;
    }

    /// 把 `at..` 的节点移动到新表，不分配节点。
    ///
    /// @param at 新表的 inclusive lower bound。
    /// @return 拥有全部 `key >= at` entry 的表。
    pub(crate) fn split_off(&mut self, at: &K) -> Self
    where
        K: Copy,
    {
        let mut right = Self::new();
        while let Some(key) = self.ceiling(at).map(|(key, _)| *key) {
            let (root, node) = remove_node(self.root.take(), &key);
            self.root = root;
            self.len -= 1;
            right.insert_box(node.expect("ceiling key must remain present"));
        }
        right
    }

    /// 把另一个表的全部节点移动进当前表，不分配节点。
    ///
    /// @param other 成功后为空的 source 表。
    pub(crate) fn append(&mut self, other: &mut Self) {
        while let Some(node) = other.pop_first_box() {
            self.insert_box(node);
        }
    }

    fn pop_first_box(&mut self) -> Option<Box<Node<K, V>>> {
        let root = self.root.take()?;
        let (root, node) = extract_min(root);
        self.root = root;
        self.len -= 1;
        Some(node)
    }

    fn insert_box(&mut self, mut node: Box<Node<K, V>>) {
        if let Some(current) = self.get_mut(&node.key) {
            core::mem::swap(current, &mut node.value);
            return;
        }
        node.height = 1;
        node.left = None;
        node.right = None;
        self.root = Some(insert_absent(self.root.take(), node));
        self.len += 1;
    }
}

impl<K, V> Default for FallibleMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for FallibleMap<K, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Ord, V> Index<&K> for FallibleMap<K, V> {
    type Output = V;

    fn index(&self, key: &K) -> &Self::Output {
        self.get(key).expect("no entry found for key")
    }
}

impl<'a, K, V> IntoIterator for &'a FallibleMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

fn height<K, V>(node: &Link<K, V>) -> u8 {
    node.as_ref().map_or(0, |node| node.height)
}

fn update_height<K, V>(node: &mut Node<K, V>) {
    node.height = 1 + height(&node.left).max(height(&node.right));
}

fn balance_factor<K, V>(node: &Node<K, V>) -> i16 {
    i16::from(height(&node.left)) - i16::from(height(&node.right))
}

fn rotate_left<K, V>(mut root: Box<Node<K, V>>) -> Box<Node<K, V>> {
    let mut pivot = root
        .right
        .take()
        .expect("left rotation requires right child");
    root.right = pivot.left.take();
    update_height(&mut root);
    pivot.left = Some(root);
    update_height(&mut pivot);
    pivot
}

fn rotate_right<K, V>(mut root: Box<Node<K, V>>) -> Box<Node<K, V>> {
    let mut pivot = root
        .left
        .take()
        .expect("right rotation requires left child");
    root.left = pivot.right.take();
    update_height(&mut root);
    pivot.right = Some(root);
    update_height(&mut pivot);
    pivot
}

fn rebalance<K, V>(mut root: Box<Node<K, V>>) -> Box<Node<K, V>> {
    update_height(&mut root);
    match balance_factor(&root) {
        2.. => {
            if balance_factor(root.left.as_ref().expect("left-heavy AVL node lost child")) < 0 {
                root.left = root.left.take().map(rotate_left);
            }
            rotate_right(root)
        }
        ..=-2 => {
            if balance_factor(
                root.right
                    .as_ref()
                    .expect("right-heavy AVL node lost child"),
            ) > 0
            {
                root.right = root.right.take().map(rotate_right);
            }
            rotate_left(root)
        }
        _ => root,
    }
}

fn insert_absent<K: Ord, V>(root: Link<K, V>, node: Box<Node<K, V>>) -> Box<Node<K, V>> {
    let Some(mut root) = root else {
        return node;
    };
    if node.key < root.key {
        root.left = Some(insert_absent(root.left.take(), node));
    } else {
        debug_assert!(node.key > root.key);
        root.right = Some(insert_absent(root.right.take(), node));
    }
    rebalance(root)
}

fn extract_min<K, V>(mut root: Box<Node<K, V>>) -> (Link<K, V>, Box<Node<K, V>>) {
    let Some(left) = root.left.take() else {
        let right = root.right.take();
        return (right, root);
    };
    let (left, minimum) = extract_min(left);
    root.left = left;
    (Some(rebalance(root)), minimum)
}

fn remove_node<K: Ord, V>(root: Link<K, V>, key: &K) -> RemoveResult<K, V> {
    let Some(mut root) = root else {
        return (None, None);
    };
    match key.cmp(&root.key) {
        Ordering::Less => {
            let (left, removed) = remove_node(root.left.take(), key);
            root.left = left;
            (Some(rebalance(root)), removed)
        }
        Ordering::Greater => {
            let (right, removed) = remove_node(root.right.take(), key);
            root.right = right;
            (Some(rebalance(root)), removed)
        }
        Ordering::Equal => match (root.left.take(), root.right.take()) {
            (None, right) => (right, Some(root)),
            (left, None) => (left, Some(root)),
            (Some(left), Some(right)) => {
                let (right, mut successor) = extract_min(right);
                core::mem::swap(&mut root.key, &mut successor.key);
                core::mem::swap(&mut root.value, &mut successor.value);
                root.left = Some(left);
                root.right = right;
                (Some(rebalance(root)), Some(successor))
            }
        },
    }
}
