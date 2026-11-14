//! 从已绑定链路和已确认邻接计算路由，沿同一 RIB 来源提交。

use super::{Identity, Link, Protocol, RoutingSource, Status, Version, native_abi, spf};
use alloc::vec::Vec;

// 合并同一轮洪泛造成的 SPF。邻接失效的撤回不等合并窗口。
const SPF_DELAY: i64 = 50_000_000;
const RIB_RETRY: i64 = 100_000_000;

pub(super) struct Publication {
    roots: Vec<spf::Neighbor>,
    pub(super) areas: spf::Areas,
    pub(super) summaries: Vec<spf::Summary>,
    pub(super) at: i64,
}

impl Default for Publication {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            areas: spf::Areas::default(),
            summaries: Vec::new(),
            at: native_abi::DEADLINE_INFINITE,
        }
    }
}

pub(super) fn topology_changed(publication: &mut [Publication; 2], key: Identity, now: i64) {
    for version in [Version::V2, Version::V3] {
        let family = &mut publication[version as usize];
        if let Some(area) = key.topology_area(version)
            && family.areas.attached.binary_search(&area).is_ok()
        {
            family.at = family.at.min(now.saturating_add(SPF_DELAY));
        }
    }
}

impl Protocol {
    pub(super) fn publish(
        &mut self,
        links: &[Link],
        source: &RoutingSource,
        router: u32,
        forwarding: [bool; 2],
        now: i64,
    ) -> Result<(), Status> {
        for version in [Version::V2, Version::V3] {
            let family = &mut self.publication[version as usize];
            let same_family = || links.iter().filter(|link| link.version == version);
            let configured = same_family().next().map(|link| link.area);
            family.areas.multiple = same_family().any(|link| Some(link.area) != configured);
            let attached = || {
                let mut previous = None;
                same_family()
                    .filter(|link| link.active.is_some())
                    .filter_map(move |link| {
                        let area = u32::from(link.area);
                        (previous.replace(area) != Some(area)).then_some(area)
                    })
            };
            if !attached().eq(family.areas.attached.iter().copied()) {
                family.areas.attached.clear();
                family
                    .areas
                    .attached
                    .try_reserve(attached().count())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                family.areas.attached.extend(attached());
                family.at = now;
            }
            let border = forwarding[version as usize]
                && family.areas.attached.iter().any(|&area| area != 0)
                && same_family().any(|link| {
                    !link.passive && link.area.is_unspecified() && link.active.is_some()
                });
            if border != family.areas.border {
                family.areas.border = border;
                self.origination.invalidate(now);
                family.at = now;
            }
            let roots = || {
                links
                    .iter()
                    .filter(|link| link.version == version)
                    .filter_map(|link| link.root(now))
            };
            if !roots().eq(family.roots.iter().copied()) {
                family.roots.clear();
                family
                    .roots
                    .try_reserve(links.len())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                family.roots.extend(roots());
                family.at = now;
            }
            if now >= family.at {
                let selected = spf::select(
                    &self.database,
                    router,
                    version,
                    &family.roots,
                    &family.areas,
                    now,
                )?;
                match source.replace(selected.batch) {
                    Ok(()) => {
                        family.at = native_abi::DEADLINE_INFINITE;
                        if family.summaries != selected.summaries {
                            family.summaries = selected.summaries;
                            self.origination.invalidate(now);
                        }
                    }
                    Err(
                        native_abi::ERR_SHOULD_WAIT
                        | native_abi::ERR_NO_MEMORY
                        | native_abi::ERR_BAD_STATE,
                    ) => {
                        // 失去依据的旧摘要立即撤回。新路径待 RIB 确认后再发布。
                        let before = family.summaries.len();
                        family.summaries.retain(|old| {
                            selected
                                .summaries
                                .binary_search_by_key(&(old.area, old.prefix), |entry| {
                                    (entry.area, entry.prefix)
                                })
                                .is_ok_and(|position| selected.summaries[position] == *old)
                        });
                        if family.summaries.len() != before {
                            self.origination.invalidate(now);
                        }
                        family.at = now.saturating_add(RIB_RETRY);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}
