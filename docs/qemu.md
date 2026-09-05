# QEMU provider

QEMU is IFX's primary free and repeatable full-system target. The provider owns verified
images, durable volumes, rootless networks, qcow2 overlays, cloud-init seeds, VM
processes, QMP lifecycle, SSH connection outputs, and adoption.

It is intentionally a declarative subset rather than an arbitrary QEMU/QMP escape
hatch. Every exposed field must be observable, diffable, and safe to recover.

## Prerequisites

- `qemu-system-x86_64`, `qemu-img`, and `ssh-keygen`;
- one NoCloud seed builder: `cloud-localds`, `genisoimage`, or `xorriso`.

Writable `/dev/kvm` enables KVM and the host CPU. Otherwise the provider warns and uses
multi-threaded TCG.

## Resources

| Resource | Managed surface |
|---|---|
| `qemu.image` | HTTP(S) download, SHA-256 verification, persistent cache, observed path/size |
| `qemu.volume` | blank qcow2/raw disks, overlays/copies, preallocation, growth, crash recovery |
| `qemu.network` | rootless socket buses or isolated user-mode endpoints, deterministic /24 subnet |
| `qemu.instance` | x86 machine/CPU/accelerator, vCPU/RAM, system disk, typed data disks, NoCloud, NICs, host forwards, serial log, QMP, process identity |

## Images and networks

```rust
let image = stack
    .qemu_image("debian")
    .url(image_url)
    .sha256(image_sha256)
    .dir(cache_dir)
    .add()?;

let lan = stack
    .qemu_network("lan")
    .name("development")
    .mode(qemu::NetworkMode::Socket)
    .dir(instance_dir)
    .cidr("10.42.7.0/24")
    .add()?;
```

Image cache names include the expected digest. Destroy retains verified bases while VM
system disks remain disposable overlays. A socket network is a rootless guest Ethernet
bus. Management exposure and steady-state egress are separate instance policies.

## Durable volumes

```rust
let data = stack
    .qemu_volume("data")
    .dir(instance_dir)
    .size_gb(20)
    .source(image.path())
    .source_mode(qemu::VolumeSourceMode::Overlay)
    .preallocation(qemu::VolumePreallocation::Metadata)
    .add()?;
```

Growth uses `qemu-img resize` while inactive. Shrink plans replacement; IFX never uses
`--shrink`. Destructive volume replacement waits for an exact revision-bound
`volume-data-loss` approval. Replacement journals retain rollback data until successor
state commits; interrupted work enters `recovery_wait` and can recover forward or roll
back.

Important outputs are `path`, `format`, `virtual_size_bytes`,
`allocated_size_bytes`, and `backing_path`.

## Instances and SSH composition

```rust
let vm = stack
    .qemu_instance("node-a")
    .dir(instance_dir)
    .image(image.path())
    .memory_mb(1024)
    .cpus(2)
    .machine("q35")
    .acceleration("auto")
    .disk_gb(10)
    .management(qemu::InstanceManagement::Direct)
    .egress(qemu::InstanceEgress::UserNat)
    .networks([lan.endpoint()])
    .port_forwards([("8080", 80)])
    .volumes([qemu::VolumeAttachment::builder()
        .path(data.path())
        .serial("app-data")
        .build()])
    .restartable(true)
    .ssh_user("debian")
    .connect_timeout_secs(300)
    .state(qemu::InstanceState::Running)
    .add()?;

stack
    .host_file("page")
    .on(vm.connection())
    .path("/var/www/html/index.html")
    .content("<h1>node-a</h1>\n")
    .privileged(true)
    .add()?;
```

`connection` is the key output: every `host.*` resource and remote `check.Exec` accepts
it exactly as they accept a Linode connection. Other outputs include `ssh_port`, host
`port_forwards`, QMP `monitor`, serial `console`, per-network `ips`, attached `volumes`,
`pid`, and observed `state`.

Memory, CPU/machine/accelerator, image, and networks replace an instance. Running,
paused, and stopped state updates use QMP and graceful shutdown. Data-disk attachment
changes apply directly while stopped. A live change may restart only when both applied
and desired declarations set `restartable(true)`; otherwise it waits for an exact
`instance-restart` approval.

## Management lifecycle and egress

`management` controls when IFX can reach SSH. It does not decide whether the guest has
outbound access:

| Mode | Bootstrap | Steady state | Later changed apply |
|---|---|---|---|
| `Direct` | host-forwarded SSH | host-forwarded SSH | direct |
| `Via` | through `management_via` | through `management_via` | via |
| `DirectThenVia` | direct | through `management_via` | via |
| `DirectThenRemove` | direct | no host SSH forward | QMP temporarily restores direct SSH |
| `Immutable` | direct | no host SSH forward | refused; replace the VM |
| `None` | none | none | unavailable |

`DirectThenRemove` opens management only when an explicit apply or refresh needs it and
closes it at the execution boundary. Planning, drift detection, and scheduled or manual
check-only runs do not open it; affected checks report `unknown` and explain that an
apply is required. `Immutable` never reopens. Increment `bootstrap_revision` together
with the desired guest configuration to replace and bootstrap a new immutable VM.

Opening temporary management writes a cleanup lease before QEMU is changed. ifxd
replays those leases at startup and after cancellation. A completed bootstrap is sealed;
an interrupted first bootstrap is stopped and retried from the declared configuration.
Failed cleanup remains pending and is retried instead of being forgotten. The
`management_cleanup_pending` output exposes this state for graph and health views.

`egress = UserNat` leaves a QEMU user-mode NIC attached for outbound traffic. Temporary
management adds and removes only its SSH host forward. `egress = None` removes the
whole temporary management NIC after bootstrap and hot-plugs it during a changed
`DirectThenRemove` apply. Application `port_forwards` require persistent user NAT,
except on a permanently `Direct` instance.

`Via` and `DirectThenVia` require a socket network and a `management_via` connection.
The target uses its deterministic address on its first socket network. Nested hops keep
their own provider-generated identity files, so the host can manage a chain without
copying private management keys into guests. An execution-scoped connection cannot be
used as a bastion because it would make sealing order ambiguous.

```rust
let front = stack
    .qemu_network("front")
    .name("front")
    .dir(instance_dir)
    .add()?;
let back = stack
    .qemu_network("back")
    .name("back")
    .dir(instance_dir)
    .add()?;

let router = stack
    .qemu_instance("router")
    .dir(instance_dir)
    .image(image.path())
    .management(qemu::InstanceManagement::Direct)
    .networks([front.endpoint()])
    .add()?;
let middle = stack
    .qemu_instance("middle")
    .dir(instance_dir)
    .image(image.path())
    .management(qemu::InstanceManagement::DirectThenVia)
    .management_via(router.connection())
    .networks([front.endpoint(), back.endpoint()])
    .add()?;
let leaf = stack
    .qemu_instance("leaf")
    .dir(instance_dir)
    .image(image.path())
    .management(qemu::InstanceManagement::DirectThenVia)
    .management_via(middle.connection())
    .networks([back.endpoint()])
    .add()?;
```

Those references create both the physical two-link layout and the management dependency
chain: host → router → middle → leaf. The leaf has no NIC on the front network.

Each instance directory contains the system overlay, seed, management key,
`known_hosts`, pidfile, QMP socket, console log, and identity record. Delete requests
QMP quit, falls back to termination after validating process identity, and removes the
instance directory.

## Adoption and omitted surface

Read verifies the pidfile against `/proc`, validates the identity record, and queries
QMP status. A matching directory can be adopted after state loss; an identity mismatch
is never taken over.

Generic QMP, arbitrary arguments, general-purpose hotplug, snapshots, migration, passthrough, graphics,
UEFI/TPM, and non-x86 system emulation are not exposed. They require typed resources
and explicit lifecycle semantics.

Run [Lab 8](../labs/08-qemu/README.md) for the complete image → LAN → instances → SSH
configuration → health graph.

The real integration test is opt-in:

```console
$ IFX_QEMU_TESTS=1 \
  IFX_QEMU_TEST_IMAGE=/path/to/debian.qcow2 \
  cargo test -p ifx --test qemu --all-features -- --nocapture
```
