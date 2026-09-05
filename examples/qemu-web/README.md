# QEMU web example

[`src/main.rs`](src/main.rs) creates two Debian VMs on one rootless socket LAN,
configures nginx over their typed SSH connections, forwards one HTTP port per VM, and
checks both host-to-guest and guest-to-guest reachability.

Install the prerequisites in the [QEMU guide](../../docs/qemu.md), start ifxd with this
directory as a trusted stack, then plan/apply/check/destroy. The initial apply downloads
and verifies the pinned image; later runs reuse it.
