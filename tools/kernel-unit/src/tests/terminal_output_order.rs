use std::{
    sync::{Arc, mpsc},
    thread,
};

use spin::Mutex;

#[derive(Default)]
struct TerminalOrderModel {
    output_complete: bool,
    echo_complete: bool,
    readiness_reentered: bool,
    pending_raw: usize,
    pending_cooked: usize,
    settings_applied: bool,
}

#[derive(Default)]
struct TerminalOrder {
    output_transaction: Mutex<()>,
    input_transaction: Mutex<()>,
    state: Mutex<TerminalOrderModel>,
}

#[test]
fn readiness_reentry_completes_before_ordered_termios_transition() {
    let terminal = Arc::new(TerminalOrder {
        state: Mutex::new(TerminalOrderModel {
            pending_raw: 17,
            pending_cooked: 23,
            ..TerminalOrderModel::default()
        }),
        ..TerminalOrder::default()
    });
    let (output_started_tx, output_started_rx) = mpsc::channel();
    let (finish_output_tx, finish_output_rx) = mpsc::channel();
    let output_terminal = terminal.clone();
    let output = thread::spawn(move || {
        let _output = output_terminal.output_transaction.lock();
        let _output_flags = output_terminal.state.lock();
        drop(_output_flags);
        output_started_tx.send(()).unwrap();
        finish_output_rx.recv().unwrap();
        // Console/Pipe publication synchronously re-enters Terminal::input_ready.
        // The state lock must therefore be free while the output transaction remains held.
        let mut state = output_terminal.state.lock();
        state.readiness_reentered = true;
        state.output_complete = true;
        state.echo_complete = true;
    });

    output_started_rx.recv().unwrap();
    let (transition_started_tx, transition_started_rx) = mpsc::channel();
    let (transition_complete_tx, transition_complete_rx) = mpsc::channel();
    let transition_terminal = terminal.clone();
    let transition = thread::spawn(move || {
        transition_started_tx.send(()).unwrap();
        let _output = transition_terminal.output_transaction.lock();
        let _input = transition_terminal.input_transaction.lock();
        let mut state = transition_terminal.state.lock();
        assert!(state.readiness_reentered);
        assert!(state.output_complete);
        assert!(state.echo_complete);
        state.pending_raw = 0;
        state.pending_cooked = 0;
        state.settings_applied = true;
        transition_complete_tx.send(()).unwrap();
    });

    transition_started_rx.recv().unwrap();
    assert!(matches!(
        transition_complete_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    finish_output_tx.send(()).unwrap();
    transition_complete_rx.recv().unwrap();
    output.join().unwrap();
    transition.join().unwrap();

    let state = terminal.state.lock();
    assert!(state.readiness_reentered);
    assert!(state.output_complete);
    assert!(state.echo_complete);
    assert!(state.settings_applied);
    assert_eq!((state.pending_raw, state.pending_cooked), (0, 0));
}
