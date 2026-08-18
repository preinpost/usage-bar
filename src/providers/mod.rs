//! Provider collectors — one module per external usage source:
//!   data.rs    local tool logs (Claude Code, Codex, OpenCode)
//!   copilot.rs GitHub Copilot (OAuth + internal usage API)
//!   grok.rs    xAI Grok (CLI auth + billing proxy)
pub mod copilot;
pub mod data;
pub mod grok;
pub mod opencode_go;
