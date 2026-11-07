//! Keel's eGPU feature: adapters, NVIDIA limits and watchdog, eGPU
//! telemetry (NVML / ADLX / PDH), game detection and the GPU lock, AMD eGPU drivers and display
//! scaling, Legion Space pinning.
pub mod scan;
pub mod gpupref;
pub mod amd;
pub mod amdtune;
pub mod apps;
pub mod config;
pub mod coord;
pub mod display;
pub mod driver;
pub mod games;
pub mod gpu;
pub mod guard;
pub mod intscale;
pub mod legion;
pub mod nvdisp;
pub mod nvdrs;
pub mod nvidia;
pub mod nvsvc;
pub mod pdh;
pub mod stress;
pub mod telemetry;
pub mod trayicons;
pub mod util;
