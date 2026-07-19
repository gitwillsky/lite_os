use std::{env, process::ExitCode};

use quote::ToTokens;
use syn::File;

mod abi_contract;
mod address_space_lock_contract;
mod architecture_boundary_contract;
mod deferred_context_contract;
mod dependency_contract;
mod documentation_contract;
mod epoll_cost;
#[cfg(test)]
mod epoll_cost_tests;
mod ext2_mapping_cost;
mod fallible_collections_contract;
mod fallible_map_cost;
mod filesystem_blocking_lock_contract;
mod fp_context_contract;
mod getdents_cost;
mod high_half_contract;
mod huge_page_cost;
mod id_cost;
mod io_copy_cost;
mod log_cost;
mod memory_copy_cost;
mod network_stack_cost;
mod packet_cost;
mod page_table_cost;
mod port_namespace_cost;
mod process_graph_cost;
mod ready_contract;
mod receive_staging_cost;
mod repository_source;
mod rng_io_cost;
mod scheduler_cost;
mod send_staging_cost;
mod source_pattern_contract;
mod source_proof_contract;
mod source_size;
#[cfg(test)]
mod source_size_tests;
mod terminal_contract;
mod timer_transaction_cost;
mod translation_fence_contract;
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
    architecture_boundary_contract::check(&root, &sources, &mut errors);
    address_space_lock_contract::check(&sources, &mut errors);
    deferred_context_contract::check(&sources, &mut errors);
    epoll_cost::check(&root, &mut errors);
    ext2_mapping_cost::check(&root, &mut errors);
    fallible_map_cost::check(&sources, &mut errors);
    fp_context_contract::check(&root, &mut errors);
    filesystem_blocking_lock_contract::check(&sources, &mut errors);
    terminal_contract::check_terminal_contract(&sources, &mut errors);
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
