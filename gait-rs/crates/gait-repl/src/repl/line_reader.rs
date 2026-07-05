#[cfg(test)]
use std::collections::VecDeque;
use std::io;

use reedline::{Prompt, Reedline, Signal};

/// Abstracts the REPL's line source so tests can drive it without a TTY.
pub(crate) trait LineReader {
    fn read_line(&mut self, prompt: &dyn Prompt) -> io::Result<Signal>;
}

/// Production reader: the configured `Reedline` editor.
pub(crate) struct ReedlineReader(pub Reedline);

impl LineReader for ReedlineReader {
    fn read_line(&mut self, prompt: &dyn Prompt) -> io::Result<Signal> {
        self.0.read_line(prompt)
    }
}

/// Test reader: returns queued signals, then `CtrlD` (EOF) when drained. Only
/// ever constructed from `#[cfg(test)]` code (line_reader/ui/harness/driver
/// command tests) — gated out of production builds so clippy's `dead_code`
/// lint doesn't flag it as unused there.
#[cfg(test)]
pub(crate) struct ScriptedReader {
    queue: VecDeque<Signal>,
}

#[cfg(test)]
impl ScriptedReader {
    pub(crate) fn new(lines: impl IntoIterator<Item = Signal>) -> Self {
        Self {
            queue: lines.into_iter().collect(),
        }
    }

    /// Convenience: each string becomes a `Signal::Success` line.
    pub(crate) fn lines(strs: &[&str]) -> Self {
        Self::new(strs.iter().map(|s| Signal::Success((*s).to_string())))
    }
}

#[cfg(test)]
impl LineReader for ScriptedReader {
    fn read_line(&mut self, _prompt: &dyn Prompt) -> io::Result<Signal> {
        Ok(self.queue.pop_front().unwrap_or(Signal::CtrlD))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_reader_yields_lines_then_eof() {
        let prompt = reedline::DefaultPrompt::default();
        let mut r = ScriptedReader::lines(&["hello", "/q"]);
        assert!(matches!(r.read_line(&prompt), Ok(Signal::Success(s)) if s == "hello"));
        assert!(matches!(r.read_line(&prompt), Ok(Signal::Success(s)) if s == "/q"));
        assert!(matches!(r.read_line(&prompt), Ok(Signal::CtrlD)));
    }
}
