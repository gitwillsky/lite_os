//! Native handoff must publish outgoing ownership before assembly and finish after it.
use std::{fs, path::Path};

#[test]
fn outgoing_context_is_saved_before_handoff_completion() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = fs::read_to_string(root.join("kernel/src/thread/context_switch.rs")).unwrap();
    let schedule = source.split("fn schedule(thread: Arc<Thread>) {").nth(1).unwrap().split("pub(super) fn switch_from_idle").next().unwrap();
    let select = schedule.find("let next = select_switch_target();").unwrap();
    let publish = schedule.find("publish_pending_handoff(thread, handoff_irq);").unwrap();
    let switch = schedule.find("switch_kernel_context(save, restore)").unwrap();
    let finish = schedule.find("complete_pending_handoff();").unwrap();
    assert!(select < publish && publish < switch && switch < finish);
    assert_eq!(schedule.matches("switch_kernel_context(").count(), 1);
    assert!(schedule.contains("next.unwrap_or_else"));
    let entry = fs::read_to_string(root.join("kernel/src/thread/mod.rs")).unwrap();
    assert!(entry.contains("fn run_user_thread() -> ! {\n    context_switch::complete_pending_handoff();"));
}
