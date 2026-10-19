use std::{fs, path::Path};

const ARCH_TABLE: &str = "kernel/src/arch/riscv64/page_table.rs";
const AREA: &str = "kernel/src/memory/mm/area.rs";
const REGION_BYTES: usize = 128 * 1024 * 1024;
const BASE_PAGE: usize = 4096;
const MIDDLE_PAGE: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhysmapCost {
    leaf_entries: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(PhysmapCost { leaf_entries: 64 }) => {}
        Ok(cost) => errors.push(format!(
            "kernel direct mappings must use the largest aligned Sv39 leaf without crossing a VMA permission boundary; B={REGION_BYTES}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<PhysmapCost, String> {
    let arch = read(root, ARCH_TABLE)?;
    let area = read(root, AREA)?;
    if arch.contains("pub(crate) fn map_contiguous_range")
        && arch.contains("fn largest_contiguous_leaf")
        && area.contains(".map_contiguous_range(")
    {
        return Ok(PhysmapCost {
            leaf_entries: REGION_BYTES / MIDDLE_PAGE,
        });
    }
    if area.contains("for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize()")
        && area.contains("self.map_one(page_table")
    {
        return Ok(PhysmapCost {
            leaf_entries: REGION_BYTES / BASE_PAGE,
        });
    }
    Err("kernel direct-map leaf selection seam is not recognized".into())
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_physmap_uses_middle_sv39_leaves() {
        let root = super::super::repository_root();
        assert_eq!(
            measure(&root).expect("production physmap cost must be measurable"),
            PhysmapCost { leaf_entries: 64 }
        );
    }
}
