use crate::fault_preflight;

#[cfg(test)]
mod fault_preflight_tests {
    use super::fault_preflight::{
        FaultAccess, FaultPermissions, FaultPreflight, FaultResidency, FileFaultState,
        preflight_fault,
    };

    #[test]
    fn unmapped_fault_does_not_inspect_file_state() {
        let outcome = preflight_fault(
            false,
            FaultPermissions::new(true, true, true, true),
            FaultAccess::Read,
            || -> Result<FileFaultState, ()> {
                panic!("an unmapped fault must not project a file page")
            },
            || FaultResidency::Private {
                lazy: true,
                resident: false,
            },
        )
        .unwrap();

        assert_eq!(outcome, FaultPreflight::SegmentationFault);
    }

    #[test]
    fn denied_access_never_requests_a_private_frame() {
        let cases = [
            (
                FaultPermissions::new(true, false, false, false),
                FaultAccess::Read,
            ),
            (
                FaultPermissions::new(true, true, false, false),
                FaultAccess::Write,
            ),
            (
                FaultPermissions::new(true, true, true, false),
                FaultAccess::Execute,
            ),
        ];

        for (permissions, access) in cases {
            assert_eq!(
                preflight_fault(
                    true,
                    permissions,
                    access,
                    || -> Result<FileFaultState, ()> {
                        panic!("a denied fault must not project a file page")
                    },
                    || FaultResidency::Private {
                        lazy: true,
                        resident: false,
                    },
                )
                .unwrap(),
                FaultPreflight::SegmentationFault
            );
        }
    }

    #[test]
    fn private_file_eof_precedes_residency_allocation() {
        let permissions = FaultPermissions::new(true, true, false, false);
        let residency = FaultResidency::Private {
            lazy: true,
            resident: false,
        };

        assert_eq!(
            preflight_fault(
                true,
                permissions,
                FaultAccess::Read,
                || Ok::<_, ()>(FileFaultState::BeyondEof),
                || -> FaultResidency { panic!("EOF classification must precede residency lookup") },
            )
            .unwrap(),
            FaultPreflight::BusError
        );
        assert_eq!(
            preflight_fault(
                true,
                permissions,
                FaultAccess::Read,
                || Ok::<_, ()>(FileFaultState::Available),
                || residency,
            )
            .unwrap(),
            FaultPreflight::NeedsPrivateFrame
        );
    }

    #[test]
    fn residency_owner_selects_the_fault_path() {
        let permissions = FaultPermissions::new(true, true, true, false);
        let cases = [
            (FaultResidency::Device, FaultPreflight::Device),
            (
                FaultResidency::SharedAnonymous,
                FaultPreflight::SharedAnonymous,
            ),
            (FaultResidency::SharedFile, FaultPreflight::SharedFile),
            (
                FaultResidency::Private {
                    lazy: true,
                    resident: true,
                },
                FaultPreflight::Private,
            ),
            (
                FaultResidency::Private {
                    lazy: false,
                    resident: false,
                },
                FaultPreflight::Private,
            ),
        ];

        for (residency, expected) in cases {
            assert_eq!(
                preflight_fault(
                    true,
                    permissions,
                    FaultAccess::Read,
                    || Ok::<_, ()>(FileFaultState::NotFile),
                    || residency,
                )
                .unwrap(),
                expected
            );
        }
    }
}
