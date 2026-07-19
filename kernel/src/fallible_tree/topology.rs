use alloc::boxed::Box;
use core::cmp::Ordering;

use super::{Link, Node};

type RemoveResult<K, V> = (Link<K, V>, Option<Box<Node<K, V>>>);

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

pub(super) fn insert_absent<K: Ord, V>(root: Link<K, V>, node: Box<Node<K, V>>) -> Box<Node<K, V>> {
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

pub(super) fn remove_node<K: Ord, V>(root: Link<K, V>, key: &K) -> RemoveResult<K, V> {
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
                root.next = successor.next;
                root.left = Some(left);
                root.right = right;
                (Some(rebalance(root)), Some(successor))
            }
        },
    }
}

/// 连接严格有序的 `left < root < right`，只沿较高一侧的 AVL spine 修改结构。
pub(super) fn join_with_root<K, V>(
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

/// 连接严格有序且不相交的两棵树；ordering 由上层边界检查或结构归纳保证。
pub(super) fn join_ordered<K, V>(left: Link<K, V>, right: Link<K, V>) -> Link<K, V> {
    let Some(right) = right else {
        return left;
    };
    let (right, root) = extract_min(right);
    join_with_root(left, root, right)
}

/// 以 `at` 结构化切开 AVL，只比较 root-to-leaf path，不逐节点 remove/reinsert。
pub(super) fn split<K: Ord, V>(root: Link<K, V>, at: &K) -> (Link<K, V>, Link<K, V>) {
    let Some(mut root) = root else {
        return (None, None);
    };
    if root.key < *at {
        let left = root.left.take();
        let (middle, right) = split(root.right.take(), at);
        (join_with_root(left, root, middle), right)
    } else {
        let right = root.right.take();
        let (left, middle) = split(root.left.take(), at);
        (left, join_with_root(middle, root, right))
    }
}

/// 精确计数一棵树；每个节点访问一次且不比较 key。
pub(super) fn count_nodes<K, V>(root: &Link<K, V>) -> usize {
    root.as_ref().map_or(0, |root| {
        1 + count_nodes(&root.left) + count_nodes(&root.right)
    })
}

/// 一次消费全部节点并保留匹配项，再从有序 node list 线性重建平衡树。
pub(super) fn retain_linear<K, V>(
    root: Link<K, V>,
    keep: &mut impl FnMut(&K, &V) -> bool,
) -> (Link<K, V>, usize) {
    fn prepend_filtered<K, V>(
        mut root: Box<Node<K, V>>,
        head: &mut Link<K, V>,
        kept: &mut usize,
        keep: &mut impl FnMut(&K, &V) -> bool,
    ) {
        let left = root.left.take();
        let right = root.right.take();
        if let Some(right) = right {
            prepend_filtered(right, head, kept, keep);
        }
        if keep(&root.key, &root.value) {
            root.height = 1;
            root.next = head.as_deref().map(core::ptr::NonNull::from);
            root.right = head.take();
            *head = Some(root);
            *kept += 1;
        }
        if let Some(left) = left {
            prepend_filtered(left, head, kept, keep);
        }
    }

    fn build_balanced<K, V>(head: &mut Link<K, V>, count: usize) -> Link<K, V> {
        if count == 0 {
            return None;
        }
        let left_count = count / 2;
        let left = build_balanced(head, left_count);
        let mut root = head.take().expect("retained AVL list shorter than count");
        *head = root.right.take();
        root.left = left;
        root.right = build_balanced(head, count - left_count - 1);
        update_height(&mut root);
        Some(root)
    }

    let mut head = None;
    let mut kept = 0;
    if let Some(root) = root {
        prepend_filtered(root, &mut head, &mut kept, keep);
    }
    let root = build_balanced(&mut head, kept);
    debug_assert!(head.is_none());
    (root, kept)
}

/// 返回最大 key，只访问 AVL 的 right spine。
pub(super) fn last_key<K, V>(root: &Link<K, V>) -> Option<&K> {
    let mut cursor = root.as_deref()?;
    while let Some(right) = cursor.right.as_deref() {
        cursor = right;
    }
    Some(&cursor.key)
}
