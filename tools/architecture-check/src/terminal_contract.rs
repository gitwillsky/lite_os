use syn::{
    Arm, Expr, ExprCall, ExprMacro, ImplItem, Item, Pat, Path, Token, Type,
    parse::{Parse, ParseStream},
    visit::Visit,
};

use super::SourceFile;

const CHARACTER_PATH: &str = "kernel/src/fs/file/character.rs";
const SEQUENTIAL_WRITE_PATH: &str = "kernel/src/syscall/fs/io/sequential/write.rs";

/// 校验 PTY user-visible readiness 与 syscall input batch 的 production dispatch。
pub(super) fn check_terminal_contract(sources: &[SourceFile], errors: &mut Vec<String>) {
    check_character_poll(sources, errors);
    check_character_write_chunk(sources, errors);
}

fn source<'a>(
    sources: &'a [SourceFile],
    path: &str,
    errors: &mut Vec<String>,
) -> Option<&'a SourceFile> {
    let source = sources.iter().find(|source| source.relative == path);
    if source.is_none() {
        errors.push(format!("missing PTY contract production source: {path}"));
    }
    source
}

fn path_ends_with(path: &Path, expected: &[&str]) -> bool {
    path.segments.len() >= expected.len()
        && path
            .segments
            .iter()
            .rev()
            .zip(expected.iter().rev())
            .all(|(actual, expected)| actual.ident == *expected)
}

fn type_ends_with(ty: &Type, expected: &str) -> bool {
    matches!(ty, Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == expected))
}

fn terminal_arm(arm: &Arm) -> bool {
    matches!(&arm.pat, Pat::Struct(pattern) if path_ends_with(&pattern.path, &["Self", "Terminal"]))
}

#[derive(Default)]
struct TerminalReadinessCalls {
    cooked: usize,
    raw_or_cooked: usize,
}

impl<'ast> Visit<'ast> for TerminalReadinessCalls {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if matches!(&*call.receiver, Expr::Path(receiver) if path_ends_with(&receiver.path, &["terminal"]))
        {
            match call.method.to_string().as_str() {
                "input_ready" => self.cooked += 1,
                "wait_ready" => self.raw_or_cooked += 1,
                _ => {}
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(function) = &*call.func {
            if path_ends_with(&function.path, &["Terminal", "input_ready"]) {
                self.cooked += 1;
            } else if path_ends_with(&function.path, &["Terminal", "wait_ready"]) {
                self.raw_or_cooked += 1;
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn check_character_poll(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = source(sources, CHARACTER_PATH, errors) else {
        return;
    };
    let mut methods = 0usize;
    let mut terminal_arms = 0usize;
    for item in &source.syntax.items {
        let Item::Impl(item_impl) = item else {
            continue;
        };
        if !type_ends_with(&item_impl.self_ty, "CharacterDevice") {
            continue;
        }
        for item in &item_impl.items {
            let ImplItem::Fn(method) = item else {
                continue;
            };
            if method.sig.ident != "poll_events" {
                continue;
            }
            methods += 1;
            let mut matches = Vec::new();
            PollMatchVisitor {
                matches: &mut matches,
            }
            .visit_block(&method.block);
            for expression in matches {
                for arm in &expression.arms {
                    if !terminal_arm(arm) {
                        continue;
                    }
                    terminal_arms += 1;
                    let mut calls = TerminalReadinessCalls::default();
                    calls.visit_expr(&arm.body);
                    if calls.cooked != 1 || calls.raw_or_cooked != 0 {
                        errors.push(format!(
                            "{CHARACTER_PATH}: CharacterDevice::Terminal poll must project exactly one `terminal.input_ready()` call and must not expose `wait_ready()` raw backlog"
                        ));
                    }
                }
            }
        }
    }
    if methods != 1 || terminal_arms != 1 {
        errors.push(format!(
            "{CHARACTER_PATH}: expected one CharacterDevice::poll_events method with one Terminal arm; found {methods} method(s) and {terminal_arms} arm(s)"
        ));
    }
}

struct PollMatchVisitor<'out, 'ast> {
    matches: &'out mut Vec<&'ast syn::ExprMatch>,
}

impl<'ast> Visit<'ast> for PollMatchVisitor<'_, 'ast> {
    fn visit_expr_match(&mut self, expression: &'ast syn::ExprMatch) {
        self.matches.push(expression);
        syn::visit::visit_expr_match(self, expression);
    }
}

struct MatchesArguments {
    expression: Expr,
    _comma: Token![,],
    pattern: Pat,
}

impl Parse for MatchesArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(Self {
            expression: input.parse()?,
            _comma: input.parse()?,
            pattern: input.call(Pat::parse_single)?,
        })
    }
}

fn is_pty_master_selection(expression: &Expr) -> bool {
    let Expr::Macro(ExprMacro { mac, .. }) = expression else {
        return false;
    };
    if !path_ends_with(&mac.path, &["matches"]) {
        return false;
    }
    let Ok(arguments) = syn::parse2::<MatchesArguments>(mac.tokens.clone()) else {
        return false;
    };
    matches!(
        (arguments.expression, arguments.pattern),
        (Expr::Path(expression), Pat::TupleStruct(pattern))
            if path_ends_with(&expression.path, &["device"])
                && path_ends_with(&pattern.path, &["CharacterDevice", "PtyMaster"])
                && pattern.elems.len() == 1
                && matches!(pattern.elems.first(), Some(Pat::Wild(_)))
    )
}

#[derive(Default)]
struct CharacterWriteChunkCalls {
    selections: Vec<Expr>,
}

impl<'ast> Visit<'ast> for CharacterWriteChunkCalls {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if matches!(&*call.func, Expr::Path(function) if path_ends_with(&function.path, &["character_write_chunk"]))
        {
            self.selections.extend(call.args.iter().nth(1).cloned());
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn check_character_write_chunk(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = source(sources, SEQUENTIAL_WRITE_PATH, errors) else {
        return;
    };
    let functions = source
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function) if function.sig.ident == "write_descriptor" => Some(function),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut calls = CharacterWriteChunkCalls::default();
    for function in &functions {
        calls.visit_block(&function.block);
    }
    if functions.len() != 1
        || calls.selections.len() != 1
        || !calls.selections.iter().all(is_pty_master_selection)
    {
        errors.push(format!(
            "{SEQUENTIAL_WRITE_PATH}: write_descriptor must select its sole character chunk with `matches!(device, CharacterDevice::PtyMaster(_))` so PTY master uses the 256-byte input budget"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(path: &str, text: &str) -> SourceFile {
        SourceFile {
            relative: path.to_owned(),
            owner: String::new(),
            text: text.to_owned(),
            lines: text.lines().map(str::to_owned).collect(),
            syntax: syn::parse_file(text).expect("terminal contract fixture must parse"),
            binary_crate: true,
        }
    }

    fn fixtures(poll_readiness: &str, chunk_selection: &str) -> Vec<SourceFile> {
        vec![
            parsed(
                CHARACTER_PATH,
                &format!(
                    r#"
                    enum CharacterDevice {{ Terminal {{ terminal: Terminal }}, Null }}
                    impl CharacterDevice {{
                        fn poll_events(&self, events: i16) -> i16 {{
                            match self {{
                                Self::Terminal {{ terminal }} => if terminal.{poll_readiness}() {{ events }} else {{ 0 }},
                                Self::Null => 0,
                            }}
                        }}
                    }}
                    "#
                ),
            ),
            parsed(
                SEQUENTIAL_WRITE_PATH,
                &format!(
                    r#"
                    fn write_descriptor(device: &CharacterDevice, remaining: usize) {{
                        character_write_chunk(remaining, {chunk_selection});
                    }}
                    "#
                ),
            ),
        ]
    }

    #[test]
    fn production_pty_dispatch_shape_is_accepted() {
        let mut errors = Vec::new();
        check_terminal_contract(
            &fixtures(
                "input_ready",
                "matches!(device, CharacterDevice::PtyMaster(_))",
            ),
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn raw_backlog_cannot_become_user_visible_poll_readiness() {
        let mut errors = Vec::new();
        check_terminal_contract(
            &fixtures(
                "wait_ready",
                "matches!(device, CharacterDevice::PtyMaster(_))",
            ),
            &mut errors,
        );
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn pty_master_cannot_fall_back_to_the_512_byte_character_chunk() {
        let mut errors = Vec::new();
        check_terminal_contract(&fixtures("input_ready", "false"), &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }
}
