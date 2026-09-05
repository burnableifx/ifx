//! Built-in providers. Each submodule registers one or more [`crate::provider::Handler`]s.

pub mod check;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "linode")]
pub mod linode;
pub mod memory;
#[cfg(feature = "qemu")]
pub mod qemu;

use crate::provider::Registry;

pub fn register_all(r: &mut Registry) {
    memory::register(r);
    check::register(r);
    #[cfg(feature = "host")]
    host::register(r);
    #[cfg(feature = "linode")]
    linode::register(r);
    #[cfg(feature = "qemu")]
    qemu::register(r);
}
