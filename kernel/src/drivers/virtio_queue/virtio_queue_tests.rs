use core::sync::atomic::Ordering;

use super::{VIRTQ_DESC_F_NEXT, VirtQueue, VirtqUsedElem};

fn publish_single(queue: &mut VirtQueue) -> u16 {
    let head = queue.free_head;
    let next = queue.desc_shadow[head as usize].next;
    queue.desc_shadow[head as usize].addr = 0x1000;
    queue.desc_shadow[head as usize].len = 64;
    queue.desc_shadow[head as usize].flags &= !VIRTQ_DESC_F_NEXT;
    queue.write_desc(head);
    queue.free_head = next;
    queue.num_free -= 1;
    queue.add_to_avail(head);
    head
}

fn inject_used(queue: &mut VirtQueue, head: u16, length: u32) {
    inject_used_id(queue, u32::from(head), length);
}

fn inject_used_id(queue: &mut VirtQueue, id: u32, length: u32) {
    let slot = queue.last_used_idx & (queue.size - 1);
    // SAFETY: test queue owns a complete used ring and slot is masked by its power-of-two size.
    unsafe {
        let ring = (queue.used as *mut u8).add(4) as *mut VirtqUsedElem;
        *ring.add(slot as usize) = VirtqUsedElem { id, len: length };
        (*queue.used)
            .idx
            .store(queue.last_used_idx.wrapping_add(1), Ordering::Release);
    }
}

#[test]
fn used_does_not_recycle_before_adapter_owner_claim() {
    let mut queue = VirtQueue::new(4).expect("host queue allocation must succeed");
    let head = publish_single(&mut queue);
    assert_eq!(queue.free_descriptor_count(), 3);
    inject_used(&mut queue, head, 64);

    let completion = queue.used().unwrap().unwrap();

    assert_eq!((completion.head(), completion.length()), (head, 64));
    assert_eq!(
        queue.free_descriptor_count(),
        3,
        "descriptor must remain occupied until the adapter owner claims completion"
    );
    queue.recycle_used(completion).unwrap();
    assert_eq!(queue.free_descriptor_count(), 4);
}

#[test]
fn duplicate_completion_cannot_recycle_the_same_chain_twice() {
    let mut queue = VirtQueue::new(4).expect("host queue allocation must succeed");
    let head = publish_single(&mut queue);
    inject_used(&mut queue, head, 64);
    let first = queue.used().unwrap().unwrap();
    assert_eq!(first.head(), head);
    queue.recycle_used(first).unwrap();
    assert_eq!(queue.free_descriptor_count(), 4);

    inject_used(&mut queue, head, 64);
    let duplicate = queue.used().unwrap().unwrap();
    assert_eq!(duplicate.head(), head);
    drop(duplicate); // adapter owner rejects the already-cleared head and resets the device.

    assert_eq!(queue.free_descriptor_count(), 4);
    assert!(
        queue.used().is_err(),
        "rejected token must latch terminal state"
    );
}

#[test]
fn unknown_completion_preserves_the_live_descriptor_chain() {
    let mut queue = VirtQueue::new(4).expect("host queue allocation must succeed");
    let published = publish_single(&mut queue);
    let unknown = (published + 1) & (queue.size - 1);
    inject_used(&mut queue, unknown, 64);

    let completion = queue.used().unwrap().unwrap();
    assert_eq!(completion.head(), unknown);
    drop(completion); // adapter owner has no mapping for this valid-but-unowned head.

    assert_eq!(queue.free_descriptor_count(), 3);
    assert!(
        queue.used().is_err(),
        "unknown head must terminate the queue"
    );
}

#[test]
fn out_of_range_completion_latches_failure_without_recycling() {
    let mut queue = VirtQueue::new(4).expect("host queue allocation must succeed");
    publish_single(&mut queue);
    let invalid_id = u32::from(queue.size);
    inject_used_id(&mut queue, invalid_id, 64);

    assert!(queue.used().is_err());
    assert_eq!(queue.free_descriptor_count(), 3);
    assert!(
        queue.used().is_err(),
        "invalid ring identity must stay terminal"
    );
}
