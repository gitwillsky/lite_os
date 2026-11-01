use alloc::boxed::Box;
use core::borrow::Borrow;
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

pub(super) fn remove_node<K: Ord + Borrow<Q>, Q: Ord + ?Sized, V>(
    root: Link<K, V>,
    key: &Q,
) -> RemoveResult<K, V> {
    let Some(mut root) = root else {
        return (None, None);
    };
    match key.cmp(root.key.borrow()) {
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
