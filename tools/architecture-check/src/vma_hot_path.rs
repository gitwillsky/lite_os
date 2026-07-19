use quote::ToTokens;
use syn::{ImplItem, Item};

use super::SourceFile;

const MM_PATH: &str = "kernel/src/memory/mm.rs";
const MMAP_PATH: &str = "kernel/src/memory/mm/mmap.rs";
const FAULT_PATH: &str = "kernel/src/memory/mm/mmap/fault.rs";
const INDEX_STATE_PATH: &str = "kernel/src/memory/mm/vma_index_state.rs";
const FIRST_TOUCH_PAGES: usize = 1024 * 1024 / 4096;
const MODEL_VMAS: usize = 100;

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(mm) = source(sources, MM_PATH, errors) else {
        return;
    };
    let Some(mmap) = source(sources, MMAP_PATH, errors) else {
        return;
    };
    let Some(fault) = source(sources, FAULT_PATH, errors) else {
        return;
    };

    let stack_body = method_body(fault, "grow_stack_for_fault").unwrap_or_default();
    if scans_all_areas(&stack_body) {
        errors.push(format!(
            "{FAULT_PATH}: grow_stack_for_fault linearly scans the VMA index on every page fault; {MODEL_VMAS} VMAs × {FIRST_TOUCH_PAGES} pages of 1 MiB prepare_user_write = {} VMA node visits before residency work",
            MODEL_VMAS * FIRST_TOUCH_PAGES
        ));
    }
    if !stack_body.contains("vma_index_state . stack_start") {
        errors.push(format!(
            "{FAULT_PATH}: grow_stack_for_fault must resolve the unique stack through VmaIndexState"
        ));
    }

    let scans = [
        (MM_PATH, "virtual_bytes", method_body(mm, "virtual_bytes")),
        (MM_PATH, "data_bytes", method_body(mm, "data_bytes")),
        (MM_PATH, "push", method_body(mm, "push")),
        (
            MMAP_PATH,
            "range_is_free",
            method_body(mmap, "range_is_free"),
        ),
        (
            MMAP_PATH,
            "merge_adjacent_anonymous",
            method_body(mmap, "merge_adjacent_anonymous"),
        ),
    ]
    .into_iter()
    .filter(|(_, _, body)| body.as_deref().is_some_and(scans_all_areas))
    .map(|(path, method, _)| format!("{path}::{method}"))
    .collect::<Vec<_>>();
    if !scans.is_empty() {
        errors.push(format!(
            "a hinted mmap traverses the same ordered VMA index {} full time(s): {}; at {MODEL_VMAS} VMAs this is at least {} node visits before page-table work",
            scans.len(),
            scans.join(", "),
            scans.len() * MODEL_VMAS
        ));
    }

    if sources
        .iter()
        .all(|source| source.relative != INDEX_STATE_PATH)
    {
        errors.push(format!(
            "{INDEX_STATE_PATH}: missing authoritative stack-key/RLIMIT projection owner"
        ));
    }

    for (method, required) in [
        ("commit_area", ["account_area", "areas . commit_vacant"]),
        ("take_area_entry", ["areas . take_entry", "unaccount_area"]),
        (
            "account_area",
            ["vma_index_state . publish", "index_contribution"],
        ),
        (
            "unaccount_area",
            ["vma_index_state . retire", "index_contribution"],
        ),
    ] {
        let body = method_body(mm, method).unwrap_or_default();
        let missing = required
            .into_iter()
            .filter(|pattern| !body.contains(pattern))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            errors.push(format!(
                "{MM_PATH}::{method} must atomically maintain VmaIndexState and the ordered node; missing {}",
                missing.join(", ")
            ));
        }
    }

    let bypasses = sources
        .iter()
        .filter(|source| {
            source.relative.starts_with("kernel/src/memory/mm/")
                && source.relative != INDEX_STATE_PATH
        })
        .filter_map(|source| {
            let tokens = source.syntax.to_token_stream().to_string();
            [
                "areas . commit_vacant",
                "areas . take_entry",
                "areas . remove",
            ]
            .into_iter()
            .find(|pattern| tokens.contains(pattern))
            .map(|pattern| format!("{}:{pattern}", source.relative))
        })
        .collect::<Vec<_>>();
    if !bypasses.is_empty() {
        errors.push(format!(
            "VMA structural mutation bypasses MemorySet accounting transaction: {}",
            bypasses.join(", ")
        ));
    }
}

fn source<'a>(
    sources: &'a [SourceFile],
    path: &str,
    errors: &mut Vec<String>,
) -> Option<&'a SourceFile> {
    let source = sources.iter().find(|source| source.relative == path);
    if source.is_none() {
        errors.push(format!("{path}: missing VMA owner"));
    }
    source
}

fn method_body(source: &SourceFile, name: &str) -> Option<String> {
    source.syntax.items.iter().find_map(|item| match item {
        Item::Impl(implementation) => implementation.items.iter().find_map(|item| match item {
            ImplItem::Fn(function) if function.sig.ident == name => {
                Some(function.block.to_token_stream().to_string())
            }
            _ => None,
        }),
        _ => None,
    })
}

fn scans_all_areas(body: &str) -> bool {
    [
        "self . areas . iter",
        "self . areas . values",
        "& self . areas",
    ]
    .into_iter()
    .any(|pattern| body.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_vma_hot_paths_are_logarithmic_or_constant() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn scan_detector_catches_real_fallible_map_iteration_shape() {
        assert!(scans_all_areas(
            "self . areas . values () . any (| area | true)"
        ));
        assert!(!scans_all_areas("self . areas . predecessor (& key)"));
    }
}
