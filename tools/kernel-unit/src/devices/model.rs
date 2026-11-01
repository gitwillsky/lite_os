#[path = "../../../../system/device-model/src/pcm_format.rs"]
pub(crate) mod pcm_format;
pub(crate) use pcm_format::{
    MAX_PCM_PERIOD_BYTES, PCM_RATES, PcmCapabilities, PcmConfiguration, PcmFormat,
};
#[path = "../../../../tools/source-view/devmgr/io_completion/io_completion/io_completion.rs"]
pub(crate) mod io_completion;

#[path = "../../../../system/device-model/src/audio_output.rs"]
pub(crate) mod audio_output;
pub(crate) use audio_output::{
    PCM_BUFFER_FRAMES, PCM_FRAME_BYTES, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_PERIODS,
    PcmCompletionObserver, PcmOutput, PcmOutputError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayMode {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
}
