use syn::{Expr, ExprCall, ExprLit, ImplItem, ImplItemFn, Item, Lit, Path, Type, visit::Visit};

use super::SourceFile;

const TTY_PATH: &str = "kernel/src/fs/tty.rs";

/// 校验 TTY user-visible readiness 与 PTY master input batch 的 production 实现。
///
/// 1. `TerminalFile::poll` 只能投影一次 cooked `terminal.input_ready()`，不得暴露
///    `wait_ready()` 的 raw backlog；
/// 2. `PtyMasterFile::write` 必须以 `character_write_chunk(.., true)` 取 256-byte
///    line-discipline 预算，`TerminalFile::write` 以 `false` 取普通 512-byte chunk。
pub(super) fn check_terminal_contract(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources.iter().find(|source| source.relative == TTY_PATH) else {
        errors.push(format!(
            "missing TTY contract production source: {TTY_PATH}"
        ));
        return;
    };
    check_terminal_poll(source, errors);
    check_write_chunk(source, "PtyMasterFile", true, errors);
    check_write_chunk(source, "TerminalFile", false, errors);
}

const PL011_PATH: &str = "kernel/src/platform/qemu_virt/aarch64/pl011.rs";

/// PL011 RX hardirq 必须先清中断、再读空 FIFO。
///
/// QEMU 的 PL011 只在 FIFO 由空变为 1 字节时置位 RX 中断；先读空再清除会让“最后一次检查为空”与
/// “清除”之间到达的字节把刚置位的中断一并清掉，该字节留在 FIFO 且不再有“空→1”的跳变，输入流
/// 永久停滞（表现为 vi 等逐字节读取的程序在突发输入后无响应）。
pub(super) fn check_pl011_clears_before_draining(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources.iter().find(|source| source.relative == PL011_PATH) else {
        errors.push(format!(
            "missing PL011 contract production source: {PL011_PATH}"
        ));
        return;
    };
    let compact: String = source.text.chars().filter(|c| !c.is_whitespace()).collect();
    let clear = compact.find("uart.write(INTERRUPT_CLEAR,RX_INTERRUPT);");
    let drain = compact.find("uart.read(DATA)");
    let handler = compact.find("fnhandle_interrupt(");
    let enable = compact.find("fnenable_receive(");
    let ordered = matches!((handler, clear, drain), (Some(h), Some(c), Some(d)) if h < c && c < d);
    // `enable_receive` 的初始化清除不属于 handler，必须排在 handler 之后且不得被误当作 handler 的清除。
    let handler_clear_is_first = match (handler, clear, enable) {
        (Some(h), Some(c), Some(e)) => h < c && c < e,
        _ => false,
    };
    if !ordered || !handler_clear_is_first {
        errors.push(format!(
            "{PL011_PATH}: handle_interrupt must clear RX/timeout interrupts before draining the FIFO"
        ));
    }
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

/// `DeviceFile for <self_type>` 中名为 `method` 的全部实现。
fn device_file_methods<'a>(
    source: &'a SourceFile,
    self_type: &str,
    method: &str,
) -> Vec<&'a ImplItemFn> {
    source
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(item_impl)
                if type_ends_with(&item_impl.self_ty, self_type)
                    && item_impl
                        .trait_
                        .as_ref()
                        .is_some_and(|(_, path, _)| path_ends_with(path, &["DeviceFile"])) =>
            {
                Some(item_impl)
            }
            _ => None,
        })
        .flat_map(|item_impl| item_impl.items.iter())
        .filter_map(|item| match item {
            ImplItem::Fn(function) if function.sig.ident == method => Some(function),
            _ => None,
        })
        .collect()
}

/// receiver 是 `terminal` 变量或 `.terminal` 字段。
fn is_terminal(receiver: &Expr) -> bool {
    match receiver {
        Expr::Path(path) => path_ends_with(&path.path, &["terminal"]),
        Expr::Field(field) => {
            matches!(&field.member, syn::Member::Named(name) if name == "terminal")
        }
        _ => false,
    }
}

#[derive(Default)]
struct TerminalReadinessCalls {
    cooked: usize,
    raw_or_cooked: usize,
}

impl<'ast> Visit<'ast> for TerminalReadinessCalls {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if is_terminal(&call.receiver) {
            match call.method.to_string().as_str() {
                "input_ready" => self.cooked += 1,
                "wait_ready" => self.raw_or_cooked += 1,
                _ => {}
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

fn check_terminal_poll(source: &SourceFile, errors: &mut Vec<String>) {
    let methods = device_file_methods(source, "TerminalFile", "poll");
    if methods.len() != 1 {
        errors.push(format!(
            "{TTY_PATH}: expected one `DeviceFile::poll` for TerminalFile; found {}",
            methods.len()
        ));
        return;
    }
    let mut calls = TerminalReadinessCalls::default();
    calls.visit_block(&methods[0].block);
    if calls.cooked != 1 || calls.raw_or_cooked != 0 {
        errors.push(format!(
            "{TTY_PATH}: TerminalFile poll must project exactly one `terminal.input_ready()` call and must not expose `wait_ready()` raw backlog"
        ));
    }
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

fn is_bool(expression: &Expr, expected: bool) -> bool {
    matches!(expression, Expr::Lit(ExprLit { lit: Lit::Bool(value), .. }) if value.value == expected)
}

fn check_write_chunk(
    source: &SourceFile,
    self_type: &str,
    pty_master: bool,
    errors: &mut Vec<String>,
) {
    let methods = device_file_methods(source, self_type, "write");
    let mut calls = CharacterWriteChunkCalls::default();
    for method in &methods {
        calls.visit_block(&method.block);
    }
    if methods.len() != 1
        || calls.selections.len() != 1
        || !calls
            .selections
            .iter()
            .all(|selection| is_bool(selection, pty_master))
    {
        errors.push(format!(
            "{TTY_PATH}: {self_type}::write must select its sole chunk with `character_write_chunk(.., {pty_master})`; PTY master uses the 256-byte input budget"
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
        vec![parsed(
            TTY_PATH,
            &format!(
                r#"
                impl DeviceFile for TerminalFile {{
                    fn poll(&self, events: i16) -> i16 {{
                        if self.terminal.{poll_readiness}() {{ events }} else {{ 0 }}
                    }}
                    fn write(&self, input: &mut dyn UserInput) {{
                        character_write_chunk(input.remaining(), false);
                    }}
                }}
                impl DeviceFile for PtyMasterFile {{
                    fn write(&self, input: &mut dyn UserInput) {{
                        character_write_chunk(input.remaining(), {chunk_selection});
                    }}
                }}
                "#
            ),
        )]
    }

    #[test]
    fn pl011_interrupt_clear_cannot_move_after_the_drain() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        check_pl011_clears_before_draining(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");

        let source = sources
            .iter_mut()
            .find(|source| source.relative == PL011_PATH)
            .expect("PL011 source");
        // 把 handler 里的清除移到读空循环之后（即原先的有缺陷顺序）。
        let clear = "uart.write(INTERRUPT_CLEAR, RX_INTERRUPT);\n";
        let start = source.text.find(clear).expect("handler clear anchor");
        source.text.replace_range(start..start + clear.len(), "");
        let publish = "crate::drivers::console::publish_received(";
        let at = source.text.find(publish).expect("publish anchor");
        source.text.insert_str(at, clear);
        let mut errors = Vec::new();
        check_pl011_clears_before_draining(&sources, &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn production_pty_dispatch_shape_is_accepted() {
        let mut errors = Vec::new();
        check_terminal_contract(&fixtures("input_ready", "true"), &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn raw_backlog_cannot_become_user_visible_poll_readiness() {
        let mut errors = Vec::new();
        check_terminal_contract(&fixtures("wait_ready", "true"), &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn pty_master_cannot_fall_back_to_the_512_byte_character_chunk() {
        let mut errors = Vec::new();
        check_terminal_contract(&fixtures("input_ready", "false"), &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }
}
