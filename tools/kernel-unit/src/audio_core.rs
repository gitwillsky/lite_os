//! audio-core 混音引擎的宿主测试入口；子模块层级与 `system/audio-core/src/` 一致。

#[path = "../../../system/audio-core/src/engine.rs"]
#[expect(
    dead_code,
    reason = "宿主测试经状态机接口驱动引擎；取原生数据面与输出占用的访问器只由 audio-core 的输出使用"
)]
pub(crate) mod engine;
#[path = "../../../system/audio-core/src/mix.rs"]
pub(crate) mod mix;

#[cfg(test)]
mod engine_tests;
