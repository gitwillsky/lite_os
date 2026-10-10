//! UDP ingress/queue/native policy 必须落在实际 transport owner，不允许外部 dequeue 后过滤。
use super::SourceFile;
use quote::ToTokens;
use std::path::Path;

fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn syntax(path: &Path) -> Result<syn::File, String> {
    let source =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    syn::parse_file(&source).map_err(|error| format!("{}: {error}", path.display()))
}
fn function(file: &syn::File, name: &str) -> Option<String> {
    file.items.iter().find_map(|item| match item {
        syn::Item::Fn(item) if item.sig.ident == name => {
            Some(normalized(&item.block.to_token_stream().to_string()))
        }
        syn::Item::Impl(item) => item.items.iter().find_map(|item| match item {
            syn::ImplItem::Fn(item) if item.sig.ident == name => {
                Some(normalized(&item.block.to_token_stream().to_string()))
            }
            _ => None,
        }),
        _ => None,
    })
}
fn ordered(file: &syn::File, name: &str, clauses: &[&str], errors: &mut Vec<String>) {
    let Some(body) = function(file, name) else {
        errors.push(format!("udp-owner: missing {name}"));
        return;
    };
    let mut cursor = 0;
    for clause in clauses {
        let normalized = normalized(clause);
        let Some(offset) = body[cursor..].find(&normalized) else {
            errors.push(format!("udp-owner: {name}: lost actual stage {clause}"));
            return;
        };
        cursor += offset + normalized.len();
    }
}
fn transport(socket: &syn::File, interface: &syn::File, errors: &mut Vec<String>) {
    ordered(
        socket,
        "accepts",
        &[
            "self.peer.is_some_and",
            "ip_repr.src_addr()",
            "repr.src_port",
            "self.endpoint.port == 0",
            "self.endpoint.port != repr.dst_port",
        ],
        errors,
    );
    for method in ["set_peer_endpoint", "set_local_endpoint"] {
        let Some(body) = function(socket, method) else {
            errors.push(format!("udp-owner: missing {method}"));
            continue;
        };
        if body.contains("reset(") || body.contains("close(") || body.contains("PacketBuffer::new")
        {
            errors.push(format!(
                "udp-owner: {method} must preserve actual queued datagrams"
            ));
        }
    }
    for method in ["send", "send_with"] {
        ordered(
            socket,
            method,
            &[
                "meta.local_port = Some(self.endpoint.port)",
                "meta.hop_limit = Some(self.hop_limit.unwrap_or(64))",
                "meta.local_address",
                "self.tx_buffer.enqueue",
            ],
            errors,
        );
    }
    ordered(
        socket,
        "dispatch",
        &[
            "packet_meta.local_port.expect",
            "packet_meta.hop_limit.expect",
        ],
        errors,
    );
    ordered(
        interface,
        "process_udp",
        &[
            "self.is_broadcast(&dst_addr)",
            "udp_socket.accepts",
            "udp_socket.process",
            "sockets.items()",
            "socket.accepts",
            "socket.admission_priority()",
            "max_by_key",
            "sockets.get_mut",
        ],
        errors,
    );
    if function(interface, "process_udp").is_none_or(|body| {
        body.contains("Vec::") || body.contains("Box::") || body.contains("collect::<")
    }) {
        errors.push("udp-owner: actual demux must not allocate a second recipient queue".into());
    }
}
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    match (
        syntax(&root.join("vendor/smoltcp/src/socket/udp.rs")),
        syntax(&root.join("vendor/smoltcp/src/iface/interface/udp.rs")),
    ) {
        (Ok(socket), Ok(interface)) => transport(&socket, &interface, errors),
        (socket, interface) => {
            errors.extend(socket.err());
            errors.extend(interface.err());
        }
    }
    let source = |path: &str| sources.iter().find(|source| source.relative == path);
    let Some(inet) = source("kernel/src/socket/inet.rs") else {
        errors.push("udp-owner: missing inet owner".into());
        return;
    };
    let endpoint = inet.syntax.items.iter().find_map(|item| match item {
        syn::Item::Struct(item) if item.ident == "EndpointState" => Some(item),
        _ => None,
    });
    if endpoint.is_none_or(|endpoint| {
        endpoint
            .fields
            .iter()
            .any(|field| field.ident.as_ref().is_some_and(|name| name == "peer"))
            || !endpoint
                .fields
                .iter()
                .any(|field| field.ident.as_ref().is_some_and(|name| name == "shutdown"))
            || !endpoint.fields.iter().any(|field| {
                field
                    .ident
                    .as_ref()
                    .is_some_and(|name| name == "user_bind_locks")
            })
    }) {
        errors.push(
            "udp-owner: EndpointState cannot mirror transport peer or omit native policy".into(),
        );
    }
    let Some(udp) = source("kernel/src/socket/inet/udp.rs") else {
        errors.push("udp-owner: missing UDP native owner".into());
        return;
    };
    ordered(
        &udp.syntax,
        "disconnect",
        &[
            "socket.set_peer_endpoint(None)",
            "locks & BIND_ADDRESS",
            "locks & BIND_PORT",
            "socket.set_local_endpoint(local)",
            "state.port_lease.take()",
            "network.udp_ports.release(lease)",
        ],
        errors,
    );
    if function(&udp.syntax, "disconnect").is_none_or(|body| {
        body.contains("close(") || body.contains("placeholder_socket(") || body.contains("remove(")
    }) {
        errors.push("udp-owner: AF_UNSPEC must retain the actual socket queues".into());
    }
    ordered(
        &udp.syntax,
        "shutdown",
        &[
            "peer_endpoint()",
            ".shutdown |= (how + 1) as u8",
            "if connected",
            "SocketError::NotConnected",
        ],
        errors,
    );
    ordered(
        &udp.syntax,
        "receive",
        &[
            "!socket.can_recv() && *read_closed",
            "(0, 0, None, None)",
            "socket.recv()",
        ],
        errors,
    );
    ordered(
        &udp.syntax,
        "bind",
        &["state.port_lease = Some(lease)", "state.user_bind_locks |="],
        errors,
    );
    if let Some(message) = source("kernel/src/syscall/socket/message.rs") {
        ordered(
            &message.syntax,
            "send_error",
            &[
                "SocketError::BrokenPipe",
                "MSG_NOSIGNAL",
                "SocketType::Stream",
                "send_thread_signal",
            ],
            errors,
        );
    } else {
        errors.push("udp-owner: missing native SIGPIPE boundary".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reject(before: &str, after: &str, diagnostic: &str) {
        let root = super::super::repository_root();
        let text = std::fs::read_to_string(root.join("vendor/smoltcp/src/socket/udp.rs")).unwrap();
        let interface = syntax(&root.join("vendor/smoltcp/src/iface/interface/udp.rs")).unwrap();
        let mut original = Vec::new();
        transport(&syn::parse_file(&text).unwrap(), &interface, &mut original);
        assert!(original.is_empty(), "{original:#?}");
        assert!(text.contains(before));
        let changed = syn::parse_file(&text.replacen(before, after, 1)).unwrap();
        let mut errors = Vec::new();
        transport(&changed, &interface, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(diagnostic)),
            "{errors:#?}"
        );
    }
    #[test]
    fn skipping_prequeue_peer_match_is_rejected() {
        reject(
            ".peer\n            .is_some_and",
            ".peer\n            .is_none_or",
            "accepts: lost actual stage",
        );
    }
    #[test]
    fn disconnect_cannot_clear_the_receive_queue() {
        reject(
            "self.endpoint = endpoint;",
            "self.rx_buffer.reset(); self.endpoint = endpoint;",
            "must preserve actual queued datagrams",
        );
    }
    #[test]
    fn queued_source_port_cannot_come_from_a_later_binding() {
        reject(
            "packet_meta\n                    .local_port",
            "Some(endpoint.port)",
            "dispatch: lost actual stage",
        );
    }
}
