//! Actual namespace lifetime accounting 与 proc sysctl permission owner 围栏。
use super::SourceFile;
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|source| source.relative == path)
        .expect("namespace quota source")
        .text
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let pid = source(sources, "kernel/src/namespace/pid.rs");
    let identity = source(sources, "kernel/src/namespace/pid_identity.rs");
    if !pid.contains("identity: Weak<PidIdentity>")
        || !pid.contains("association.identity.upgrade()")
        || !pid.contains("pub(crate) fn lookup_identity(")
        || !identity.contains(".publish(&self.identity)")
        || !identity.contains("namespace.publish_initial(&identity)")
    {
        errors.push("namespace-quota: PID namespace must publish and look up the same actual immutable identity without another registry or number-lifetime pin".into());
    }

    let user = source(sources, "kernel/src/namespace/user.rs");
    let uts = source(sources, "kernel/src/namespace/uts.rs");
    let quota = source(sources, "kernel/src/namespace/quota.rs");
    if !user.contains("quotas: UserQuotas")
        || !user.contains("_charge: Option<UserResourceCharge>")
        || !uts.contains("_charge: Option<UserResourceCharge>")
        || !user.contains("UserResource::User")
        || !uts.contains("UserResource::Uts")
        || !quota.contains("impl Drop for UserResourceCharge")
        || !quota.contains("namespace.quotas.decrement(*uid, self.resource)")
        || !quota.contains("charged_uid = namespace.owner()")
        || !quota.contains("self.chain[..self.charged]")
    {
        errors.push("namespace-quota: concrete namespace lifetime must own one creator/ancestor charge and rollback exact successful prefix".into());
    }
    let proc = source(sources, "kernel/src/fs/procfs.rs");
    let opened = source(sources, "kernel/src/fs/procfs/opened_operations.rs");
    let node = source(sources, "kernel/src/fs/procfs/node.rs");
    let identity = source(sources, "kernel/src/fs/permission.rs");
    let credentials = source(sources, "kernel/src/execution/task/model/credentials.rs");
    let proc_tokens: String = proc.split_whitespace().collect();
    let opened_tokens: String = opened.split_whitespace().collect();
    let node_tokens: String = node.split_whitespace().collect();
    if !proc_tokens.contains("source.current_namespace_authority()?.namespace")
        || !proc.contains("fn require_access(")
        || !proc.contains("identity.namespace_capable(")
        || !node_tokens.contains("Self::UserResourceLimit(_)|Self::UserMap(_,_)|Self::TimeOffsets(_)|Self::MountLimit|Self::InotifyLimit(_)|Self::PidLimit|Self::IpcLimit(_)|Self::CoreBufferLimit(_)|Self::OptmemLimit)=>{0o100644}")
        || !opened_tokens.contains("inode.user_namespace")
        || !proc.contains("let key = (node, scope)")
        || !identity.contains("effective_uid: self.capability_uid")
        || !credentials.contains("self.effective_uid,")
    {
        errors.push("namespace-quota: sysctl fd must pin lookup namespace and scoped permission use EUID ancestry independently of FSUID and stat mode".into());
    }
    let inode = source(sources, "kernel/src/fs/inode.rs");
    let open = source(sources, "kernel/src/syscall/fs/open.rs");
    let access = source(sources, "kernel/src/syscall/fs/access.rs");
    if !inode.contains("fn require_access(")
        || !open.contains("inode.require_access(&identity, requested)")
        || !access.contains(".require_access(&identity, mode as u8 | crate::fs::MAY_ACCESS)")
    {
        errors.push("namespace-quota: syscall permission must dispatch through actual inode policy without a sysctl-only ABI bypass".into());
    }
}
#[cfg(test)]
mod tests {
    fn reject(path: &str, old: &str, new: &str, expected: &str) {
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
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn rejects_pid_lookup_pinning_namespace_registration_instead_of_observer() {
        reject(
            "kernel/src/namespace/pid.rs",
            "identity: Weak<PidIdentity>",
            "identity: Arc<PidIdentity>",
            "PID namespace must publish and look up the same actual",
        );
    }
    #[test]
    fn rejects_pid_association_publication_without_actual_identity() {
        reject(
            "kernel/src/namespace/pid_identity.rs",
            ".publish(&self.identity)",
            ".publish()",
            "PID namespace must publish and look up the same actual",
        );
    }
    #[test]
    fn production_charges_actual_namespace_lifetimes() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_free_without_releasing_ancestor_charges() {
        reject(
            "kernel/src/namespace/quota.rs",
            "namespace.quotas.decrement(*uid, self.resource)",
            "let _ = (uid, namespace)",
            "exact successful prefix",
        );
    }
    #[test]
    fn rejects_fsuid_substitution_for_namespace_capability_owner() {
        reject(
            "kernel/src/fs/permission.rs",
            "effective_uid: self.capability_uid",
            "effective_uid: self.uid",
            "EUID ancestry",
        );
    }
    #[test]
    fn rejects_uts_bypassing_actual_resource_admission() {
        reject(
            "kernel/src/namespace/uts.rs",
            "UserResource::Uts",
            "UserResource::User",
            "concrete namespace lifetime",
        );
    }
}
