use super::Node;

/// 只持有当前节点的无分配 AVL 升序 cursor。
pub(crate) struct Iter<'a, K, V> {
    current: Option<&'a Node<K, V>>,
}

impl<'a, K, V> Iter<'a, K, V> {
    pub(super) fn new(root: Option<&'a Node<K, V>>) -> Self {
        let mut current = root;
        while let Some(left) = current.and_then(|node| node.left.as_deref()) {
            current = Some(left);
        }
        Self { current }
    }

    pub(super) fn from_key(root: Option<&'a Node<K, V>>, start: &K) -> Self
    where
        K: Ord,
    {
        let mut cursor = root;
        let mut current = None;
        while let Some(node) = cursor {
            if node.key < *start {
                cursor = node.right.as_deref();
            } else {
                current = Some(node);
                cursor = node.left.as_deref();
            }
        }
        Self { current }
    }

    pub(super) fn after_key(root: Option<&'a Node<K, V>>, start: &K) -> Self
    where
        K: Ord,
    {
        let mut cursor = root;
        let mut current = None;
        while let Some(node) = cursor {
            if node.key <= *start {
                cursor = node.right.as_deref();
            } else {
                current = Some(node);
                cursor = node.left.as_deref();
            }
        }
        Self { current }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.current.take()?;
        // SAFETY: every published next pointer targets the next live Box node in this map.
        // Iter borrows the map immutably, so structural mutation/drop cannot race this dereference.
        self.current = node.next.map(|next| unsafe { next.as_ref() });
        Some((&node.key, &node.value))
    }
}
