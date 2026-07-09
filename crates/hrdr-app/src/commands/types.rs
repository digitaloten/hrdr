use std::future::Future;
use std::pin::Pin;

/// A future producing a system line to display (empty = show nothing). The host
/// spawns it on its runtime and pipes the result to its transcript.
pub type LineFuture = Pin<Box<dyn Future<Output = String> + Send>>;

/// How an async result line should be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A plain system line.
    System,
    /// A unified diff (frontends with diff-aware rendering color it).
    Diff,
}

/// What `/expand` should do to tool output (parsed by the shared dispatcher;
/// applied by the frontend, which owns the expansion state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    /// Show every tool result in full.
    All,
    /// Collapse everything.
    Off,
    /// Toggle the most recent tool result.
    ToggleLast,
}
