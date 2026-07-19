use quote::ToTokens;
use syn::{ImplItem, Item, ItemImpl};

use super::SourceFile;

const SOURCE: &str = "kernel/src/drivers/virtio_blk.rs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockCompletionContract {
    pub(super) used_length_reads: usize,
    pub(super) length_policy_calls: usize,
    pub(super) rejected_claims: usize,
    pub(super) recycled_tokens: usize,
    pub(super) accepted_claims: usize,
    pub(super) length_precedes_status: bool,
    pub(super) recycle_precedes_accept: bool,
}

impl BlockCompletionContract {
    fn complete(self) -> bool {
        self.used_length_reads == 1
            && self.length_policy_calls == 1
            && self.rejected_claims == 2
            && self.recycled_tokens == 1
            && self.accepted_claims == 1
            && self.length_precedes_status
            && self.recycle_precedes_accept
    }
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    match measure(sources) {
        Ok(contract) if contract.complete() => {}
        Ok(contract) => errors.push(format!(
            "{SOURCE}: each used completion must validate length before status/data access and accept or reject its claim exactly once; measured {contract:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(sources: &[SourceFile]) -> Result<BlockCompletionContract, String> {
    let source = sources
        .iter()
        .find(|source| source.relative == SOURCE)
        .ok_or_else(|| format!("{SOURCE}: missing production block adapter"))?;
    let method = source
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(ItemImpl { items, .. }) => Some(items),
            _ => None,
        })
        .flatten()
        .find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == "reclaim_completions" => Some(method),
            _ => None,
        })
        .ok_or_else(|| format!("{SOURCE}: missing reclaim_completions"))?;
    let text = method.block.to_token_stream().to_string();
    let length_policy = text.find("completion_length_is_valid");
    let status_read = text.find("decode_status");
    let recycle = text.find("recycle_used");
    let accept = text.find("accept_completion");
    Ok(BlockCompletionContract {
        used_length_reads: text.matches("completion . length ()").count(),
        length_policy_calls: text.matches("completion_length_is_valid").count(),
        rejected_claims: text.matches("reject_completion").count(),
        recycled_tokens: text.matches("recycle_used").count(),
        accepted_claims: text.matches("accept_completion").count(),
        length_precedes_status: matches!((length_policy, status_read), (Some(a), Some(b)) if a < b),
        recycle_precedes_accept: matches!((recycle, accept), (Some(a), Some(b)) if a < b),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn repository_validates_used_length_before_status_or_data() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let contract = super::measure(&sources).expect("block completion contract");
        assert!(contract.complete(), "measured {contract:?}");
    }
}
