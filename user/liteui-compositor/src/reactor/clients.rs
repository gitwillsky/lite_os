use crate::{
    ffi::{self, PollFd},
    scene::{Damage, Scene},
    server::Server,
};

/// Collects every ready application OFD into one reactor-owned damage set.
///
/// A protocol failure closes only that identity's subtree. Returning an error
/// is reserved for compositor state failure, so one broken application cannot
/// terminate the display generation.
pub(super) fn collect(
    server: &mut Server,
    scene: &mut Scene,
    descriptors: &[PollFd],
) -> Result<Damage, ()> {
    let mut combined = Damage::EMPTY;
    for (index, descriptor) in descriptors.iter().enumerate() {
        let slot = Server::slot(index);
        let damage = if descriptor.returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
            server.disconnect(slot, scene)?
        } else {
            if descriptor.returned & ffi::POLLOUT != 0 && server.flush(slot).is_err() {
                combined.merge(server.disconnect(slot, scene)?);
                continue;
            }
            if descriptor.returned & ffi::POLLIN != 0 {
                match server.read(slot, scene) {
                    Ok(damage) => damage,
                    Err(()) => server.disconnect(slot, scene)?,
                }
            } else {
                Damage::EMPTY
            }
        };
        combined.merge(damage);
    }
    Ok(combined)
}
