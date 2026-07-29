//! Monitor control via DDC/CI (dxva2.dll).

#[path = "ddc.rs"]
mod backend;

pub use backend::*;
