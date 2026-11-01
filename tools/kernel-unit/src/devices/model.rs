pub(crate) use device_model::{
    PCM_BUFFER_FRAMES, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_PERIODS, PcmCapabilities,
    PcmCompletionObserver, PcmConfiguration, PcmFormat, PcmOutput, PcmOutputError,
};
pub(crate) const PCM_FRAME_BYTES: usize = PCM_PERIOD_BYTES / PCM_PERIOD_FRAMES;
#[path = "../../../../tools/source-view/devmgr/io_completion/io_completion/io_completion.rs"]
pub(crate) mod io_completion;
