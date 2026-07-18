use std::{
    sync::{Arc, mpsc},
    thread,
};

use spin::Mutex;

#[derive(Default)]
struct TerminalOrderModel {
    output_complete: bool,
    echo_complete: bool,
    pending_raw: usize,
    pending_cooked: usize,
    settings_applied: bool,
}

#[test]
fn settings_transition_waits_for_output_and_echo_before_flushing_input() {
    let terminal = Arc::new(Mutex::new(TerminalOrderModel {
        pending_raw: 17,
        pending_cooked: 23,
        ..TerminalOrderModel::default()
    }));
    let (output_started_tx, output_started_rx) = mpsc::channel();
    let (finish_output_tx, finish_output_rx) = mpsc::channel();
    let output_terminal = terminal.clone();
    let output = thread::spawn(move || {
        let mut state = output_terminal.lock();
        output_started_tx.send(()).unwrap();
        finish_output_rx.recv().unwrap();
        state.output_complete = true;
        state.echo_complete = true;
    });

    output_started_rx.recv().unwrap();
    let (transition_started_tx, transition_started_rx) = mpsc::channel();
    let (transition_complete_tx, transition_complete_rx) = mpsc::channel();
    let transition_terminal = terminal.clone();
    let transition = thread::spawn(move || {
        transition_started_tx.send(()).unwrap();
        let mut state = transition_terminal.lock();
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

    let state = terminal.lock();
    assert!(state.output_complete);
    assert!(state.echo_complete);
    assert!(state.settings_applied);
    assert_eq!((state.pending_raw, state.pending_cooked), (0, 0));
}
