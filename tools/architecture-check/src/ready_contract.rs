use std::collections::{BTreeMap, BTreeSet};

use syn::{
    Expr, ImplItemFn, ItemConst, ItemFn, ItemImpl, ItemMod, ItemStatic, ItemStruct, ItemUse, Macro,
    PatIdent, UseTree, spanned::Spanned, visit::Visit,
};

use super::{SourceFile, normalized};

#[cfg(test)]
mod ready_contract_tests;

const READY_TRANSITION_CONSUMER: &str = "consume_ready_projection_parts";
const READY_RETIREMENT_CONSUMER: &str = "consume_ready_projection_cpu";
const READY_MEMBERSHIP_PATH: &str = "kernel/src/task/processor/ready_membership.rs";
const PROCESSOR_PATH: &str = "kernel/src/task/processor.rs";
const READY_TRANSITION_COMMIT: &str = "commit_ready_transition";
const READY_RETIREMENT_COMMIT: &str = "commit_ready_retirement";
const READY_COMMIT_IMPORT: &str =
    "ready_membership :: { commit_ready_retirement , commit_ready_transition }";

#[derive(Default)]
struct ReadyProjectionConsumerVisitor {
    scope: Vec<String>,
    uses: Vec<(usize, String, Option<String>)>,
    commit_definitions: Vec<(usize, String, String)>,
    commit_shadows: Vec<(usize, String)>,
    commit_imports: Vec<(usize, String)>,
}

impl ReadyProjectionConsumerVisitor {
    fn visit_function(&mut self, name: String, body: &syn::Block) {
        self.scope.push(name);
        self.visit_block(body);
        self.scope.pop();
    }

    fn record(&mut self, line: usize, name: &str) {
        if matches!(name, READY_TRANSITION_CONSUMER | READY_RETIREMENT_CONSUMER) {
            let caller = (!self.scope.is_empty()).then(|| self.scope.join("::"));
            self.uses.push((line, name.to_owned(), caller));
        }
    }

    fn record_commit_definition(&mut self, line: usize, name: &str) {
        if matches!(name, READY_TRANSITION_COMMIT | READY_RETIREMENT_COMMIT) {
            let mut path = self.scope.clone();
            path.push(name.to_owned());
            self.commit_definitions
                .push((line, name.to_owned(), path.join("::")));
        }
    }

    fn record_commit_shadow(&mut self, line: usize, name: &str) {
        if matches!(name, READY_TRANSITION_COMMIT | READY_RETIREMENT_COMMIT) {
            self.commit_shadows.push((line, name.to_owned()));
        }
    }
}

impl<'ast> Visit<'ast> for ReadyProjectionConsumerVisitor {
    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let name = item.sig.ident.to_string();
        self.record_commit_definition(item.sig.ident.span().start().line, &name);
        self.visit_function(name, &item.block);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        let name = item.sig.ident.to_string();
        self.record_commit_definition(item.sig.ident.span().start().line, &name);
        self.visit_function(name, &item.block);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        self.scope
            .push(format!("impl {}", normalized(&item.self_ty)));
        syn::visit::visit_item_impl(self, item);
        self.scope.pop();
    }

    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if item.content.is_some() {
            self.scope.push(format!("mod {}", item.ident));
            syn::visit::visit_item_mod(self, item);
            self.scope.pop();
        }
    }

    fn visit_item_const(&mut self, item: &'ast ItemConst) {
        self.record_commit_shadow(item.ident.span().start().line, &item.ident.to_string());
        syn::visit::visit_item_const(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        self.record_commit_shadow(item.ident.span().start().line, &item.ident.to_string());
        syn::visit::visit_item_static(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        self.record_commit_shadow(item.ident.span().start().line, &item.ident.to_string());
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_pat_ident(&mut self, pattern: &'ast PatIdent) {
        self.record_commit_shadow(
            pattern.ident.span().start().line,
            &pattern.ident.to_string(),
        );
        syn::visit::visit_pat_ident(self, pattern);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        let import = normalized(&item.tree);
        if import.contains(READY_TRANSITION_COMMIT) || import.contains(READY_RETIREMENT_COMMIT) {
            self.commit_imports
                .push((item.use_token.span.start().line, import));
        }
        syn::visit::visit_item_use(self, item);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.record(call.method.span().start().line, &call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        for segment in &path.path.segments {
            self.record(
                segment.ident.span().start().line,
                &segment.ident.to_string(),
            );
        }
        syn::visit::visit_expr_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        match tree {
            UseTree::Path(path) => {
                self.record(path.ident.span().start().line, &path.ident.to_string())
            }
            UseTree::Name(name) => {
                self.record(name.ident.span().start().line, &name.ident.to_string())
            }
            UseTree::Rename(rename) => {
                self.record(rename.ident.span().start().line, &rename.ident.to_string())
            }
            UseTree::Glob(_) | UseTree::Group(_) => {}
        }
        syn::visit::visit_use_tree(self, tree);
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        let tokens = node.tokens.to_string();
        for name in [READY_TRANSITION_CONSUMER, READY_RETIREMENT_CONSUMER] {
            if tokens.contains(name) {
                self.record(node.path.span().start().line, name);
            }
        }
        syn::visit::visit_macro(self, node);
    }
}

fn method_call_on<'a>(
    expression: &'a Expr,
    receiver: &str,
    method: &str,
) -> Option<&'a syn::ExprMethodCall> {
    let Expr::MethodCall(call) = expression else {
        return None;
    };
    let Expr::Path(path) = call.receiver.as_ref() else {
        return None;
    };
    (call.method == method && path.path.is_ident(receiver)).then_some(call)
}

fn ready_commit_consumes_eagerly(function: &ItemFn) -> bool {
    match function.sig.ident.to_string().as_str() {
        READY_TRANSITION_COMMIT => function.block.stmts.first().is_some_and(|statement| {
            let syn::Stmt::Local(local) = statement else {
                return false;
            };
            local.init.as_ref().is_some_and(|initialization| {
                method_call_on(
                    &initialization.expr,
                    "transition",
                    READY_TRANSITION_CONSUMER,
                )
                .is_some()
            })
        }),
        READY_RETIREMENT_COMMIT => function.block.stmts.first().is_some_and(|statement| {
            let syn::Stmt::Expr(Expr::Call(call), _) = statement else {
                return false;
            };
            let Expr::Path(callee) = call.func.as_ref() else {
                return false;
            };
            callee.path.is_ident("retire")
                && call.args.len() == 1
                && method_call_on(
                    call.args.first().expect("checked one retirement argument"),
                    "retirement",
                    READY_RETIREMENT_CONSUMER,
                )
                .is_some()
        }),
        _ => true,
    }
}

fn check_projection_consumers(sources: &[SourceFile], errors: &mut Vec<String>) {
    let mut use_counts = BTreeMap::from([
        (READY_TRANSITION_CONSUMER, 0_usize),
        (READY_RETIREMENT_CONSUMER, 0_usize),
    ]);
    let mut definition_counts = BTreeMap::from([
        (READY_TRANSITION_COMMIT, 0_usize),
        (READY_RETIREMENT_COMMIT, 0_usize),
    ]);
    for source in sources {
        if source.relative == READY_MEMBERSHIP_PATH {
            for item in &source.syntax.items {
                let syn::Item::Fn(function) = item else {
                    continue;
                };
                let name = function.sig.ident.to_string();
                if matches!(
                    name.as_str(),
                    READY_TRANSITION_COMMIT | READY_RETIREMENT_COMMIT
                ) && !ready_commit_consumes_eagerly(function)
                {
                    errors.push(format!(
                        "{}: `{name}` must eagerly consume its parameter in the reviewed top-level statement shape",
                        source.at(function.sig.ident.span().start().line)
                    ));
                }
            }
        }
        let mut visitor = ReadyProjectionConsumerVisitor::default();
        visitor.visit_file(&source.syntax);
        for (line, consumer, caller) in visitor.uses {
            let required_caller = if consumer == READY_TRANSITION_CONSUMER {
                READY_TRANSITION_COMMIT
            } else {
                READY_RETIREMENT_COMMIT
            };
            if source.relative == READY_MEMBERSHIP_PATH
                && caller.as_deref() == Some(required_caller)
            {
                *use_counts
                    .get_mut(consumer.as_str())
                    .expect("Ready consumer count must exist") += 1;
            } else {
                errors.push(format!(
                    "{}: `{consumer}` may only be called by {READY_MEMBERSHIP_PATH}::{required_caller}",
                    source.at(line)
                ));
            }
        }
        for (line, name, path) in visitor.commit_definitions {
            if source.relative == READY_MEMBERSHIP_PATH && path == name {
                *definition_counts
                    .get_mut(name.as_str())
                    .expect("Ready commit definition count must exist") += 1;
            } else {
                errors.push(format!(
                    "{}: `{name}` must be the unique top-level definition in {READY_MEMBERSHIP_PATH}",
                    source.at(line)
                ));
            }
        }
        for (line, name) in visitor.commit_shadows {
            errors.push(format!(
                "{}: `{name}` may not shadow a Ready projection committer",
                source.at(line)
            ));
        }
        for (line, import) in visitor.commit_imports {
            if source.relative != PROCESSOR_PATH || import != READY_COMMIT_IMPORT {
                errors.push(format!(
                    "{}: Ready committers may only use the reviewed `{READY_COMMIT_IMPORT}` import",
                    source.at(line)
                ));
            }
        }
    }
    for (consumer, count) in use_counts {
        if count != 1 {
            errors.push(format!(
                "{READY_MEMBERSHIP_PATH}: expected exactly one `{consumer}` call, found {count}"
            ));
        }
    }
    for (commit, count) in definition_counts {
        if count != 1 {
            errors.push(format!(
                "{READY_MEMBERSHIP_PATH}: expected exactly one top-level `{commit}` definition, found {count}"
            ));
        }
    }
}

const READY_INGRESS: &str = "transition_to_ready";
const READY_RUNNING_EGRESS: &str = "transition_ready_to_running";
const READY_STOPPED_EGRESS: &str = "transition_ready_to_stopped";

type ReadyTransitionUse = (usize, usize, String);

#[derive(Default)]
struct ReadyTransitionShapeVisitor {
    uses: Vec<ReadyTransitionUse>,
    direct_commits: BTreeSet<ReadyTransitionUse>,
    malformed_commits: Vec<(usize, String)>,
    glob_imports: Vec<(usize, String)>,
}

impl ReadyTransitionShapeVisitor {
    fn is_transition(name: &str) -> bool {
        matches!(
            name,
            READY_INGRESS | READY_RUNNING_EGRESS | READY_STOPPED_EGRESS
        )
    }

    fn direct_transition(expression: &Expr) -> Option<ReadyTransitionUse> {
        match expression {
            Expr::MethodCall(call) => {
                let name = call.method.to_string();
                Self::is_transition(&name).then(|| {
                    let start = call.method.span().start();
                    (start.line, start.column, name)
                })
            }
            Expr::Call(call) => match call.func.as_ref() {
                Expr::Path(path) => path.path.segments.last().and_then(|segment| {
                    let name = segment.ident.to_string();
                    Self::is_transition(&name).then(|| {
                        let start = segment.ident.span().start();
                        (start.line, start.column, name)
                    })
                }),
                _ => None,
            },
            _ => None,
        }
    }

    fn record_ident(&mut self, identifier: &syn::Ident) {
        let name = identifier.to_string();
        if Self::is_transition(&name) {
            let start = identifier.span().start();
            self.uses.push((start.line, start.column, name));
        }
    }
}

impl<'ast> Visit<'ast> for ReadyTransitionShapeVisitor {
    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        let import = normalized(&item.tree);
        if import.contains('*') {
            self.glob_imports
                .push((item.use_token.span.start().line, import));
        }
        syn::visit::visit_item_use(self, item);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let commit = match call.func.as_ref() {
            Expr::Path(path) if path.path.segments.len() == 1 => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string()),
            _ => None,
        };
        if matches!(
            commit.as_deref(),
            Some(READY_TRANSITION_COMMIT | READY_RETIREMENT_COMMIT)
        ) {
            let transition = call.args.first().and_then(Self::direct_transition);
            let expected = if commit.as_deref() == Some(READY_TRANSITION_COMMIT) {
                READY_INGRESS
            } else {
                "Ready egress"
            };
            let matches_commit = transition.as_ref().is_some_and(|(_, _, name)| {
                if commit.as_deref() == Some(READY_TRANSITION_COMMIT) {
                    name == READY_INGRESS
                } else {
                    matches!(name.as_str(), READY_RUNNING_EGRESS | READY_STOPPED_EGRESS)
                }
            });
            if matches_commit {
                self.direct_commits
                    .insert(transition.expect("checked direct transition"));
            } else {
                self.malformed_commits
                    .push((call.span().start().line, expected.to_owned()));
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.record_ident(&call.method);
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        for segment in &path.path.segments {
            self.record_ident(&segment.ident);
        }
        syn::visit::visit_expr_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        match tree {
            UseTree::Path(path) => self.record_ident(&path.ident),
            UseTree::Name(name) => self.record_ident(&name.ident),
            UseTree::Rename(rename) => self.record_ident(&rename.ident),
            UseTree::Glob(_) | UseTree::Group(_) => {}
        }
        syn::visit::visit_use_tree(self, tree);
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        let tokens = node.tokens.to_string();
        for name in [READY_INGRESS, READY_RUNNING_EGRESS, READY_STOPPED_EGRESS] {
            if tokens.contains(name) {
                let start = node.path.span().start();
                self.uses.push((start.line, start.column, name.to_owned()));
            }
        }
        syn::visit::visit_macro(self, node);
    }
}

fn check_transition_shapes(sources: &[SourceFile], errors: &mut Vec<String>) {
    for source in sources {
        let mut visitor = ReadyTransitionShapeVisitor::default();
        visitor.visit_file(&source.syntax);
        if !visitor.uses.is_empty() {
            let reviewed_super_glob = matches!(
                source.relative.as_str(),
                "kernel/src/task/processor/job_control.rs"
                    | "kernel/src/task/processor/placement.rs"
                    | "kernel/src/task/processor/ready_queue.rs"
            );
            for (line, import) in &visitor.glob_imports {
                if !reviewed_super_glob || import != "super :: *" {
                    errors.push(format!(
                        "{}: Ready transitions may only inherit committers through the reviewed `super::*` processor import",
                        source.at(*line)
                    ));
                }
            }
        }
        for (line, expected) in visitor.malformed_commits {
            errors.push(format!(
                "{}: Ready commit must directly consume {expected}",
                source.at(line)
            ));
        }
        for transition in visitor.uses {
            if !visitor.direct_commits.contains(&transition) {
                let (line, _, name) = transition;
                errors.push(format!(
                    "{}: `{name}` must be a direct argument of its Ready projection commit",
                    source.at(line)
                ));
            }
        }
    }
}

/// 检查 Ready projection token 的唯一 consumer 与 transition 的直接 commit 形状。
///
/// `sources` 是已解析的 production source；所有 owner、调用形状和计数失败都追加到
/// `errors`。
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    check_projection_consumers(sources, errors);
    check_transition_shapes(sources, errors);
}
