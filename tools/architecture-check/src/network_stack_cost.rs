use std::{fs, path::Path};

use quote::ToTokens;
use syn::{Expr, ExprMethodCall, ImplItem, Item, ItemFn, visit::Visit};

const INET: &str = "kernel/src/net/inet.rs";
const UDP: &str = "kernel/src/net/inet/udp.rs";
const RAW: &str = "kernel/src/net/inet/raw.rs";
const TCP: &str = "kernel/src/net/inet/tcp.rs";
const TCP_ACCEPT: &str = "kernel/src/net/inet/tcp/accept.rs";
const TCP_IO: &str = "kernel/src/net/inet/tcp/io.rs";
const TCP_LIFECYCLE: &str = "kernel/src/net/inet/tcp/lifecycle.rs";
const READINESS: &str = "kernel/src/net/inet/readiness.rs";
const OWNER: &str = "kernel/src/net/inet/protocol_owner.rs";
const ENDPOINTS: usize = 64;
const BYTES_PER_ENDPOINT: usize = 64 * 1024;
const SOCKET_CAPACITY: usize = 1024;
const CLEANUP_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DataPlaneCost {
    contended_endpoint_pairs: usize,
    payload_bytes_under_owner: usize,
    device_calls_under_spin_owner: usize,
    poll_conflict_spin_iterations: usize,
    poll_exclusive_owner_transactions: usize,
    pending_cleanup_capacity: usize,
    cleanup_budget: usize,
}

const fn legacy_cost() -> DataPlaneCost {
    DataPlaneCost {
        contended_endpoint_pairs: ENDPOINTS * (ENDPOINTS - 1) / 2,
        payload_bytes_under_owner: ENDPOINTS * BYTES_PER_ENDPOINT,
        device_calls_under_spin_owner: ENDPOINTS,
        poll_conflict_spin_iterations: ENDPOINTS,
        poll_exclusive_owner_transactions: 1,
        pending_cleanup_capacity: 0,
        cleanup_budget: 0,
    }
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(DataPlaneCost {
            contended_endpoint_pairs: 0,
            payload_bytes_under_owner: 0,
            device_calls_under_spin_owner: 0,
            poll_conflict_spin_iterations: 0,
            poll_exclusive_owner_transactions: 1,
            pending_cleanup_capacity: SOCKET_CAPACITY,
            cleanup_budget: CLEANUP_BUDGET,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{INET}: endpoint payload must run outside the unique owner and deferred poll must never spin; N={ENDPOINTS}, B={BYTES_PER_ENDPOINT}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<DataPlaneCost, String> {
    let readiness = parse(READINESS, &read(root, READINESS)?)?;
    if !readiness_walks_are_linear(&readiness)? {
        return Err(format!(
            "{READINESS}: full readiness snapshots must visit each endpoint AVL once without successor/relookup"
        ));
    }
    let inet_text = read(root, INET)?;
    let owner_text = read(root, OWNER)?;
    let inet = parse(INET, &inet_text)?;
    let owner = parse(OWNER, &owner_text)?;
    let payload_loans = [
        payload_loan(root, UDP, "send", "send_slice")?,
        payload_loan(root, UDP, "receive", "append")?,
        payload_loan(root, RAW, "send", "send_slice")?,
        payload_loan(root, RAW, "receive", "append")?,
        payload_loan(root, TCP_IO, "send", "send_slice")?,
        payload_loan(root, TCP_IO, "receive", "append")?,
    ];
    let endpoint_lifetime_proof = endpoint_lifetime_is_single_path(&inet)?;
    let payload_outside_owner =
        payload_loans.into_iter().all(|proof| proof) && endpoint_lifetime_proof;

    let try_poll = method(&owner, "try_poll")?;
    let mut poll_calls = CallFacts::default();
    poll_calls.visit_block(&try_poll.block);
    let task_mutex_owner = struct_field_type(&owner, "NetworkStackOwner", "state")?
        .contains("TaskMutex < NetworkStackState >");
    let poll_is_nonblocking = poll_calls.methods.contains(&"try_lock".to_owned())
        && !poll_calls.methods.contains(&"lock".to_owned())
        && poll_calls.loops == 0;
    let poll_dispatch = free_function(&inet, "dispatch_network_work")?;
    let mut dispatch_calls = CallFacts::default();
    dispatch_calls.visit_block(&poll_dispatch.block);
    let one_poll_owner = dispatch_calls
        .methods
        .iter()
        .filter(|name| *name == "try_poll")
        .count()
        == 1
        && dispatch_calls
            .methods
            .iter()
            .filter(|name| *name == "poll")
            .count()
            == 1;

    let payload_owner = method(&owner, "with_payload_loan")?;
    let order = payload_owner.block.to_token_stream().to_string();
    let release = order.find("drop (state)");
    let copy = order.find("operation (& mut payload)");
    let restore = order.find("lock_prepared (& mut wait)");
    let loan_releases_owner =
        matches!((release, copy, restore), (Some(a), Some(b), Some(c)) if a < b && b < c);

    let cleanup_type = struct_field_type(&owner, "NetworkStackOwner", "pending_cleanup")?;
    let drain_cleanup = method(&owner, "drain_cleanup")?;
    let mut cleanup_calls = CallFacts::default();
    cleanup_calls.visit_block(&drain_cleanup.block);
    let slot_proof = cleanup_slots_are_owned(
        &inet_text,
        &read(root, TCP)?,
        &read(root, TCP_ACCEPT)?,
        &read(root, TCP_IO)?,
        &read(root, TCP_LIFECYCLE)?,
    );
    let cleanup_capacity = if slot_proof
        && cleanup_type.contains("PendingCleanup < InetEndpoint , SOCKET_STORAGE_CAPACITY >")
    {
        const_usize(&inet, "SOCKET_STORAGE_CAPACITY")?
    } else {
        0
    };
    let cleanup_budget = if cleanup_calls.methods.contains(&"pop".to_owned())
        && cleanup_calls.methods.contains(&"has_pending".to_owned())
        && inet_text.contains("cleanup_backlog")
    {
        const_usize(&owner, "NETWORK_CLEANUP_BUDGET")?
    } else {
        0
    };
    let proven_payload = payload_outside_owner && loan_releases_owner;
    let proven_poll = task_mutex_owner && poll_is_nonblocking && one_poll_owner;
    Ok(DataPlaneCost {
        contended_endpoint_pairs: if proven_payload {
            0
        } else {
            legacy_cost().contended_endpoint_pairs
        },
        payload_bytes_under_owner: if proven_payload {
            0
        } else {
            legacy_cost().payload_bytes_under_owner
        },
        device_calls_under_spin_owner: if proven_poll {
            0
        } else {
            legacy_cost().device_calls_under_spin_owner
        },
        poll_conflict_spin_iterations: poll_calls.loops,
        // smoltcp 的 Interface/SocketSet 仍由一次必要的 exclusive transaction 推进；
        // 该数字不能虚报为零，区别在于 task waiter 睡眠且 deferred caller 只 try。
        poll_exclusive_owner_transactions: usize::from(one_poll_owner),
        pending_cleanup_capacity: cleanup_capacity,
        cleanup_budget,
    })
}

// 完整 poll 阶段不能套用有界 notification drain 的逐项 O(log N) 后继查询。
fn readiness_walks_are_linear(syntax: &syn::File) -> Result<bool, String> {
    for name in ["snapshot_readiness", "capture_readiness_transitions"] {
        let function = method(syntax, name)?;
        let mut facts = CallFacts::default();
        facts.visit_block(&function.block);
        let text = function.block.to_token_stream().to_string();
        if facts.loops != 0
            || facts
                .methods
                .iter()
                .filter(|name| *name == "for_each_mut")
                .count()
                != 3
            || facts
                .methods
                .iter()
                .any(|name| matches!(name.as_str(), "successor" | "iter_after" | "get_mut"))
            || facts.functions.contains(&"find_endpoint_after".to_owned())
            || ["endpoints", "raw_endpoints", "tcp_endpoints"]
                .into_iter()
                .any(|map| {
                    text.matches(&format!("self . {map} . for_each_mut"))
                        .count()
                        != 1
                })
        {
            return Ok(false);
        }
    }
    Ok(true)
}

// 固定 ring 容量必须由真实 slot admission 证明，不能仅比较两个数字或 facade Arc 数量。
fn cleanup_slots_are_owned(inet: &str, tcp: &str, accept: &str, io: &str, lifecycle: &str) -> bool {
    let inet: String = inet.split_whitespace().collect();
    let tcp: String = tcp.split_whitespace().collect();
    let accept: String = accept.split_whitespace().collect();
    let io: String = io.split_whitespace().collect();
    let lifecycle: String = lifecycle.split_whitespace().collect();
    // Closed 初始 endpoint 有一个真实 handle，listener 保留 first 并 clamp backlog>=1；accept 先加 replacement 再移交。
    let fresh = tcp
        .split("fncreate_endpoint(")
        .nth(1)
        .unwrap_or("")
        .split("fnset_no_delay(")
        .next()
        .unwrap_or("");
    let allocate = fresh.find("lethandle=add_socket(network)?;");
    let retain = fresh.find("handles.push(TcpHandle::new(handle));");
    let publish = fresh.find("network.tcp_endpoints.commit_vacant(");
    let transfer = accept.find(".handles.remove(position)");
    let replacement = accept.find("letreplacement=add_socket(&mutnetwork)?;");
    inet.contains("self.sockets.iter().count()>=SOCKET_STORAGE_CAPACITY")
        && matches!((allocate, retain, publish), (Some(a), Some(b), Some(c)) if a < b && b < c)
        && tcp.contains("letbacklog=backlog.clamp(1,TCP_BACKLOG_MAX);")
        && tcp.contains("letfirst=network.tcp_endpoints[&id].handles[0].handle;")
        && matches!((replacement, transfer), (Some(a), Some(b)) if a < b)
        && accept.contains("accepted_handles.push(entry);")
        && io.contains("if!state.orphaned||")
        && io.contains("tcp_endpoints.retain(|_,state|")
        && lifecycle.contains("elseif!retained{")
        && lifecycle.contains("*tcp=placeholder_socket();")
        && lifecycle.contains("tcp.set_send_mss(send_mss);retained=true;true")
        && lifecycle.contains("state.handles.retain_mut(|entry|")
        && lifecycle.contains("sockets.remove(entry.handle);false")
}

fn endpoint_lifetime_is_single_path(inet: &syn::File) -> Result<bool, String> {
    let send_wrapper = method(inet, "send_to")?;
    let receive_wrapper = method(inet, "receive")?;
    let drop_wrapper = method(inet, "drop")?;
    let close_wrapper = method(inet, "close_description")?;
    let operation = method(inet, "open_operation")?;
    let mut send_calls = CallFacts::default();
    send_calls.visit_block(&send_wrapper.block);
    let mut receive_calls = CallFacts::default();
    receive_calls.visit_block(&receive_wrapper.block);
    let mut drop_calls = CallFacts::default();
    drop_calls.visit_block(&drop_wrapper.block);
    let mut close_calls = CallFacts::default();
    close_calls.visit_block(&close_wrapper.block);
    let token = close_wrapper.block.to_token_stream().to_string();
    let open = operation.block.to_token_stream().to_string();
    Ok(send_calls.methods.contains(&"open_operation".to_owned())
        && receive_calls.methods.contains(&"open_operation".to_owned())
        && drop_calls.methods.contains(&"close_description".to_owned())
        && close_calls.methods.contains(&"cleanup_or_defer".to_owned())
        && token.contains("self . operation . lock () . take ()")
        && open.contains("operation . is_none ()")
        && struct_field_type(inet, "InetSocket", "operation")?
            == "Mutex < Option < InetEndpoint > >")
}

fn payload_loan(
    root: &Path,
    relative: &str,
    function: &str,
    payload_method: &str,
) -> Result<bool, String> {
    let text = read(root, relative)?;
    let syntax = parse(relative, &text)?;
    let function = free_function(&syntax, function)?;
    let mut loans = LoanCalls::default();
    loans.visit_block(&function.block);
    let [call] = loans.calls.as_slice() else {
        return Ok(false);
    };
    let arguments = call.args.iter().collect::<Vec<_>>();
    let [take, operation, restore] = arguments.as_slice() else {
        return Ok(false);
    };
    let (Expr::Closure(take), Expr::Closure(operation), Expr::Closure(restore)) =
        (take, operation, restore)
    else {
        return Ok(false);
    };
    let mut take_calls = CallFacts::default();
    take_calls.visit_expr(&take.body);
    let mut operation_calls = CallFacts::default();
    operation_calls.visit_expr(&operation.body);
    let mut restore_calls = CallFacts::default();
    restore_calls.visit_expr(&restore.body);
    Ok(take_calls.functions.contains(&"replace".to_owned())
        && operation_calls.methods.contains(&payload_method.to_owned())
        && !operation_calls
            .methods
            .iter()
            .any(|name| matches!(name.as_str(), "lock" | "try_lock" | "lock_prepared"))
        && restore_calls.functions.contains(&"replace".to_owned()))
}

#[derive(Default)]
struct LoanCalls<'ast> {
    calls: Vec<&'ast ExprMethodCall>,
}

impl<'ast> Visit<'ast> for LoanCalls<'ast> {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if call.method == "with_payload_loan" {
            self.calls.push(call);
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

#[derive(Default)]
struct CallFacts {
    methods: Vec<String>,
    functions: Vec<String>,
    loops: usize,
}

impl<'ast> Visit<'ast> for CallFacts {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        self.methods.push(call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = &*call.func
            && let Some(segment) = path.path.segments.last()
        {
            self.functions.push(segment.ident.to_string());
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_loop(&mut self, expression: &'ast syn::ExprLoop) {
        self.loops += 1;
        syn::visit::visit_expr_loop(self, expression);
    }

    fn visit_expr_while(&mut self, expression: &'ast syn::ExprWhile) {
        self.loops += 1;
        syn::visit::visit_expr_while(self, expression);
    }
}

fn free_function<'a>(syntax: &'a syn::File, name: &str) -> Result<&'a ItemFn, String> {
    syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Fn(function) if function.sig.ident == name => Some(function),
            _ => None,
        })
        .ok_or_else(|| format!("function {name} not found"))
}

fn method<'a>(syntax: &'a syn::File, name: &str) -> Result<&'a syn::ImplItemFn, String> {
    syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(implementation) => Some(implementation),
            _ => None,
        })
        .flat_map(|implementation| &implementation.items)
        .find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == name => Some(method),
            _ => None,
        })
        .ok_or_else(|| format!("method {name} not found"))
}

fn struct_field_type(syntax: &syn::File, owner: &str, field: &str) -> Result<String, String> {
    syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == owner => item
                .fields
                .iter()
                .find(|candidate| candidate.ident.as_ref().is_some_and(|ident| ident == field))
                .map(|field| field.ty.to_token_stream().to_string()),
            _ => None,
        })
        .ok_or_else(|| format!("field {owner}.{field} not found"))
}

fn const_usize(syntax: &syn::File, name: &str) -> Result<usize, String> {
    syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Const(item) if item.ident == name => match &*item.expr {
                Expr::Lit(expression) => match &expression.lit {
                    syn::Lit::Int(value) => value.base10_parse().ok(),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .ok_or_else(|| format!("usize const {name} not found"))
}

fn parse(relative: &str, text: &str) -> Result<syn::File, String> {
    syn::parse_file(text).map_err(|error| format!("{relative}: {error}"))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_global_mutex_cost_is_quadratic_and_spinning() {
        assert_eq!(legacy_cost().contended_endpoint_pairs, 2016);
        assert_eq!(legacy_cost().payload_bytes_under_owner, 4 * 1024 * 1024);
        assert_eq!(legacy_cost().poll_conflict_spin_iterations, ENDPOINTS);
    }

    #[test]
    fn endpoint_loans_and_nonblocking_poll_have_explicit_costs() {
        let root = super::super::repository_root();
        assert_eq!(
            measure(&root).expect("network ownership cost must be measurable"),
            DataPlaneCost {
                contended_endpoint_pairs: 0,
                payload_bytes_under_owner: 0,
                device_calls_under_spin_owner: 0,
                poll_conflict_spin_iterations: 0,
                poll_exclusive_owner_transactions: 1,
                pending_cleanup_capacity: SOCKET_CAPACITY,
                cleanup_budget: CLEANUP_BUDGET,
            }
        );
    }

    #[test]
    fn full_readiness_walk_rejects_per_endpoint_tree_relookup() {
        let root = super::super::repository_root();
        let text = read(&root, READINESS).unwrap();
        assert!(readiness_walks_are_linear(&parse(READINESS, &text).unwrap()).unwrap());
        let mutated = text.replacen(
            "self.tcp_endpoints.for_each_mut(|_, state| {",
            "let _ = self.tcp_endpoints.successor(&0); self.tcp_endpoints.for_each_mut(|_, state| {",
            1,
        );
        assert_ne!(mutated, text);
        assert!(!readiness_walks_are_linear(&parse(READINESS, &mutated).unwrap()).unwrap());
        let missing = text.replacen(
            "self.raw_endpoints.for_each_mut",
            "self.endpoints.for_each_mut",
            1,
        );
        assert!(!readiness_walks_are_linear(&parse(READINESS, &missing).unwrap()).unwrap());
    }

    #[test]
    fn final_drop_is_separate_from_live_endpoint_operations() {
        let root = super::super::repository_root();
        let text = read(&root, INET).unwrap();
        let inet = parse(INET, &text).unwrap();
        assert!(endpoint_lifetime_is_single_path(&inet).unwrap());
    }
    #[test]
    fn cleanup_capacity_rejects_fresh_metadata_without_an_actual_socket_slot() {
        let root = super::super::repository_root();
        let inet = read(&root, INET).unwrap();
        let tcp = read(&root, TCP).unwrap();
        let accept = read(&root, TCP_ACCEPT).unwrap();
        let io = read(&root, TCP_IO).unwrap();
        let lifecycle = read(&root, TCP_LIFECYCLE).unwrap();
        assert!(cleanup_slots_are_owned(
            &inet, &tcp, &accept, &io, &lifecycle
        ));
        assert!(!cleanup_slots_are_owned(
            &inet,
            &tcp.replace("handles.push(TcpHandle::new(handle));", "let _ = handle;"),
            &accept,
            &io,
            &lifecycle
        ));
        assert!(!cleanup_slots_are_owned(
            &inet,
            &tcp,
            &accept,
            &io.replace("!state.orphaned", "false"),
            &lifecycle
        ));
        assert!(!cleanup_slots_are_owned(
            &inet,
            &tcp,
            &accept,
            &io,
            &lifecycle.replace("*tcp = placeholder_socket();", "tcp.abort();")
        ));
    }
}
