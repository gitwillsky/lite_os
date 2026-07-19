use quote::ToTokens;
use syn::{ExprCall, ExprForLoop, ExprMethodCall, Item, ItemFn, visit::Visit};

use super::SourceFile;

const PROCESS_EXIT: &str = "kernel/src/task/task_manager/process_exit.rs";
const PARENT_DEATH: &str = "kernel/src/task/task_manager/parent_death.rs";
const PROCESS_GROUP: &str = "kernel/src/task/task_manager/process_group.rs";
const WAIT_CHILD: &str = "kernel/src/task/task_manager/wait_child.rs";
const THREAD_SELECTOR: &str = "kernel/src/task/task_manager/thread_selector.rs";

const PROCESSES: usize = 1024;
const THREADS_PER_PROCESS: usize = 8;
const STOPPED_GROUPS: usize = PROCESSES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcessGraphCost {
    pub(super) exit_process_visits: usize,
    pub(super) zero_pdeath_thread_visits: usize,
    pub(super) wait_process_visits: usize,
    pub(super) wait_graph_transactions: usize,
    pub(super) tid_lookup_process_visits: usize,
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    match measure(sources) {
        Ok(ProcessGraphCost {
            exit_process_visits: 0,
            zero_pdeath_thread_visits: 0,
            wait_process_visits: 0,
            wait_graph_transactions: 1,
            tid_lookup_process_visits: 0,
        }) => {}
        Ok(cost) => errors.push(format!(
            "ProcessGraph hot paths must use owner-maintained affected-set indexes and one wait transaction; fixed P={PROCESSES}, T={THREADS_PER_PROCESS}, K={STOPPED_GROUPS} measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
    match source(sources, WAIT_CHILD).and_then(|source| function(source, "find_waitable_child")) {
        Ok(function)
            if function
                .to_token_stream()
                .to_string()
                .contains("if selector > 0 { break ; }") => {}
        Ok(_) => errors.push(format!(
            "{WAIT_CHILD}: a consumed exact waitpid selector must return ECHILD, not fail-stop as a stale parent index"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(sources: &[SourceFile]) -> Result<ProcessGraphCost, String> {
    let exit = source(sources, PROCESS_EXIT)?;
    let pdeath = source(sources, PARENT_DEATH)?;
    let groups = source(sources, PROCESS_GROUP)?;
    let wait = source(sources, WAIT_CHILD)?;
    let selector = source(sources, THREAD_SELECTOR)?;

    let exit_candidate_scans = scans(exit, "mark_orphaned_stopped_groups")?;
    let new_orphan_scans = scans(exit, "mark_new_orphaned_stopped_groups")?;
    let direct_exit_scans = scans(exit, "prepare_current_exit")?;
    let orphan_test_scans = scans(groups, "process_group_is_orphaned")?;
    let pdeath_scans = scans(pdeath, "mark_parent_exit")?;

    // Each stopped-group orphan check and member marking holds the graph lock. This analytic
    // fixture is deterministic and intentionally reports a lower bound, not wall-clock noise.
    let exit_process_visits = (exit_candidate_scans.process_collections
        + new_orphan_scans.process_collections)
        * PROCESSES
        + orphan_test_scans.process_collections * STOPPED_GROUPS * 2 * PROCESSES
        + direct_exit_scans.process_collections * PROCESSES
        + pdeath_scans.process_collections * PROCESSES;
    let zero_pdeath_thread_visits =
        pdeath_scans.thread_collections * PROCESSES * THREADS_PER_PROCESS;

    let wait_find = scans(wait, "find_waitable_child")?;
    let wait_body = function(wait, "wait_child")?;
    let wait_calls = named_calls(wait_body, "find_waitable_child");
    let wait_process_visits = wait_find.process_collections * wait_calls * PROCESSES;

    let tid_lookup = scans(selector, "thread_by_tid")?;
    Ok(ProcessGraphCost {
        exit_process_visits,
        zero_pdeath_thread_visits,
        wait_process_visits,
        wait_graph_transactions: wait_calls,
        tid_lookup_process_visits: tid_lookup.process_collections * PROCESSES,
    })
}

fn source<'a>(sources: &'a [SourceFile], path: &str) -> Result<&'a SourceFile, String> {
    sources
        .iter()
        .find(|source| source.relative == path)
        .ok_or_else(|| format!("{path}: missing ProcessGraph production seam"))
}

fn function<'a>(source: &'a SourceFile, name: &str) -> Result<&'a ItemFn, String> {
    source
        .syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Fn(function) if function.sig.ident == name => Some(function),
            _ => None,
        })
        .ok_or_else(|| format!("{}: missing measured function {name}", source.relative))
}

#[derive(Default)]
struct CollectionScans {
    process_collections: usize,
    thread_collections: usize,
}

fn scans(source: &SourceFile, name: &str) -> Result<CollectionScans, String> {
    let function = function(source, name)?;
    let mut scans = CollectionScans::default();
    scans.visit_item_fn(function);
    Ok(scans)
}

impl Visit<'_> for CollectionScans {
    fn visit_expr_for_loop(&mut self, expression: &ExprForLoop) {
        let collection = expression.expr.to_token_stream().to_string();
        if collection.contains("graph . nodes") {
            self.process_collections += 1;
        }
        syn::visit::visit_expr_for_loop(self, expression);
    }

    fn visit_expr_method_call(&mut self, call: &ExprMethodCall) {
        let method = call.method.to_string();
        if matches!(
            method.as_str(),
            "iter" | "iter_after" | "values" | "for_each_mut"
        ) {
            let receiver = call.receiver.to_token_stream().to_string();
            if receiver.contains("graph . nodes") {
                self.process_collections += 1;
            } else if receiver == "threads" || receiver.ends_with("threads") {
                self.thread_collections += 1;
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

fn named_calls(function: &ItemFn, name: &str) -> usize {
    struct Calls<'a> {
        name: &'a str,
        count: usize,
    }
    impl Visit<'_> for Calls<'_> {
        fn visit_expr_call(&mut self, call: &ExprCall) {
            if call
                .func
                .to_token_stream()
                .to_string()
                .split_whitespace()
                .last()
                == Some(self.name)
            {
                self.count += 1;
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    let mut calls = Calls { name, count: 0 };
    calls.visit_item_fn(function);
    calls.count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Clone)]
    struct ModelProcess {
        parent: Option<usize>,
        creator: Option<usize>,
        group: usize,
        threads: BTreeSet<usize>,
        pdeath_enabled: bool,
        exited: bool,
        claimed: bool,
    }

    struct GraphModel {
        processes: BTreeMap<usize, ModelProcess>,
        children: BTreeMap<usize, BTreeSet<usize>>,
        creator_children: BTreeMap<usize, BTreeSet<usize>>,
        tid_owner: BTreeMap<usize, usize>,
        groups: BTreeMap<usize, BTreeSet<usize>>,
        affected_visits: usize,
        pdeath_thread_visits: usize,
        wait_transactions: usize,
    }

    impl GraphModel {
        fn new() -> Self {
            let mut model = Self {
                processes: BTreeMap::new(),
                children: BTreeMap::new(),
                creator_children: BTreeMap::new(),
                tid_owner: BTreeMap::new(),
                groups: BTreeMap::new(),
                affected_visits: 0,
                pdeath_thread_visits: 0,
                wait_transactions: 0,
            };
            model.publish(1, None, None, 1, 1);
            model
        }

        fn publish(
            &mut self,
            pid: usize,
            parent: Option<usize>,
            creator: Option<usize>,
            group: usize,
            thread_count: usize,
        ) {
            let threads = (0..thread_count)
                .map(|offset| pid * 16 + offset)
                .collect::<BTreeSet<_>>();
            for &tid in &threads {
                assert!(self.tid_owner.insert(tid, pid).is_none());
                self.creator_children.entry(tid).or_default();
            }
            if let Some(parent) = parent {
                self.children.entry(parent).or_default().insert(pid);
            }
            if let Some(creator) = creator {
                self.creator_children
                    .get_mut(&creator)
                    .expect("creator is live")
                    .insert(pid);
            }
            self.children.entry(pid).or_default();
            self.groups.entry(group).or_default().insert(pid);
            assert!(
                self.processes
                    .insert(
                        pid,
                        ModelProcess {
                            parent,
                            creator,
                            group,
                            threads,
                            pdeath_enabled: false,
                            exited: false,
                            claimed: false,
                        },
                    )
                    .is_none()
            );
        }

        fn exit_thread(&mut self, tid: usize) {
            let pid = self.tid_owner.remove(&tid).expect("live tid");
            let replacement = self.processes[&pid]
                .threads
                .iter()
                .copied()
                .find(|candidate| *candidate != tid)
                .unwrap_or(16);
            let created = self.creator_children.remove(&tid).expect("thread index");
            for child in created {
                let child_node = self.processes.get_mut(&child).expect("created child");
                if child_node.pdeath_enabled {
                    self.pdeath_thread_visits += child_node.threads.len();
                }
                child_node.creator = Some(replacement);
                self.creator_children
                    .get_mut(&replacement)
                    .expect("replacement creator")
                    .insert(child);
            }
            let process = self.processes.get_mut(&pid).expect("thread owner");
            process.threads.remove(&tid);
            if !process.threads.is_empty() {
                return;
            }
            process.exited = true;
            let children = core::mem::take(self.children.get_mut(&pid).expect("child index"));
            self.affected_visits += children.len();
            for child in children {
                self.processes.get_mut(&child).expect("child").parent = Some(1);
                self.children
                    .get_mut(&1)
                    .expect("init children")
                    .insert(child);
            }
        }

        fn wait_claim(&mut self, parent: usize) -> Option<usize> {
            self.wait_transactions += 1;
            for child in self.children.get(&parent).expect("parent child index") {
                self.affected_visits += 1;
                let node = self.processes.get_mut(child).expect("child");
                if node.exited && !node.claimed {
                    node.claimed = true;
                    return Some(*child);
                }
            }
            None
        }

        fn reap(&mut self, child: usize) {
            let node = self.processes.remove(&child).expect("claimed child");
            assert!(node.exited && node.claimed && node.threads.is_empty());
            self.children
                .get_mut(&node.parent.expect("child parent"))
                .expect("parent index")
                .remove(&child);
            self.creator_children
                .get_mut(&node.creator.expect("child creator"))
                .expect("creator index")
                .remove(&child);
            self.groups.get_mut(&node.group).unwrap().remove(&child);
            self.children.remove(&child);
        }

        fn assert_indexes(&self) {
            for (&pid, process) in &self.processes {
                if let Some(parent) = process.parent {
                    assert!(self.children[&parent].contains(&pid));
                }
                if let Some(creator) = process.creator {
                    assert!(self.creator_children[&creator].contains(&pid));
                }
                assert!(self.groups[&process.group].contains(&pid));
                for tid in &process.threads {
                    assert_eq!(self.tid_owner.get(tid), Some(&pid));
                }
            }
        }
    }

    #[test]
    fn repository_process_graph_cost_is_bounded() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn exit_storm_and_zero_pdeath_touch_only_affected_members() {
        let mut model = GraphModel::new();
        model.publish(2, Some(1), Some(16), 1, 1);
        let creator = 32;
        for pid in 3..1027 {
            model.publish(pid, Some(2), Some(creator), 1, 8);
        }
        model.exit_thread(creator);
        assert_eq!(model.affected_visits, 1024);
        assert_eq!(model.pdeath_thread_visits, 0);
        model.assert_indexes();
    }

    #[test]
    fn concurrent_wait_claim_is_single_transaction_and_exactly_once() {
        let mut model = GraphModel::new();
        model.publish(2, Some(1), Some(16), 1, 1);
        model.exit_thread(32);
        assert_eq!(model.wait_claim(1), Some(2));
        assert_eq!(model.wait_claim(1), None);
        assert_eq!(model.wait_transactions, 2);
        model.reap(2);
        model.assert_indexes();
    }

    #[test]
    fn randomized_publication_exit_and_reap_preserve_all_indexes() {
        let mut model = GraphModel::new();
        let mut seed = 0x5eed_cafe_u64;
        let mut next_pid = 2;
        for _ in 0..2_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let live_threads = model.tid_owner.keys().copied().collect::<Vec<_>>();
            if seed & 3 != 0 && next_pid < 700 {
                let creator = live_threads[(seed as usize) % live_threads.len()];
                let parent = model.tid_owner[&creator];
                model.publish(next_pid, Some(parent), Some(creator), 1, 1);
                next_pid += 1;
            } else if let Some(&tid) = live_threads
                .iter()
                .filter(|tid| **tid != 16)
                .nth((seed as usize) % live_threads.len().max(1))
            {
                model.exit_thread(tid);
            }
            if let Some(child) = model.wait_claim(1) {
                model.reap(child);
            }
            model.assert_indexes();
        }
    }
}
