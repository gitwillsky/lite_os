//! Pure CSS paint-policy decisions shared by the recursive and fixed walks.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::style::Computed;

/// Whether one appearing `<input>` takes focus unprompted: only with an
/// explicit `autoFocus` prop and only while no field currently owns focus
/// (standard DOM autofocus semantics — appearing fields never steal it).
pub(super) fn takes_autofocus(props: &BTreeMap<String, Value>, focused: Option<u64>) -> bool {
    focused.is_none() && props.get("autoFocus").and_then(Value::as_bool) == Some(true)
}

/// Whether a node's subtree emits hit/scroll regions: `pointer-events: none`
/// on the node or any ancestor disables the whole subtree. LiteUI does not
/// implement the CSS `pointer-events: auto` re-enable on descendants
/// (documented subset limit).
pub(super) fn hits_enabled(ancestor: bool, computed: &Computed) -> bool {
    ancestor && computed.get("pointer-events") != Some("none")
}

pub(super) fn stacking_level(computed: &Computed, flex_item: bool) -> i32 {
    let positioned = matches!(
        computed.get("position"),
        Some("relative" | "absolute" | "fixed")
    );
    if !positioned && !flex_item {
        return 0;
    }
    computed
        .get("z-index")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .unwrap_or(0)
}
