//! Per-connection immutable display lists and compositor-owned GPU textures.

use std::{collections::HashMap, io};

use display_proto::{
    DisplayCommand, DisplayListCommit, MAX_CLIENT_TEXTURES, TextureCreate, TextureDestroy,
    TextureFormat, TexturePublish, TextureWrite,
};
use linux_uapi::drm::{VirglContext, VirglResource};

use super::{Owner, invalid};

struct StagingTexture {
    resource: VirglResource,
    format: TextureFormat,
    written: u32,
}

struct PublishedTexture {
    resource: VirglResource,
    format: TextureFormat,
}

enum TextureState {
    Staging(StagingTexture),
    Published(PublishedTexture),
}

struct PublishedList {
    revision: u64,
    configuration_serial: u64,
    payload: Vec<u8>,
}

/// Unique owner of client texture identities and validated paint snapshots.
pub(super) struct PaintStore {
    textures: HashMap<(Owner, u32), TextureState>,
    lists: HashMap<Owner, PublishedList>,
    /// Highest display-list revision decoded for each live connection owner.
    /// This watermark outlives a discarded list; without it, removing a
    /// superseded configure frame would let a later lower revision pass the
    /// monotonicity check.
    last_revisions: HashMap<Owner, u64>,
}

impl PaintStore {
    pub(super) fn new() -> Self {
        Self {
            textures: HashMap::new(),
            lists: HashMap::new(),
            last_revisions: HashMap::new(),
        }
    }

    /// Allocates compositor-owned storage without exposing DRM to the client.
    pub(super) fn create_texture(
        &mut self,
        graphics: &VirglContext,
        owner: Owner,
        create: TextureCreate,
    ) -> io::Result<()> {
        let key = (owner, create.texture_id);
        if self.textures.contains_key(&key)
            || self
                .textures
                .keys()
                .filter(|(candidate, _)| *candidate == owner)
                .count()
                >= MAX_CLIENT_TEXTURES
        {
            return Err(invalid("texture identity or quota invalid"));
        }
        let resource = match create.format {
            TextureFormat::Bgra8Premultiplied => {
                graphics.create_texture(create.size.width, create.size.height)?
            }
            TextureFormat::R8 => {
                graphics.create_mask_texture(create.size.width, create.size.height)?
            }
        };
        if resource.size() != create.byte_len as usize {
            return Err(invalid("texture storage does not match declaration"));
        }
        self.textures.insert(
            key,
            TextureState::Staging(StagingTexture {
                resource,
                format: create.format,
                written: 0,
            }),
        );
        Ok(())
    }

    /// Copies one ordered range. Sequential offsets make overlap and holes
    /// impossible without maintaining a second interval structure.
    pub(super) fn write_texture(
        &mut self,
        owner: Owner,
        write: TextureWrite<'_>,
    ) -> io::Result<()> {
        let TextureState::Staging(texture) = self
            .textures
            .get_mut(&(owner, write.texture_id))
            .ok_or_else(|| invalid("texture upload is not staging"))?
        else {
            return Err(invalid("published texture is immutable"));
        };
        let end = write
            .offset
            .checked_add(u32::try_from(write.bytes.len()).map_err(|_| invalid("texture chunk"))?)
            .ok_or_else(|| invalid("texture upload overflow"))?;
        if write.offset != texture.written || end as usize > texture.resource.size() {
            return Err(invalid("texture upload is not contiguous"));
        }
        texture.resource.bytes_mut()[write.offset as usize..end as usize]
            .copy_from_slice(write.bytes);
        texture.written = end;
        Ok(())
    }

    /// Uploads the complete storage and atomically changes staging to published.
    pub(super) fn publish_texture(
        &mut self,
        owner: Owner,
        publish: TexturePublish,
    ) -> io::Result<()> {
        let key = (owner, publish.texture_id);
        let state = self
            .textures
            .remove(&key)
            .ok_or_else(|| invalid("unknown texture"))?;
        let TextureState::Staging(texture) = state else {
            self.textures.insert(key, state);
            return Err(invalid("texture was already published"));
        };
        if texture.written as usize != texture.resource.size() {
            self.textures.insert(key, TextureState::Staging(texture));
            return Err(invalid("texture upload is incomplete"));
        }
        texture.resource.transfer_to_host(
            0,
            0,
            texture.resource.width(),
            texture.resource.height(),
        )?;
        texture.resource.wait()?;
        self.textures.insert(
            key,
            TextureState::Published(PublishedTexture {
                resource: texture.resource,
                format: texture.format,
            }),
        );
        Ok(())
    }

    /// Publishes one fully validated display list only after all texture
    /// references resolve to immutable resources of the required format.
    pub(super) fn commit_list(
        &mut self,
        owner: Owner,
        payload: Vec<u8>,
    ) -> io::Result<(u64, u64)> {
        let commit = DisplayListCommit::parse(&payload)
            .ok_or_else(|| invalid("invalid GPU display list"))?;
        if self
            .last_revisions
            .get(&owner)
            .is_some_and(|current| commit.revision <= *current)
        {
            return Err(invalid("display-list revision is not monotonic"));
        }
        for command in commit.commands() {
            let requirement = match command {
                DisplayCommand::Image { texture_id, .. } => {
                    Some((texture_id, TextureFormat::Bgra8Premultiplied))
                }
                DisplayCommand::GlyphRun { texture_id, .. } => {
                    Some((texture_id, TextureFormat::R8))
                }
                _ => None,
            };
            if let Some((texture_id, required)) = requirement {
                let Some(TextureState::Published(texture)) =
                    self.textures.get(&(owner, texture_id))
                else {
                    return Err(invalid("display list references unpublished texture"));
                };
                if texture.format != required {
                    return Err(invalid("display list texture format mismatch"));
                }
            }
        }
        let revision = commit.revision;
        let configuration_serial = commit.configuration_serial;
        self.last_revisions.insert(owner, revision);
        self.lists.insert(
            owner,
            PublishedList {
                revision,
                configuration_serial,
                payload,
            },
        );
        Ok((revision, configuration_serial))
    }

    /// Removes one terminally discarded display list while preserving its
    /// monotonic revision watermark.
    pub(super) fn discard_list(&mut self, owner: Owner, revision: u64) -> io::Result<()> {
        let current = self
            .lists
            .get(&owner)
            .ok_or_else(|| invalid("discarded display list disappeared"))?;
        if current.revision != revision {
            return Err(invalid("discarded display list revision changed"));
        }
        self.lists.remove(&owner);
        Ok(())
    }

    pub(super) fn destroy_texture(
        &mut self,
        owner: Owner,
        destroy: TextureDestroy,
    ) -> io::Result<()> {
        if self.list_references(owner, destroy.texture_id) {
            return Err(invalid("texture is referenced by the current display list"));
        }
        self.textures
            .remove(&(owner, destroy.texture_id))
            .ok_or_else(|| invalid("unknown texture"))?;
        Ok(())
    }

    pub(super) fn remove_owner(&mut self, owner: Owner) {
        self.lists.remove(&owner);
        self.last_revisions.remove(&owner);
        self.textures
            .retain(|(candidate, _), _| *candidate != owner);
    }

    fn list_references(&self, owner: Owner, texture_id: u32) -> bool {
        self.lists.get(&owner).is_some_and(|list| {
            DisplayListCommit::parse(&list.payload).is_some_and(|commit| {
                commit.commands().any(|command| {
                    matches!(command,
                        DisplayCommand::Image { texture_id: candidate, .. }
                        | DisplayCommand::GlyphRun { texture_id: candidate, .. }
                        if candidate == texture_id
                    )
                })
            })
        })
    }

    pub(super) fn texture(
        &self,
        owner: Owner,
        texture_id: u32,
    ) -> Option<(&VirglResource, TextureFormat)> {
        match self.textures.get(&(owner, texture_id))? {
            TextureState::Published(texture) => Some((&texture.resource, texture.format)),
            TextureState::Staging(_) => None,
        }
    }

    pub(super) fn list(&self, owner: Owner) -> Option<DisplayListCommit<'_>> {
        let list = self.lists.get(&owner)?;
        let commit = DisplayListCommit::parse(&list.payload)?;
        debug_assert_eq!(commit.configuration_serial, list.configuration_serial);
        Some(commit)
    }
}

#[cfg(test)]
mod tests {
    use display_proto::{DisplayListCommit, Rect, parse_frame};

    use super::{Owner, PaintStore};

    fn display_list(revision: u64) -> Vec<u8> {
        let mut bytes = [0; 128];
        let encoded = DisplayListCommit::encode(
            &mut bytes,
            revision,
            3,
            0,
            Rect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            },
            &[],
        )
        .expect("display list encodes");
        parse_frame(encoded)
            .expect("display-list frame parses")
            .payload()
            .to_vec()
    }

    #[test]
    fn discarded_list_keeps_its_revision_watermark() {
        let mut store = PaintStore::new();
        assert_eq!(
            store
                .commit_list(Owner::App(7), display_list(9))
                .expect("first list accepted"),
            (9, 3)
        );
        store
            .discard_list(Owner::App(7), 9)
            .expect("current list discarded");
        assert!(store.list(Owner::App(7)).is_none());
        assert!(
            store
                .commit_list(Owner::App(7), display_list(8))
                .is_err(),
            "discard must not reopen lower revisions"
        );
    }
}
