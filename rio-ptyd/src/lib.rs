//! rio-ptyd: per-pane PTY session daemon.
//!
//! One small daemon owns one shell's PTY and a bounded replay buffer.
//! A client (rio, or the bundled CLI) attaches over the pane's unix
//! socket — or over any byte transport that reaches `attach --stdio`
//! on the daemon's machine (ssh, container exec) — and exchanges
//! framed messages: keystrokes in, terminal output back, resize, kill.
//!
//! Detach (or client death) leaves the shell running; a later attach
//! receives a synthesized terminal-state prefix plus the replay ring,
//! then the live stream. Only an explicit `Exited` frame means the
//! shell died — transport EOF means the link was lost.

pub mod modes;
pub mod protocol;
pub mod ring;
pub mod sockdir;

#[cfg(unix)]
pub mod attach_cli;
#[cfg(unix)]
pub mod daemon;
#[cfg(unix)]
pub mod osproc;
#[cfg(unix)]
pub mod pty;
