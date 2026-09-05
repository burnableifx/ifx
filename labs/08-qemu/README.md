# Lab 8: interconnected QEMU machines

This is IFX's primary end-to-end lab. [`src/main.rs`](src/main.rs) declares one verified
Debian image, two rootless socket links, and three VMs in a chain:

```text
host ──forward── router ──front── middle ──back── leaf
       direct             via router       via middle
```

Only the router exposes a permanent host SSH forward and HTTP service forward. The
middle and leaf bootstrap directly, remove that direct SSH forward, and are subsequently
managed through nested SSH hops. The leaf has no NIC on the front link. Each VM's
`connection` output feeds `host.Package`, `host.File`, and `host.Service`; TCP, HTTP,
and guest-to-guest executable checks prove each permitted link.

The image URL and SHA-256 are named constants and can be overridden with
`-c image_url=... -c image_sha256=...`. The verified base remains in the shared QEMU
cache; instance disks are disposable qcow2 overlays.

Prerequisites:

```console
$ command -v qemu-system-x86_64 qemu-img ssh-keygen
$ command -v cloud-localds || command -v genisoimage || command -v xorriso
```

Run the complete lifecycle:

```console
$ target/debug/ifx-labs run 8
```

The first run downloads one cloud image. IFX verifies it before use, boots every VM,
waits for SSH, seals transitional management, configures nginx through the final paths,
verifies host → router → middle → leaf traffic, destroys
the instances, checks that no QEMU pidfile remains, and retains only the verified image.
Without writable `/dev/kvm`, expect a substantially slower TCG boot.

See the [QEMU provider guide](../../docs/qemu.md) for volumes, lifecycle approvals,
adoption, and the complete typed surface. Continue with [Lab 9](../09-deployment-explorer/README.md).
