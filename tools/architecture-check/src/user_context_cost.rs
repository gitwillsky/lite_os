use std::{fs, path::Path};

use quote::ToTokens;
use syn::{Arm, ExprMatch, ImplItem, Item, ItemFn, visit::Visit};

use super::SourceFile;

const TRAP_SOURCE: &str = "kernel/src/trap/mod.rs";
const ADDRESS_SPACE_SOURCE: &str = "kernel/src/task/model/address_space.rs";
const USER_CONTEXT_SOURCE: &str = "kernel/src/arch/riscv64/user_context.rs";
const USER_CONTEXT_BYTES: usize = 72 * core::mem::size_of::<u64>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContextCost {
    pub(super) full_copies: usize,
    pub(super) copied_bytes: usize,
    pub(super) address_space_locks: usize,
    pub(super) page_table_walks: usize,
}

pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let expected = ContextCost {
        full_copies: 0,
        copied_bytes: 0,
        address_space_locks: 0,
        page_table_walks: 0,
    };
    match measure_simple_syscall(root) {
        Ok(cost) if cost == expected => {}
        Ok(cost) => errors.push(format!(
            "{TRAP_SOURCE}: simple syscall UserContext tax must be zero, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
    check_call_graph(sources, errors);
}

fn check_call_graph(sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources {
        if source.text.contains("load_user_context(") || source.text.contains("set_user_context(") {
            errors.push(format!(
                "{}: legacy full UserContext copy interface is forbidden",
                source.relative
            ));
        }
    }
    let Some(trap) = sources.iter().find(|source| source.relative == TRAP_SOURCE) else {
        errors.push(format!("{TRAP_SOURCE}: missing syscall context caller"));
        return;
    };
    let expected = [
        (".take_syscall_request()", 1usize),
        (".complete_syscall(", 2usize),
        (".prepare_user_return(", 1usize),
    ];
    for (call, count) in expected {
        let actual = trap.text.matches(call).count();
        if actual != count {
            errors.push(format!(
                "{TRAP_SOURCE}: expected {count} `{call}` call(s), found {actual}"
            ));
        }
    }
    let clone_snapshots = sources
        .iter()
        .map(|source| {
            source
                .text
                .matches(".snapshot_user_context_for_clone()")
                .count()
        })
        .sum::<usize>();
    let owner_snapshots = sources
        .iter()
        .filter(|source| source.relative == "kernel/src/task/model/trap_context.rs")
        .map(|source| source.text.matches(".snapshot_for_clone()").count())
        .sum::<usize>();
    if clone_snapshots != 2 || owner_snapshots != 1 {
        errors.push(format!(
            "UserContext full snapshot must remain clone-only: clone callers={clone_snapshots}, owner adapters={owner_snapshots}"
        ));
    }
}

pub(super) fn measure_simple_syscall(root: &Path) -> Result<ContextCost, String> {
    let trap_text = read(root, TRAP_SOURCE)?;
    let trap = syn::parse_file(&trap_text).map_err(|error| error.to_string())?;
    let address_text = read(root, ADDRESS_SPACE_SOURCE)?;
    let address = syn::parse_file(&address_text).map_err(|error| error.to_string())?;
    let context_text = read(root, USER_CONTEXT_SOURCE)?;
    if !context_text.contains("size_of::<UserContext>() == 72 * WORD") {
        return Err(format!(
            "{USER_CONTEXT_SOURCE}: expected fixed riscv64 72-word UserContext layout"
        ));
    }

    let handle = function(&trap, "handle_user_trap")
        .ok_or_else(|| format!("{TRAP_SOURCE}: missing handle_user_trap"))?;
    let syscall_arm = user_environment_call_arm(handle)
        .ok_or_else(|| format!("{TRAP_SOURCE}: missing UserEnvironmentCall arm"))?;
    let trap_return = function(&trap, "trap_return")
        .ok_or_else(|| format!("{TRAP_SOURCE}: missing trap_return"))?;
    let mut calls = ContextCalls::default();
    calls.visit_expr(&syscall_arm.body);
    calls.visit_item_fn(trap_return);

    let full_copies = calls.loads + calls.stores;
    if full_copies == 0 {
        return Ok(ContextCost {
            full_copies: 0,
            copied_bytes: 0,
            address_space_locks: 0,
            page_table_walks: 0,
        });
    }

    let load = method(&address, "load_user_context")
        .ok_or_else(|| format!("{ADDRESS_SPACE_SOURCE}: missing measured load_user_context"))?;
    let store = method(&address, "set_user_context")
        .ok_or_else(|| format!("{ADDRESS_SPACE_SOURCE}: missing measured set_user_context"))?;
    let context_va = method(&address, "user_context_va")
        .ok_or_else(|| format!("{ADDRESS_SPACE_SOURCE}: missing user_context_va"))?;
    let mut access = AccessCost::default();
    access.visit_block(&load.block);
    access.visit_block(&store.block);
    let mut context_va_access = AccessCost::default();
    context_va_access.visit_block(&context_va.block);

    let per_pair_locks = access.locks + 2 * context_va_access.locks;
    let pairs = calls.loads.min(calls.stores);
    Ok(ContextCost {
        full_copies,
        copied_bytes: full_copies * USER_CONTEXT_BYTES,
        address_space_locks: pairs * per_pair_locks,
        page_table_walks: pairs * access.walks,
    })
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn function<'a>(file: &'a syn::File, name: &str) -> Option<&'a ItemFn> {
    file.items.iter().find_map(|item| match item {
        Item::Fn(function) if function.sig.ident == name => Some(function),
        _ => None,
    })
}

fn method<'a>(file: &'a syn::File, name: &str) -> Option<&'a syn::ImplItemFn> {
    file.items.iter().find_map(|item| match item {
        Item::Impl(implementation) => implementation.items.iter().find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == name => Some(method),
            _ => None,
        }),
        _ => None,
    })
}

fn user_environment_call_arm(function: &ItemFn) -> Option<&Arm> {
    struct Finder<'ast> {
        arm: Option<&'ast Arm>,
    }
    impl<'ast> Visit<'ast> for Finder<'ast> {
        fn visit_expr_match(&mut self, expression: &'ast ExprMatch) {
            if self.arm.is_none() {
                self.arm = expression.arms.iter().find(|arm| {
                    arm.pat
                        .to_token_stream()
                        .to_string()
                        .ends_with("UserEnvironmentCall")
                });
            }
            if self.arm.is_none() {
                syn::visit::visit_expr_match(self, expression);
            }
        }
    }
    let mut finder = Finder { arm: None };
    finder.visit_item_fn(function);
    finder.arm
}

#[derive(Default)]
struct ContextCalls {
    loads: usize,
    stores: usize,
}

impl<'ast> Visit<'ast> for ContextCalls {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        match call.method.to_string().as_str() {
            "load_user_context" => self.loads += 1,
            "set_user_context" => self.stores += 1,
            _ => {}
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

#[derive(Default)]
struct AccessCost {
    locks: usize,
    walks: usize,
}

impl<'ast> Visit<'ast> for AccessCost {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        match call.method.to_string().as_str() {
            "lock" => self.locks += 1,
            "trap_context_ppn" => self.walks += 1,
            _ => {}
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_syscall_has_no_full_context_copy_lock_or_walk_tax() {
        let root = super::super::repository_root();
        let cost =
            measure_simple_syscall(&root).expect("production context cost must be measurable");
        assert_eq!(
            cost,
            ContextCost {
                full_copies: 0,
                copied_bytes: 0,
                address_space_locks: 0,
                page_table_walks: 0,
            }
        );
    }

    #[test]
    fn production_call_graph_has_one_context_owner_track() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
