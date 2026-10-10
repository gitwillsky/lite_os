//! Calling credential capability owner、exec facts 与内容去权限事务围栏。
use super::SourceFile;
use quote::ToTokens;
use syn::Item;

fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a SourceFile {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("capability owner source")
}
fn field(source: &SourceFile, structure: &str, name: &str) -> Option<String> {
    source.syntax.items.iter().find_map(|item| {
        let Item::Struct(item) = item else {
            return None;
        };
        if item.ident != structure {
            return None;
        }
        item.fields
            .iter()
            .find(|field| field.ident.as_ref().is_some_and(|id| id == name))
            .map(|field| field.ty.to_token_stream().to_string())
    })
}
fn require(source: &SourceFile, facts: &[&str], message: &str, errors: &mut Vec<String>) {
    if facts.iter().any(|fact| !source.text.contains(fact)) {
        errors.push(format!("{}: {message}", source.relative));
    }
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let credentials = source(sources, "kernel/src/task/model/credentials.rs");
    if field(credentials, "Credentials", "capabilities").as_deref() != Some("CapabilityState") {
        errors.push("Credentials must own capabilities under the same credential lock".into());
    }
    for (path, structure, expected) in [
        (
            "kernel/src/task/model.rs",
            "Process",
            "LeaderCredentialsHandle",
        ),
        (
            "kernel/src/task/process_table.rs",
            "ProcessNode",
            "super :: model :: LeaderCredentialsHandle",
        ),
    ] {
        if field(source(sources, path), structure, "leader_credentials").as_deref()
            != Some(expected)
        {
            errors.push(format!(
                "{path}: leader/zombie must alias one retargetable credential handle"
            ));
        }
    }
    require(
        credentials,
        &[
            "target.capabilities.snapshot().sets.permitted & !caller_permitted == 0",
            "InspectionCredentials::Filesystem =>",
            "InspectionCredentials::Real =>",
            "caller.capabilities.sets.permitted",
            "target.capabilities.sets.permitted & !capabilities == 0",
            "self.effective_uid != next.effective_uid",
            "!self.access_identity(true).in_group(next.effective_gid)",
            "mode & 0o2010 == 0o2010",
            "outcome.downgrade_ids",
            "prepared.credentials",
        ],
        "credential authorization and prepared exec must retain commoncap subset/ID rules",
        errors,
    );
    let exec = source(sources, "kernel/src/task/model/process_exec.rs");
    let stages = [
        "prepare_exec_credentials(",
        "credentials.stack_credentials()",
        "self.cleanup_robust_list()",
        "self.commit_exec_credentials(credentials, loaded.readable())",
    ];
    let mut previous = 0;
    for stage in stages {
        match exec.text.find(stage) {
            Some(index) if index > previous => previous = index,
            _ => {
                errors.push("exec must build auxv from the same prepared credential commit".into())
            }
        }
    }
    let reap = source(sources, "kernel/src/task/process_table/wait_child/claim.rs");
    require(
        reap,
        &["retired.retire(&mut graph, record.pid)", "drop(retired);"],
        "zombie credential owner must retire outside the IRQ graph guard",
        errors,
    );
    let permission = source(sources, "kernel/src/storage/fs/permission.rs");
    require(
        permission,
        &[
            "fn write_kill_mask(",
            "Capability::FSETID",
            "fn may_remove_entry(",
            "Capability::FOWNER",
        ],
        "inode mode and sticky policy need the canonical capability predicates",
        errors,
    );
    if permission.text.contains("self.uid == 0") {
        errors.push("inode permissions cannot grant privilege solely from UID zero".into());
    }
    let regular = source(sources, "kernel/src/syscall/fs/io/regular.rs");
    if !matches!((regular.text.find("file.prepare_content_write()"),regular.text.find("cursor.stage_from_user_pagewise_into")),(Some(prepare),Some(copy)) if prepare<copy)
    {
        errors.push("regular write killpriv must commit before actual user-copy".into());
    }
    let sendfile = source(
        sources,
        "kernel/src/syscall/fs/io/sendfile/regular_transfer.rs",
    );
    if !matches!((sendfile.text.find("if read == 0"),sendfile.text.find("writer.prepare_content_write()"),sendfile.text.find("writer.write(")),(Some(read),Some(prepare),Some(write)) if read<prepare && prepare<write)
    {
        errors.push("sendfile must read a positive input prefix before output killpriv".into());
    }
    let tmp = source(sources, "kernel/src/storage/fs/tmpfs/content_mutation.rs");
    require(
        tmp,
        &[
            "self.state.lock_prepared(wait)",
            "Some(file.begin_write()?)",
        ],
        "tmpfs post-allocation mode commit needs prepared metadata waiting and shared write sequence",
        errors,
    );
    let mm = source(sources, "kernel/src/storage/fs/memory_file.rs");
    require(
        mm,
        &["modified: impl FnOnce()", "drop(state);", "modified();"],
        "successful memory allocation metadata publication cannot fail after page commit",
        errors,
    );
}

#[cfg(test)]
mod tests {
    fn reject(path: &str, old: &str, new: &str, message: &str) {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(old));
        source.text = source.text.replace(old, new);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(message)),
            "{errors:#?}"
        );
    }
    #[test]
    fn production_has_one_credential_and_mode_policy_owner() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
    #[test]
    fn rejects_copied_zombie_capabilities() {
        reject(
            "kernel/src/task/process_table.rs",
            "leader_credentials: super::model::LeaderCredentialsHandle,",
            "leader_credentials: CapabilityState,",
            "retargetable",
        );
    }
    #[test]
    fn rejects_scheduler_subset_bypass() {
        reject(
            "kernel/src/task/model/credentials.rs",
            "target.capabilities.snapshot().sets.permitted & !caller_permitted == 0",
            "true",
            "commoncap subset",
        );
    }
    #[test]
    fn rejects_uid_zero_dac_grant() {
        reject(
            "kernel/src/storage/fs/permission.rs",
            "self.capabilities & capability.mask() != 0",
            "self.uid == 0 || self.capabilities & capability.mask() != 0",
            "solely from UID zero",
        );
    }
    #[test]
    fn rejects_exec_recomputing_after_stack() {
        reject(
            "kernel/src/task/model/process_exec.rs",
            "self.commit_exec_credentials(credentials, loaded.readable());",
            "self.apply_exec_setid(0,0,0);",
            "same prepared",
        );
    }
    #[test]
    fn rejects_first_copy_fault_skipping_killpriv() {
        reject(
            "kernel/src/syscall/fs/io/regular.rs",
            "file.prepare_content_write()",
            "Ok::<(), FileSystemError>(())",
            "before actual user-copy",
        );
    }
    #[test]
    fn rejects_post_allocation_metadata_waiter_oom() {
        reject(
            "kernel/src/storage/fs/tmpfs/content_mutation.rs",
            "self.state.lock_prepared(wait)",
            "self.state.lock().unwrap()",
            "prepared metadata",
        );
    }
}
