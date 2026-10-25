use super::Progress;

#[test]
fn the_bar_never_runs_ahead_of_the_real_milestone() {
    let mut progress = Progress::new(400);
    progress.advance(10.0);
    assert_eq!(progress.permille(), 400, "no milestone, no movement");
    progress.raise_target(600);
    progress.advance(10.0);
    assert_eq!(
        progress.permille(),
        600,
        "a long pause still stops at the milestone"
    );
    assert!(!progress.animating());
}

#[test]
fn the_bar_catches_up_smoothly_and_in_proportion_to_elapsed_time() {
    let mut progress = Progress::new(400);
    progress.raise_target(1000);
    progress.advance(0.1);
    assert_eq!(progress.permille(), 445);
    assert!(progress.animating());
    for _ in 0..100 {
        progress.advance(0.1);
    }
    assert!(progress.complete());
}

#[test]
fn targets_only_rise_and_are_capped_at_the_full_bar() {
    let mut progress = Progress::new(400);
    progress.raise_target(800);
    progress.raise_target(500);
    progress.advance(100.0);
    assert_eq!(progress.permille(), 800);
    progress.raise_target(5000);
    progress.advance(100.0);
    assert_eq!(progress.permille(), 1000);
}

#[test]
fn small_steps_accumulate_instead_of_being_rounded_away() {
    let mut progress = Progress::new(0);
    progress.raise_target(1000);
    for _ in 0..100 {
        progress.advance(0.001);
    }
    assert_eq!(progress.permille(), 45);
}

#[test]
fn raise_target_reports_whether_the_milestone_was_new() {
    let mut progress = Progress::new(400);
    assert!(progress.raise_target(600));
    assert!(!progress.raise_target(600), "same milestone again");
    assert!(!progress.raise_target(500), "an older milestone");
    assert!(progress.raise_target(9999), "capped at 1000 but still new");
    assert!(!progress.raise_target(1000));
}
