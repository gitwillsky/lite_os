use syn::{Expr, ExprCall, ImplItem, Item, visit::Visit};

use super::SourceFile;

const MEMORY_PREFIX: &str = "kernel/src/memory/";
const MMAP_PATH: &str = "kernel/src/memory/mm/mmap.rs";
const FAULT_PATH: &str = "kernel/src/memory/mm/mmap/fault.rs";
const POLICY_PATH: &str = "kernel/src/memory/mm/shootdown.rs";
const RFENCE_PATH: &str = "bootloader/src/rfence.rs";
const TRAP_VECTOR_PATH: &str = "bootloader/src/trap_vec.rs";

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(mmap) = sources.iter().find(|source| source.relative == MMAP_PATH) else {
        errors.push(format!("{MMAP_PATH}: missing mmap owner"));
        return;
    };
    let Some(fault) = sources.iter().find(|source| source.relative == FAULT_PATH) else {
        errors.push(format!("{FAULT_PATH}: missing page-fault owner"));
        return;
    };

    let lazy_flushes = ["map_anonymous", "map_private_file", "map_shared_file"]
        .into_iter()
        .map(|name| legacy_calls_in_function(mmap, name))
        .sum::<usize>();
    if lazy_flushes != 0 {
        errors.push(format!(
            "{MMAP_PATH}: lazy VMA publication creates no leaf PTE and must issue zero TLB fences; found {lazy_flushes} whole-machine flush call(s)"
        ));
    }

    let fault_flushes = legacy_calls_in_function(fault, "handle_page_fault_with_limits");
    if fault_flushes != 0 {
        errors.push(format!(
            "{FAULT_PATH}: invalid-to-valid page-fault publication must use the translation-change policy, not {fault_flushes} direct whole-machine flushes; 1 MiB/256-page first-touch currently targets 256*(online_cpus-1) remote CPU fences"
        ));
    }

    let legacy_callers = sources
        .iter()
        .filter(|source| source.relative.starts_with(MEMORY_PREFIX))
        .filter_map(|source| {
            let count = named_calls(&source.syntax, "flush_tlb_all_cpus");
            (count != 0).then_some(format!("{}:{count}", source.relative))
        })
        .collect::<Vec<_>>();
    if !legacy_callers.is_empty() {
        errors.push(format!(
            "memory translation changes must commit through the sole policy owner {POLICY_PATH}; direct whole-machine flush callers remain: {}",
            legacy_callers.join(", ")
        ));
    }
    if sources.iter().all(|source| source.relative != POLICY_PATH) {
        errors.push(format!(
            "{POLICY_PATH}: missing sole translation-change policy/commit owner"
        ));
    } else if let Some(policy) = sources.iter().find(|source| source.relative == POLICY_PATH) {
        for required in [
            "const TLB_RANGE_PAGE_LIMIT: usize = 64;",
            "1..=TLB_RANGE_PAGE_LIMIT",
            "FenceScope::All",
            "Some((0, usize::MAX))",
        ] {
            if !policy.text.contains(required) {
                errors.push(format!(
                    "{POLICY_PATH}: TLB commit cost must stay bounded at 64 page fences and normalize larger spans to one full local/remote fence; missing `{required}`"
                ));
            }
        }
    }

    let direct_remote_fences = sources
        .iter()
        .filter(|source| source.relative != POLICY_PATH)
        .filter_map(|source| {
            let count = named_calls(&source.syntax, "synchronize_tlb");
            (count != 0).then_some(format!("{}:{count}", source.relative))
        })
        .collect::<Vec<_>>();
    if !direct_remote_fences.is_empty() {
        errors.push(format!(
            "only {POLICY_PATH} may select remote TLB targets/ranges; direct platform callers remain: {}",
            direct_remote_fences.join(", ")
        ));
    }

    let Some(rfence) = sources.iter().find(|source| source.relative == RFENCE_PATH) else {
        errors.push(format!("{RFENCE_PATH}: missing SBI RFENCE owner"));
        return;
    };
    let Some(trap_vector) = sources
        .iter()
        .find(|source| source.relative == TRAP_VECTOR_PATH)
    else {
        errors.push(format!(
            "{TRAP_VECTOR_PATH}: missing RFENCE interrupt consumer"
        ));
        return;
    };
    for (source, required) in [
        (rfence, ["STARTS", "SIZES", "start_addr", "size"]),
        (
            trap_vector,
            [
                "rfence_starts",
                "rfence_sizes",
                "sfence.vma a0, x0",
                "bgeu a0, a3",
            ],
        ),
    ] {
        let missing = required
            .into_iter()
            .filter(|needle| !source.text.contains(needle))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            errors.push(format!(
                "{}: remote range fence must carry and execute the SBI [start,size) contract; missing {}",
                source.relative,
                missing.join(", ")
            ));
        }
    }
}

fn legacy_calls_in_function(source: &SourceFile, name: &str) -> usize {
    source
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(implementation) => implementation.items.iter().find_map(|item| match item {
                ImplItem::Fn(function) if function.sig.ident == name => Some(&function.block),
                _ => None,
            }),
            _ => None,
        })
        .map(|block| {
            let mut calls = NamedCalls::new("flush_tlb_all_cpus");
            calls.visit_block(block);
            calls.count
        })
        .sum()
}

fn named_calls(file: &syn::File, name: &str) -> usize {
    let mut calls = NamedCalls::new(name);
    calls.visit_file(file);
    calls.count
}

struct NamedCalls<'a> {
    name: &'a str,
    count: usize,
}

impl<'a> NamedCalls<'a> {
    fn new(name: &'a str) -> Self {
        Self { name, count: 0 }
    }
}

impl<'ast> Visit<'ast> for NamedCalls<'_> {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if matches!(&*call.func, Expr::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == self.name))
        {
            self.count += 1;
        }
        syn::visit::visit_expr_call(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_obeys_translation_fence_contract() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn legacy_whole_machine_flush_detector_rejects_regression() {
        let syntax = syn::parse_file("fn mutate() { flush_tlb_all_cpus(); }").unwrap();
        assert_eq!(named_calls(&syntax, "flush_tlb_all_cpus"), 1);
    }
}
