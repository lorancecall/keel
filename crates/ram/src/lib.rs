//! Keel memory feature: memory hogs and their launchers (`scan`),
//! app families, cleanup scripts, memory samples and leak report, kernel pool, on-demand families
//! (the service's `daemon`, switching, follow-the-app), duplicate tray-settings entries.
//! The keel CLI exposes it as `keel ram <command>`.
pub mod cleanup;
pub mod daemon;
pub mod deny;
pub mod dwmwatch;
pub mod events;
pub mod family;
pub mod fileinfo;
pub mod follow;
pub mod gpu;
pub mod inventory;
pub mod leak;
pub mod net;
pub mod observe;
pub mod ondemand;
pub mod paths;
pub mod pipe;
pub mod pool;
pub mod procs;
pub mod scan;
pub mod sources;
pub mod switch;
pub mod trayicons;
pub mod userside;

/// Launching other programs goes through the shared module (no console windows).
pub use keel_core::cmd;
