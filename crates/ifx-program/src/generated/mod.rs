//! Typed Rust authoring API generated from resource schemas by `cargo run -p ifx-gen`.
//! Each provider module is re-exported from `crate::providers::<provider>`.

pub mod check;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "linode")]
pub mod linode;
pub mod memory;
#[cfg(feature = "qemu")]
pub mod qemu;
