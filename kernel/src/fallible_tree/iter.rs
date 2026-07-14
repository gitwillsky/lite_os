use super::Node;

// AVL 最小节点数按 Fibonacci 增长；可寻址节点不超过 usize::MAX，因此树高严格小于
// 2 * usize::BITS。固定栈避免只读迭代器在 reclaim/OOM 路径再次请求 heap。
const MAX_HEIGHT: usize = usize::BITS as usize * 2;

/// 固定栈、无分配的 AVL 升序迭代器。
pub(crate) struct Iter<'a, K, V> {
    stack: [Option<&'a Node<K, V>>; MAX_HEIGHT],
    depth: usize,
}

impl<'a, K, V> Iter<'a, K, V> {
    pub(super) fn new(root: Option<&'a Node<K, V>>) -> Self {
        let mut iter = Self {
            stack: [None; MAX_HEIGHT],
            depth: 0,
        };
        iter.push_left(root);
        iter
    }

    pub(super) fn from_key(root: Option<&'a Node<K, V>>, start: &K) -> Self
    where
        K: Ord,
    {
        let mut iter = Self {
            stack: [None; MAX_HEIGHT],
            depth: 0,
        };
        let mut cursor = root;
        while let Some(node) = cursor {
            if node.key < *start {
                cursor = node.right.as_deref();
            } else {
                iter.push(node);
                cursor = node.left.as_deref();
            }
        }
        iter
    }

    pub(super) fn after_key(root: Option<&'a Node<K, V>>, start: &K) -> Self
    where
        K: Ord,
    {
        let mut iter = Self {
            stack: [None; MAX_HEIGHT],
            depth: 0,
        };
        let mut cursor = root;
        while let Some(node) = cursor {
            if node.key <= *start {
                cursor = node.right.as_deref();
            } else {
                iter.push(node);
                cursor = node.left.as_deref();
            }
        }
        iter
    }

    fn push_left(&mut self, mut cursor: Option<&'a Node<K, V>>) {
        while let Some(node) = cursor {
            self.push(node);
            cursor = node.left.as_deref();
        }
    }

    fn push(&mut self, node: &'a Node<K, V>) {
        assert!(
            self.depth < MAX_HEIGHT,
            "AVL iterator height bound violated"
        );
        self.stack[self.depth] = Some(node);
        self.depth += 1;
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.depth = self.depth.checked_sub(1)?;
        let node = self.stack[self.depth]
            .take()
            .expect("AVL iterator stack hole");
        self.push_left(node.right.as_deref());
        Some((&node.key, &node.value))
    }
}
