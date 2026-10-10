// Owned bytes are test observations, never physical backing or residency owners.
pub(super) struct ContentSnapshot<'a> {
    pub(super) owner: &'a Arc<crate::memory::SwappablePage>,
    bytes: [u8; 4096],
}
impl ContentSnapshot<'_> {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl PrivateResident {
    pub(super) fn content_snapshot(&self) -> Option<ContentSnapshot<'_>> {
        if !self.is_installed() {
            return None;
        }
        let owner = self.mapping().expect("fixture user content").page();
        let mut bytes = [0; 4096];
        owner.pin().unwrap().read(0, &mut bytes);
        Some(ContentSnapshot { owner, bytes })
    }
}
