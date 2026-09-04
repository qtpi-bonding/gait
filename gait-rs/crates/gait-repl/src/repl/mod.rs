pub mod commands;
pub mod complete;
#[cfg(feature = "rich-tui")]
pub(crate) mod driver;
pub mod helper;
#[cfg(feature = "rich-tui")]
pub mod highlight;
pub mod hinter;
pub mod input;
pub mod prompt;
pub(crate) mod skill;
pub(crate) mod status;

pub use complete::GaitCompleter;
#[cfg(feature = "rich-tui")]
pub use driver::ReplSink;
#[cfg(feature = "rich-tui")]
pub(crate) use driver::{Control, ReplShared, ReplSource, Shared};
pub use helper::collect_at_refs;
#[cfg(feature = "rich-tui")]
pub use highlight::GaitHighlighter;
pub use hinter::GaitHinter;
pub use input::{bang_command_str, is_bang_command};
pub use prompt::GaitPrompt;

pub(crate) mod line_reader;
#[cfg(any(test, feature = "dev-harness"))]
pub(crate) use line_reader::ScriptedReader;
pub(crate) use line_reader::{LineReader, ReedlineReader};
