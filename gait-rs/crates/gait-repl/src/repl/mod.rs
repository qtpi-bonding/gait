pub mod commands;
pub mod complete;
pub(crate) mod driver;
pub mod helper;
pub mod highlight;
pub mod hinter;
pub mod input;
pub mod prompt;

pub use complete::GaitCompleter;
pub use driver::ReplSink;
pub(crate) use driver::{Control, ReplShared, ReplSource, Shared};
pub use helper::collect_at_refs;
pub use highlight::GaitHighlighter;
pub use hinter::GaitHinter;
pub use input::{bang_command_str, is_bang_command};
pub use prompt::GaitPrompt;

pub(crate) mod line_reader;
#[cfg(test)]
pub(crate) use line_reader::ScriptedReader;
pub(crate) use line_reader::{LineReader, ReedlineReader};
