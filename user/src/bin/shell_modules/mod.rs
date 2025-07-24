pub mod editor;
pub mod history;
pub mod jobs;
pub mod completion;
pub mod builtins;
pub mod executor;

// 重新导出主要类型
pub use editor::LineEditor;
pub use history::CommandHistory;
pub use jobs::JobManager;
pub use completion::TabCompletion;
pub use builtins::{handle_cd_command, handle_help_command, handle_fg_command, handle_bg_command};
pub use executor::{
    has_pipe, parse_pipeline, execute_command_with_jobs,
    execute_pipeline_with_jobs
};