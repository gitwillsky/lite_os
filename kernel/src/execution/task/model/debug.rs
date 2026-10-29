use super::TaskControlBlock;

impl core::fmt::Debug for TaskControlBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            r#"
            TaskControlBlock {{
                tgid: {},
                tid: {},
                task_status: {:?}
            }}"#,
            self.tgid(),
            self.tid(),
            self.scheduling.state.lock().run_state()
        )
    }
}
