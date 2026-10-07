#[path = "../../../kernel/src/devices/drivers/io_completion.rs"]
pub(crate) mod io_completion;

#[path = "../../../kernel/src/devices/drivers/audio_output.rs"]
pub(crate) mod audio_output;
pub(crate) use audio_output::{PCM_BUFFER_FRAMES, PCM_PERIOD_FRAMES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayMode {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
}
