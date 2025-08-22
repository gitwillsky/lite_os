pub mod builtins;
pub mod completion;
pub mod editor;
pub mod executor;
pub mod history;
pub mod jobs;

// 重新导出主要类型
pub use builtins::{handle_bg_command, handle_cd_command, handle_fg_command, handle_help_command};
pub use completion::TabCompletion;
pub use editor::LineEditor;
pub use executor::{
    execute_command_with_jobs, execute_pipeline_with_jobs, has_pipe, parse_pipeline,
};
pub use history::CommandHistory;
pub use jobs::JobManager;
