#![cfg(test)]

use core::{fmt, ptr::NonNull};

use super::{FallibleMap, Link, Node};

#[cfg(test)]
impl<K, V> FallibleMap<K, V> {
    pub(crate) fn test_visit_node_addresses(&self, mut visit: impl FnMut(usize)) {
        fn walk<K, V>(root: &Link<K, V>, visit: &mut impl FnMut(usize)) {
            let Some(root) = root else {
                return;
            };
            visit(&**root as *const _ as usize);
            walk(&root.left, visit);
            walk(&root.right, visit);
        }
        walk(&self.root, &mut visit);
    }

    pub(crate) fn test_root_height(&self) -> u8 {
        self.root.as_ref().map_or(0, |root| root.height)
    }
}

#[cfg(test)]
impl<K: Ord + fmt::Debug, V> FallibleMap<K, V> {
    pub(crate) fn test_assert_invariants(&self) {
        fn walk<'a, K: Ord + fmt::Debug, V>(
            root: &'a Link<K, V>,
            lower: Option<&'a K>,
            upper: Option<&'a K>,
        ) -> (u8, usize) {
            let Some(root) = root else {
                return (0, 0);
            };
            if let Some(lower) = lower {
                assert!(lower < &root.key, "AVL lower ordering bound violated");
            }
            if let Some(upper) = upper {
                assert!(&root.key < upper, "AVL upper ordering bound violated");
            }
            let (left_height, left_len) = walk(&root.left, lower, Some(&root.key));
            let (right_height, right_len) = walk(&root.right, Some(&root.key), upper);
            assert!(left_height.abs_diff(right_height) <= 1);
            let height = 1 + left_height.max(right_height);
            assert!(root.height == height, "stale AVL height at {:?}", root.key);
            (height, 1 + left_len + right_len)
        }

        fn walk_links<'a, K, V>(root: &'a Link<K, V>, previous: &mut Option<&'a Node<K, V>>) {
            let Some(root) = root else {
                return;
            };
            walk_links(&root.left, previous);
            if let Some(previous) = previous {
                assert!(previous.next == Some(NonNull::from(&**root)));
            }
            *previous = Some(root);
            walk_links(&root.right, previous);
        }

        let (_, structural_len) = walk(&self.root, None, None);
        assert!(structural_len == self.len, "stale AVL map length");
        let mut previous = None;
        walk_links(&self.root, &mut previous);
        assert!(previous.is_none_or(|node| node.next.is_none()));
        assert!(
            self.iter().count() == self.len,
            "AVL iterator lost an entry"
        );
    }
}
