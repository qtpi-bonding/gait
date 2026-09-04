#[cfg(all(feature = "dev-harness", feature = "rich-tui"))]
pub(crate) mod annotate;
#[cfg(feature = "rich-tui")]
pub mod builder;
#[cfg(feature = "dev-harness")]
pub(crate) mod capture;
pub mod permission;
#[cfg(feature = "rich-tui")]
pub mod renderer;
#[cfg(feature = "rich-tui")]
pub mod state;
#[cfg(feature = "rich-tui")]
pub mod timer;
#[cfg(feature = "rich-tui")]
pub mod view;

#[cfg(feature = "rich-tui")]
pub use builder::{InteractionOutcome, ReplBuilder};
#[cfg(feature = "rich-tui")]
pub use renderer::ReplRenderer;
#[cfg(feature = "rich-tui")]
pub use state::{ReplState, ViewSignal};
#[cfg(feature = "rich-tui")]
pub use view::ReplViewItem;
#[cfg(all(test, feature = "rich-tui"))]
mod harness;
