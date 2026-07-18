use super::unit_source_size::{
    UNIT_SOURCE_ROOTS, UNIT_TEST_MODULE_ROOTS, UnitSourceSizeDisposition, classify,
};

#[test]
fn unit_source_roots_cover_host_test_crates_and_checker_test_modules() {
    assert_eq!(
        UNIT_SOURCE_ROOTS,
        ["tools/kernel-unit/src", "tools/scheduler-unit/src"]
    );
    assert_eq!(UNIT_TEST_MODULE_ROOTS, ["tools/architecture-check/src"]);
}

#[test]
fn unit_source_size_boundaries_match_production_thresholds() {
    assert_eq!(classify(600), UnitSourceSizeDisposition::Accepted);
    assert_eq!(classify(601), UnitSourceSizeDisposition::Review);
    assert_eq!(classify(1_200), UnitSourceSizeDisposition::Review);
    assert_eq!(classify(1_201), UnitSourceSizeDisposition::Reject);
}
