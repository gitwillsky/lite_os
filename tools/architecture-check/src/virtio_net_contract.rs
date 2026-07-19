use quote::ToTokens;
use syn::{Expr, ImplItem, Item, Type, visit::Visit};

use super::SourceFile;

const VIRTIO_NET_SOURCE: &str = "kernel/src/drivers/virtio_net.rs";

/// 校验 production RX path 只能通过唯一 slot lifecycle owner 完成 completion。
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources
        .iter()
        .find(|source| source.relative == VIRTIO_NET_SOURCE)
    else {
        errors.push(format!("{VIRTIO_NET_SOURCE}: missing VirtIO-net adapter"));
        return;
    };

    let owner_fields = source.syntax.items.iter().filter_map(|item| match item {
        Item::Struct(item) if item.ident == "QueueState" => item
            .fields
            .iter()
            .find(|field| {
                field
                    .ident
                    .as_ref()
                    .is_some_and(|name| name == "receive_slots")
            })
            .map(|field| &field.ty),
        _ => None,
    });
    let owner_fields = owner_fields.collect::<Vec<_>>();
    if owner_fields.len() != 1 || !type_ends_with(owner_fields[0], "ReceiveSlots") {
        errors.push(format!(
            "{VIRTIO_NET_SOURCE}: QueueState must have exactly one ReceiveSlots RX lifecycle owner"
        ));
    }

    let receive_methods = source.syntax.items.iter().filter_map(|item| match item {
        Item::Impl(implementation)
            if implementation.trait_.as_ref().is_some_and(|(_, path, _)| {
                path.segments
                    .last()
                    .is_some_and(|segment| segment.ident == "NetworkDevice")
            }) =>
        {
            implementation.items.iter().find_map(|item| match item {
                ImplItem::Fn(method) if method.sig.ident == "receive" => Some(method),
                _ => None,
            })
        }
        _ => None,
    });
    let receive_methods = receive_methods.collect::<Vec<_>>();
    let mut calls = CompletionCalls::default();
    for method in &receive_methods {
        calls.visit_block(&method.block);
    }
    if receive_methods.len() != 1 || calls.owner_calls != 1 || calls.legacy_head_takes != 0 {
        errors.push(format!(
            "{VIRTIO_NET_SOURCE}: NetworkDevice::receive must delegate exactly once to `receive_slots.complete` and must not take head mappings directly"
        ));
    }
}

fn type_ends_with(ty: &Type, expected: &str) -> bool {
    matches!(ty, Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == expected))
}

#[derive(Default)]
struct CompletionCalls {
    owner_calls: usize,
    legacy_head_takes: usize,
}

impl<'ast> Visit<'ast> for CompletionCalls {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "complete"
            && matches!(&*call.receiver, Expr::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == "receive_slots"))
        {
            self.owner_calls += 1;
        }
        if call.method == "take"
            && matches!(&*call.receiver, Expr::Index(index) if matches!(&*index.expr, Expr::Field(field) if field.member.to_token_stream().to_string() == "receive_by_head"))
        {
            self.legacy_head_takes += 1;
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(receive_body: &str) -> SourceFile {
        let text = format!(
            "struct QueueState {{ receive_slots: ReceiveSlots<Box<[u8; 32]>, 32> }}\n\
             trait NetworkDevice {{ fn receive(&self); }}\n\
             impl NetworkDevice for Device {{ fn receive(&self) {{ {receive_body} }} }}"
        );
        SourceFile {
            relative: VIRTIO_NET_SOURCE.to_owned(),
            owner: String::new(),
            lines: text.lines().map(str::to_owned).collect(),
            syntax: syn::parse_file(&text).unwrap(),
            text,
            binary_crate: true,
        }
    }

    #[test]
    fn lifecycle_owner_dispatch_is_accepted() {
        let mut errors = Vec::new();
        check(
            &[source(
                "receive_slots.complete(queue, head, len, 12, frame);",
            )],
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn direct_head_mapping_take_is_rejected() {
        let mut errors = Vec::new();
        check(&[source("receive_by_head[head].take();")], &mut errors);
        assert_eq!(errors.len(), 1);
    }
}
