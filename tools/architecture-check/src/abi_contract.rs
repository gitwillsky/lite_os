use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use syn::{Arm, Expr, ExprLit, ExprMatch, File, Lit, Pat, PatIdent, Path as SynPath, visit::Visit};

fn syscall_constants(file: &File) -> BTreeSet<String> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Const(item) if item.ident.to_string().starts_with("SYSCALL_") => {
                Some(item.ident.to_string())
            }
            _ => None,
        })
        .collect()
}

pub(super) fn syscall_entries(file: &File) -> BTreeMap<String, usize> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Const(item) if item.ident.to_string().starts_with("SYSCALL_") => {
                let Expr::Lit(ExprLit {
                    lit: Lit::Int(number),
                    ..
                }) = item.expr.as_ref()
                else {
                    return None;
                };
                Some((
                    item.ident
                        .to_string()
                        .trim_start_matches("SYSCALL_")
                        .to_ascii_lowercase(),
                    number
                        .base10_parse()
                        .expect("syscall number must be an integer"),
                ))
            }
            _ => None,
        })
        .collect()
}

#[derive(Default)]
struct DispatchVisitor {
    constants: BTreeSet<String>,
    numeric_arms: Vec<usize>,
}

impl<'ast> Visit<'ast> for DispatchVisitor {
    fn visit_path(&mut self, path: &'ast SynPath) {
        for segment in &path.segments {
            let name = segment.ident.to_string();
            if name.starts_with("SYSCALL_") {
                self.constants.insert(name);
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_arm(&mut self, arm: &'ast Arm) {
        if matches!(
            &arm.pat,
            Pat::Lit(ExprLit {
                lit: Lit::Int(_),
                ..
            })
        ) {
            self.numeric_arms
                .push(arm.fat_arrow_token.spans[0].start().line);
        }
        syn::visit::visit_arm(self, arm);
    }

    fn visit_pat_ident(&mut self, pattern: &'ast PatIdent) {
        let name = pattern.ident.to_string();
        if name.starts_with("SYSCALL_") {
            self.constants.insert(name);
        }
        syn::visit::visit_pat_ident(self, pattern);
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        syn::visit::visit_expr_match(self, node);
    }
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    let abi_path = root.join("syscall-abi/src/lib.rs");
    let dispatch_path = root.join("kernel/src/syscall/mod.rs");
    let abi_text = fs::read_to_string(&abi_path).expect("syscall ABI source must exist");
    let dispatch_text = fs::read_to_string(&dispatch_path).expect("syscall dispatch must exist");
    let abi = syn::parse_file(&abi_text).expect("syscall ABI must parse");
    let dispatch = syn::parse_file(&dispatch_text).expect("syscall dispatch must parse");
    let constants = syscall_constants(&abi);
    let mut visitor = DispatchVisitor::default();
    visitor.visit_file(&dispatch);
    for name in constants.difference(&visitor.constants) {
        errors.push(format!("syscall ABI constant is not dispatched: {name}"));
    }
    for name in visitor.constants.difference(&constants) {
        errors.push(format!(
            "dispatcher uses a syscall absent from syscall-abi: {name}"
        ));
    }
    for line in visitor.numeric_arms {
        errors.push(format!(
            "kernel/src/syscall/mod.rs:{line}: raw numeric syscall dispatch is forbidden"
        ));
    }
}
