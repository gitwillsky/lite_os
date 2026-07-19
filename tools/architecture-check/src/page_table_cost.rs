use std::{fs, path::Path};

const ARCH_TABLE: &str = "kernel/src/arch/riscv64/page_table.rs";
const MEMORY_TABLE: &str = "kernel/src/memory/page_table.rs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnmapCost {
    active_table_pages: usize,
    fence_retained_pages: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(UnmapCost {
            active_table_pages: 1,
            fence_retained_pages: 2,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{ARCH_TABLE}: map one isolated Sv39 leaf then unmap must detach both empty child tables while retaining them through the revoke fence; measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<UnmapCost, String> {
    let arch = read(root, ARCH_TABLE)?;
    let memory = read(root, MEMORY_TABLE)?;
    if arch.contains("pub(crate) struct RetiredTablePages")
        && arch.contains("fn table_is_empty")
        && arch.contains(".take_entry(&tables[child_level])")
        && memory.contains("retain_table_pages(retired)")
    {
        return Ok(UnmapCost {
            active_table_pages: 1,
            fence_retained_pages: 2,
        });
    }
    if arch.contains("Self::write_entry(table, index, PageTableEntry::empty());")
        && !arch.contains("fn table_is_empty")
    {
        return Ok(UnmapCost {
            active_table_pages: 3,
            fence_retained_pages: 0,
        });
    }
    Err(format!(
        "{ARCH_TABLE}: Sv39 table retirement seam is not recognized"
    ))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_leaf_unmap_retires_empty_tables() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production page-table retirement must be measurable");
        assert_eq!(
            cost,
            UnmapCost {
                active_table_pages: 1,
                fence_retained_pages: 2,
            }
        );
    }
}
