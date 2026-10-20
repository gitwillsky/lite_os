use std::{env, process::ExitCode};

use quote::ToTokens;
use syn::File;

mod abi_contract;
mod address_space_lock_contract;
mod anonymous_inode_contract;
mod architecture_boundary_contract;
mod asid_contract;
mod capability_contract;
mod context_lifetime_contract;
mod deferred_context_contract;
mod dependency_contract;
mod documentation_contract;
mod epoll_cost;
#[cfg(test)]
mod epoll_cost_tests;
mod ext4_mapping_cost;
mod ext4_xattr_inode_contract;
mod fallible_collections_contract;
mod fallible_map_cost;
mod fd_publication_contract;
mod file_lifetime_contract;
mod filesystem_blocking_lock_contract;
mod fp_context_contract;
mod fs_credentials_contract;
mod fs_root_contract;
mod futex_contract;
mod getdents_cost;
mod high_half_contract;
mod huge_page_cost;
mod id_cost;
mod inet_options_contract;
mod inotify_contract;
mod interface_mtu_contract;
mod io_copy_cost;
mod ipc_contract;
mod ipv4_address_contract;
mod ipv4_routing_contract;
mod log_cost;
mod memory_copy_cost;
mod namespace_quota_contract;
mod netlink_namespace_contract;
mod netns_publication_contract;
mod network_failure_contract;
mod network_namespace_contract;
mod network_stack_cost;
mod packet_cost;
mod packet_namespace_contract;
mod page_cache_epoch_contract;
mod page_table_cost;
mod pidfd_contract;
mod port_namespace_cost;
mod proc_network_contract;
mod process_graph_cost;
mod pseudo_attributes_contract;
mod ready_contract;
mod receive_staging_cost;
mod repository_source;
mod rng_io_cost;
mod scheduler_cost;
mod send_staging_cost;
mod sighand_contract;
mod socket_buffer_contract;
mod socket_optmem_contract;
mod source_pattern_contract;
mod source_proof_contract;
mod source_size;
#[cfg(test)]
mod source_size_tests;
mod tcp_mss_contract;
mod tcp_timer_contract;
mod terminal_contract;
mod thread_mm_contract;
mod timer_transaction_cost;
mod translation_fence_contract;
mod udp_transport_contract;
mod unix_connect_contract;
mod user_context_cost;
mod userspace_contract;
mod vfs_opened_cost;
#[cfg(test)]
mod vfs_opened_cost_tests;
mod virtio_blk_completion_contract;
mod virtio_blk_cost;
mod virtio_dma_cost;
mod virtio_gpu_sequence_cost;
mod virtio_net_contract;
mod vma_hot_path;

use repository_source::{load_sources, repository_root, rust_files};

struct SourceFile {
    relative: String,
    owner: String,
    text: String,
    lines: Vec<String>,
    syntax: File,
    binary_crate: bool,
}

impl SourceFile {
    fn at(&self, line: usize) -> String {
        format!("{}:{line}", self.relative)
    }

    fn preceding_contains(&self, line: usize, distance: usize, marker: &str) -> bool {
        let start = line.saturating_sub(distance + 1);
        self.lines[start..line.saturating_sub(1).min(self.lines.len())]
            .iter()
            .any(|candidate| candidate.contains(marker))
    }
}

fn main() -> ExitCode {
    let write_interface = env::args().skip(1).any(|arg| arg == "--write-interface");
    let root = repository_root();
    let mut errors = Vec::new();
    let mut review_notices = Vec::new();
    let sources = match load_sources(&root) {
        Ok(sources) => sources,
        Err(error) => {
            eprintln!("architecture fence failed:\n- {error}");
            return ExitCode::FAILURE;
        }
    };

    dependency_contract::check(&root, &sources, &mut errors);
    source_size::check(&root, &sources, &mut errors, &mut review_notices);
    source_pattern_contract::check(&root, &sources, &mut errors);
    network_failure_contract::check(&sources, &mut errors);
    architecture_boundary_contract::check(&root, &sources, &mut errors);
    address_space_lock_contract::check(&sources, &mut errors);
    context_lifetime_contract::check(&sources, &mut errors);
    thread_mm_contract::check(&sources, &mut errors);
    sighand_contract::check(&sources, &mut errors);
    fs_root_contract::check(&sources, &mut errors);
    anonymous_inode_contract::check(&sources, &mut errors);
    fd_publication_contract::check(&sources, &mut errors);
    inet_options_contract::check(&sources, &mut errors);
    interface_mtu_contract::check(&root, &sources, &mut errors);
    ipv4_routing_contract::check(&root, &sources, &mut errors);
    ipv4_address_contract::check(&sources, &mut errors);
    packet_namespace_contract::check(&sources, &mut errors);
    tcp_mss_contract::check(&root, &sources, &mut errors);
    tcp_timer_contract::check(&root, &sources, &mut errors);
    socket_buffer_contract::check(&sources, &mut errors);
    socket_optmem_contract::check(&sources, &mut errors);
    network_namespace_contract::check(&sources, &mut errors);
    netns_publication_contract::check(&sources, &mut errors);
    netlink_namespace_contract::check(&sources, &mut errors);
    page_cache_epoch_contract::check(&sources, &mut errors);
    proc_network_contract::check(&sources, &mut errors);
    ext4_xattr_inode_contract::check(&sources, &mut errors);
    fs_credentials_contract::check(&sources, &mut errors);
    namespace_quota_contract::check(&sources, &mut errors);
    ipc_contract::check(&sources, &mut errors);
    futex_contract::check(&sources, &mut errors);
    pidfd_contract::check(&sources, &mut errors);
    udp_transport_contract::check(&root, &sources, &mut errors);
    inotify_contract::check(&sources, &mut errors);
    file_lifetime_contract::check(&sources, &mut errors);
    pseudo_attributes_contract::check(&sources, &mut errors);
    capability_contract::check(&sources, &mut errors);
    asid_contract::check(&sources, &mut errors);
    deferred_context_contract::check(&sources, &mut errors);
    epoll_cost::check(&root, &mut errors);
    ext4_mapping_cost::check(&root, &mut errors);
    fallible_map_cost::check(&sources, &mut errors);
    fp_context_contract::check(&root, &mut errors);
    filesystem_blocking_lock_contract::check(&sources, &mut errors);
    terminal_contract::check_terminal_contract(&sources, &mut errors);
    terminal_contract::check_pl011_clears_before_draining(&sources, &mut errors);
    timer_transaction_cost::check(&root, &mut errors);
    translation_fence_contract::check(&sources, &mut errors);
    unix_connect_contract::check(&sources, &mut errors);
    user_context_cost::check(&root, &sources, &mut errors);
    virtio_blk_cost::check(&sources, &mut errors);
    virtio_blk_completion_contract::check(&sources, &mut errors);
    virtio_dma_cost::check(&sources, &mut errors);
    virtio_gpu_sequence_cost::check(&sources, &mut errors);
    getdents_cost::check(&root, &sources, &mut errors);
    high_half_contract::check(&root, &mut errors);
    huge_page_cost::check(&root, &mut errors);
    io_copy_cost::check(&root, &mut errors);
    id_cost::check(&root, &mut errors);
    log_cost::check(&root, &mut errors);
    memory_copy_cost::check(&root, &mut errors);
    network_stack_cost::check(&root, &mut errors);
    packet_cost::check(&root, &mut errors);
    page_table_cost::check(&root, &mut errors);
    port_namespace_cost::check(&root, &mut errors);
    process_graph_cost::check(&sources, &mut errors);
    receive_staging_cost::check(&root, &mut errors);
    rng_io_cost::check(&root, &mut errors);
    scheduler_cost::check(&root, &mut errors);
    send_staging_cost::check(&root, &mut errors);
    vma_hot_path::check(&sources, &mut errors);
    virtio_net_contract::check(&sources, &mut errors);
    vfs_opened_cost::check(&root, &mut errors);
    ready_contract::check(&sources, &mut errors);
    fallible_collections_contract::check(&root, &sources, &mut errors);
    source_proof_contract::check(&sources, &mut errors);
    abi_contract::check(&root, &mut errors);
    userspace_contract::check(&root, &mut errors);
    documentation_contract::check(&root, &sources, write_interface, &mut errors);

    if !review_notices.is_empty() {
        eprintln!("architecture review required:");
        for notice in review_notices {
            eprintln!("- {notice}");
        }
    }

    if errors.is_empty() {
        println!("architecture fence passed");
        ExitCode::SUCCESS
    } else {
        eprintln!("architecture fence failed:");
        for error in errors {
            eprintln!("- {error}");
        }
        ExitCode::FAILURE
    }
}

fn normalized(tokens: impl ToTokens) -> String {
    tokens.into_token_stream().to_string()
}
