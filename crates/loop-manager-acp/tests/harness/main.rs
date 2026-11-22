//! The contract suites of the `claude` profile (`claude`, `contract`) and of the `codex` profile (`codex`), as modules
//! of one test binary: one build and one link for all of them, while nextest still runs each test in a process of its
//! own.

mod claude;
mod codex;
mod contract;
