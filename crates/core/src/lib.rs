//! Keel shared code used by every crate and exe of the workspace: data paths, the append-only
//! log, process launching without console windows, single instance, working-set trimming, the
//! service's control pipe, the feature supervisor and the build-script icon helper.
pub mod appicon;
pub mod cmd;
pub mod log;
pub mod paths;
pub mod pipe;
pub mod single;
pub mod supervise;
pub mod traycfg;
pub mod trim;

#[cfg(feature = "build")]
pub mod build;
