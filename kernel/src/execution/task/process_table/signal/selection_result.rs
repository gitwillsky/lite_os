/// One live process candidate's completed permission/generation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionAttempt {
    /// Signal zero completed a permitted existence probe without generation.
    Probe,
    /// Signal generation completed, including ignored or coalesced signals.
    Generated,
    /// A matching live process failed the existing permission policy.
    Denied,
}

/// Linux result of folding every matching live process candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionOutcome {
    Success(usize),
    Permission,
    NotFound,
}

/// Allocation-free accumulator for multi-process kill return semantics.
#[derive(Debug, Default)]
pub(super) struct SelectionResult {
    successful: usize,
    denied: bool,
}

impl SelectionResult {
    pub(super) const fn new() -> Self {
        Self {
            successful: 0,
            denied: false,
        }
    }

    pub(super) fn record(&mut self, attempt: SelectionAttempt) {
        match attempt {
            SelectionAttempt::Probe | SelectionAttempt::Generated => {
                // Every success consumes one distinct live TGID from a PID domain strictly
                // smaller than usize, so the counter cannot overflow.
                self.successful += 1;
            }
            SelectionAttempt::Denied => self.denied = true,
        }
    }

    pub(super) const fn finish(self) -> SelectionOutcome {
        if self.successful != 0 {
            SelectionOutcome::Success(self.successful)
        } else if self.denied {
            SelectionOutcome::Permission
        } else {
            SelectionOutcome::NotFound
        }
    }
}
