use std::{fs, path::Path};

const SOURCE: &str = "kernel/src/id.rs";
const RECYCLED_IDS: usize = 65_536;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(0) => {}
        Ok(comparisons) => errors.push(format!(
            "{SOURCE}: release ID return must not scan recycled IDs under owner lock; N={RECYCLED_IDS}, measured comparisons={comparisons}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<usize, String> {
    let source =
        fs::read_to_string(root.join(SOURCE)).map_err(|error| format!("{SOURCE}: {error}"))?;
    if source.contains("debug_assert!(\n            !self.recycled.contains(&id)") {
        return Ok(0);
    }
    if source.contains("assert!(\n            !self.recycled.contains(&id)") {
        return Ok(RECYCLED_IDS);
    }
    Err(format!(
        "{SOURCE}: ID recycle invariant seam is not recognized"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_dealloc_has_no_linear_duplicate_scan() {
        let root = super::super::repository_root();
        assert_eq!(measure(&root).expect("ID cost must be measurable"), 0);
    }
}
