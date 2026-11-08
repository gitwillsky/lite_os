use syn::{Expr, ExprCall, ExprLit, ImplItem, ImplItemFn, Item, Lit, Path, Type, visit::Visit};

use super::SourceFile;

const TTY_PATH: &str = "system/linuxd/src/devices/tty/mod.rs";

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

/// PL011 有限预算读取只能显式确认 timeout，RX 电平由 FIFO 阈值撤销。
///
/// 先清 RX 会在宿主持续回填、预算耗尽且 FIFO 仍满时丢失后续中断；timeout 则必须在读取
/// 前确认，避免清掉读空之后新到达的通知。初始化清除不属于 handler 的确认契约。
pub(super) fn check_pl011_receive_ack(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources.iter().find(|source| source.relative == PL011_PATH) else {
        errors.push(format!(
            "missing PL011 contract production source: {PL011_PATH}"
        ));
        return;
    };
    let compact: String = source.text.chars().filter(|c| !c.is_whitespace()).collect();
    let handler = compact.find("fnhandle_interrupt(");
    let enable = compact.find("fnenable_receive(");
    let body = match (handler, enable) {
        (Some(h), Some(e)) if h < e => &compact[h..e],
        _ => "",
    };
    let clear = body.find("uart.write(INTERRUPT_CLEAR,RX_TIMEOUT_INTERRUPT);");
    let drain = body.find("uart.read(DATA)");
    let ordered = matches!((clear, drain), (Some(c), Some(d)) if c < d);
    if !ordered || body.matches("uart.write(INTERRUPT_CLEAR,").count() != 1 {
        errors.push(format!(
            "{PL011_PATH}: handle_interrupt must acknowledge only RX timeout before draining; bounded reads must preserve the RX FIFO level interrupt"
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
    fn pl011_ack_preserves_bounded_receive_progress() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        check_pl011_receive_ack(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");

        let source = sources
            .iter_mut()
            .find(|source| source.relative == PL011_PATH)
            .expect("PL011 source");
        // 同一生产实现验证两种真实丢通知回归：清除顺序与误清 RX 电平。
        let clear = "uart.write(INTERRUPT_CLEAR, RX_TIMEOUT_INTERRUPT);\n";
        let start = source.text.find(clear).expect("handler clear anchor");
        source.text.replace_range(start..start + clear.len(), "");
        let publish = "crate::serial::publish_received(";
        let at = source.text.find(publish).expect("publish anchor");
        source.text.insert_str(at, clear);
        let mut errors = Vec::new();
        check_pl011_receive_ack(&sources, &mut errors);
        assert_eq!(errors.len(), 1, "{errors:#?}");

        let source = sources
            .iter_mut()
            .find(|source| source.relative == PL011_PATH)
            .unwrap();
        source.text = source.text.replacen(clear, "", 1);
        let at = source
            .text
            .find("let mut bytes")
            .expect("receive budget anchor");
        source
            .text
            .insert_str(at, "uart.write(INTERRUPT_CLEAR, RX_INTERRUPT);\n");
        let mut errors = Vec::new();
        check_pl011_receive_ack(&sources, &mut errors);
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
