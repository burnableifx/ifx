# Examples

Copyable Rust stacks:

- [`local-files`](local-files/README.md) — local files, output references, and a required check;
- [`qemu-web`](qemu-web/README.md) — two rootless VMs, shared LAN, SSH configuration, and checks;
- [`linode-web`](linode-web/README.md) — paid cloud instances and firewall;
- [`rust-program`](rust-program/README.md) — a minimal standalone emitter fixture.

Each directory contains `Cargo.toml`, `Cargo.lock`, `ifx.toml`, and `src/main.rs`. Start
ifxd with the example directory as a trusted stack, then use the normal CLI:

```console
$ ifxd --db "surrealkv://$PWD/.ifx/db" --stack "$PWD=default"
$ ifx build
$ ifx plan
$ ifx apply -y
$ ifx check
$ ifx destroy -y
```
