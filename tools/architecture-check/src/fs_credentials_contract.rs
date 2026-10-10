//! Filesystem credentials 与 mm dumpability 的 publication/permission owner 围栏。
use super::SourceFile;
const CREDENTIALS: &str = "kernel/src/task/model/credentials.rs";
const EXEC: &str = "kernel/src/task/model/process_exec.rs";
const TRANSITION: &str = "kernel/src/task/process_table/resource_limit.rs";
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|source| source.relative == path)
        .expect("credential source")
        .text
}
fn ordered(text: &str, facts: &[&str]) -> bool {
    let mut position = 0;
    facts.iter().all(|fact| {
        let Some(next) = text[position..].find(fact) else {
            return false;
        };
        position += next + fact.len();
        true
    })
}
/// 检查首次 publication：后方保留正确路径不能掩盖提前发布的新增分支。
fn effects_before_publication(text: &str, start: &str, facts: &[&str]) -> bool {
    let Some(body) = text.split_once(start).map(|(_, body)| body) else {
        return false;
    };
    let Some((effects, _)) = body.split_once("core::mem::replace(&mut *live,") else {
        return false;
    };
    ordered(effects, facts)
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let credentials = source(sources, CREDENTIALS);
    let normalized_credentials: String = credentials.split_whitespace().collect();
    for fact in [
        "fs_uid: u32",
        "fs_gid: u32",
        "self.fs_uid",
        "self.fs_gid",
        "fsids: [self.fs_uid, self.fs_gid]",
        "caller.fsids[0]",
        "caller.fsids[1]",
        "mm.dumpable() || authority.capable(mm.user_namespace(), Capability::SYS_PTRACE)",
        "MmMembership::Retired { user_dumpable }",
        "authority.capable(&crate::namespace::initial_user(), Capability::SYS_PTRACE)",
    ] {
        if !normalized_credentials.contains(&fact.split_whitespace().collect::<String>()) {
            errors.push(format!(
                "fs-credentials: filesystem IDs and dumpable target authorization require {fact}"
            ));
        }
    }
    if !ordered(
        credentials,
        &[
            "fn commit_identity_change(",
            "let mm = self.mm_snapshot()",
            "with_credential_transition(self,",
            "let mut live =",
            "let mut next = live.clone()",
            "update(&mut next)?",
            "live.changes_security(&next)",
            "mm.set_dumpable(false)",
            "transition.clear_parent_death()",
            "core::mem::replace(&mut *live, next)",
            "drop(retired)",
        ],
    ) || !effects_before_publication(
        credentials,
        "fn commit_identity_change(",
        &[
            "update(&mut next)?",
            "mm.set_dumpable(false)",
            "transition.clear_parent_death()",
        ],
    ) || credentials.contains("clear_parent_death_signal")
    {
        errors.push("fs-credentials: candidate security effects must precede identity publication and retired values drop outside guards".into());
    }
    let transition = source(sources, TRANSITION);
    if !ordered(
        transition,
        &[
            "pub(in crate::task) fn with_credential_transition",
            "process_creation.lock()",
            "graph.lock()",
            "commit(&mut CredentialTransition",
        ],
    ) || !transition.contains("self.task.parent_death_signal(Some(0))")
        || !transition.contains("self.node.pdeath_enabled_threads -= 1")
    {
        errors.push("fs-credentials: typed transition must borrow canonical graph under creation before credentials and clear PDEATH without reentry".into());
    }
    if !ordered(
        credentials,
        &[
            "let credentials = target.process_credential_snapshot()",
            "target.process_memory_inspection()",
        ],
    ) || !ordered(
        credentials,
        &[
            "let credentials = target.credential_snapshot()",
            "target.memory_inspection()",
        ],
    ) {
        errors.push(
            "fs-credentials: target credentials must precede actual MM dumpability query".into(),
        );
    }
    let exec = source(sources, EXEC);
    if !ordered(
        exec,
        &[
            "AddressSpace::new(new_memory_set, self.user_namespace())?",
            "new_address_space.set_dumpable(false)",
            "self.replace_address_space(new_address_space)",
            "self.close_cloexec_files()",
            "self.commit_exec_credentials(credentials, loaded.readable())",
        ],
    ) || !ordered(
        credentials,
        &[
            "fn commit_exec_credentials(",
            "mm.set_dumpable(readable && prepared.provisional_dumpable)",
            "live.changes_security(&prepared.credentials)",
            "mm.set_dumpable(false)",
            "core::mem::replace(&mut *live, prepared.credentials)",
        ],
    ) || !effects_before_publication(
        credentials,
        "fn commit_exec_credentials(",
        &[
            "mm.set_dumpable(readable && prepared.provisional_dumpable)",
            "live.changes_security(&prepared.credentials)",
            "mm.set_dumpable(false)",
        ],
    ) || !credentials.contains("provisional_dumpable: self.real_uid == self.effective_uid")
    {
        errors.push("fs-credentials: exec private MM disabled, CLOEXEC then old-ID provisional/effects/credentials publication; no final-ID restart".into());
    }
    let mm = source(sources, "kernel/src/task/model/address_space.rs");
    if !mm.contains("dumpability: Dumpability")
        || !mm.contains("self.dumpability.user_dumpable()")
        || !mm.contains(".set_user_dumpable(self.dumpability.user_dumpable())")
    {
        errors.push(
            "fs-credentials: one MM-owned dumpability shared by VM and copied on fork".into(),
        );
    }
    if !mm.contains("user_namespace: Arc<crate::namespace::UserNamespace>")
        || !mm.contains("Self::new(MemorySet::try_new()?, self.user_namespace.clone())?")
        || !credentials.contains("identity_allowed\n            && match mm")
        || !credentials.contains(
            "mm.dumpable() || authority.capable(mm.user_namespace(), Capability::SYS_PTRACE)",
        )
    {
        errors.push("fs-credentials: independent nondumpable MM namespace authorization must survive fork and credential unshare".into());
    }
    let loader = source(sources, "kernel/src/task/loader.rs");
    if !loader.contains("let mut readable = identity.permits(metadata, 4)")
        || !loader.contains("readable &= identity.permits(interpreter_metadata, 4)")
    {
        errors.push(
            "fs-credentials: executable and actual PT_INTERP readability must feed dumpability"
                .into(),
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    fn reject(path: &str, old: &str, new: &str, expected: &str) {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(old));
        source.text = source.text.replace(old, new);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn accepts_single_security_publication_owner() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_using_current_credentials_namespace_for_nondumpable_mm() {
        reject(
            CREDENTIALS,
            "authority.capable(mm.user_namespace(), Capability::SYS_PTRACE)",
            "authority.capable(&target.user_namespace, Capability::SYS_PTRACE)",
            "independent nondumpable MM namespace",
        );
    }
    #[test]
    fn rejects_fork_rebinding_mm_namespace_to_child_credentials() {
        reject(
            "kernel/src/task/model/address_space.rs",
            "self.user_namespace.clone()",
            "crate::namespace::initial_user()",
            "independent nondumpable MM namespace",
        );
    }
    #[test]
    fn rejects_effective_ids_in_fscreds() {
        reject(
            CREDENTIALS,
            "caller.fsids[0]",
            "caller.uids[1]",
            "filesystem IDs",
        );
    }
    #[test]
    fn rejects_retired_mm_bypassing_nondumpable_authorization() {
        reject(
            CREDENTIALS,
            "MmMembership::Retired { user_dumpable }",
            "MmMembership::Retired { user_dumpable: _ }",
            "filesystem IDs",
        );
    }
    #[test]
    fn rejects_publication_before_dumpability_effects() {
        reject(
            CREDENTIALS,
            "update(&mut next)?",
            "core::mem::replace(&mut *live, next.clone()); update(&mut next)?",
            "precede identity publication",
        );
    }
    #[test]
    fn rejects_recursive_pdeath_graph_acquisition() {
        reject(
            CREDENTIALS,
            "transition.clear_parent_death()",
            "self.clear_parent_death_signal()",
            "precede identity publication",
        );
    }
    #[test]
    fn rejects_old_mm_pin_before_new_credential_snapshot() {
        reject(
            CREDENTIALS,
            "let credentials = target.credential_snapshot();",
            "let old_mm = target.memory_inspection();",
            "precede actual MM",
        );
    }
    #[test]
    fn rejects_exec_dumpability_using_final_ids() {
        reject(
            CREDENTIALS,
            "provisional_dumpable: self.real_uid == self.effective_uid",
            "provisional_dumpable: next.real_uid == next.effective_uid",
            "no final-ID restart",
        );
    }
    #[test]
    fn rejects_early_exec_publication_even_when_normal_commit_remains() {
        reject(
            CREDENTIALS,
            "mm.set_dumpable(readable && prepared.provisional_dumpable)",
            "core::mem::replace(&mut *live, prepared.credentials.clone()); mm.set_dumpable(readable && prepared.provisional_dumpable)",
            "no final-ID restart",
        );
    }
    #[test]
    fn rejects_visible_exec_mm_dumpable_before_cloexec() {
        reject(
            EXEC,
            "new_address_space.set_dumpable(false)",
            "new_address_space.set_dumpable(true)",
            "private MM disabled",
        );
    }
    #[test]
    fn rejects_fork_losing_dumpability() {
        reject(
            "kernel/src/task/model/address_space.rs",
            ".set_user_dumpable(self.dumpability.user_dumpable())",
            ".set_user_dumpable(true)",
            "copied on fork",
        );
    }
    #[test]
    fn rejects_unreadable_dynamic_interpreter_becoming_dumpable() {
        reject(
            "kernel/src/task/loader.rs",
            "readable &= identity.permits(interpreter_metadata, 4)",
            "readable = true",
            "PT_INTERP readability",
        );
    }
}
