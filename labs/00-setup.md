# Lab 0: setup

Required for every lab:

- a current stable Rust toolchain;
- `cargo`, `rustfmt`, and `clippy`;
- enough space for the shared compiler cache and lab state.

```console
$ cargo build -p ifx-cli -p ifxd -p ifx-labs
$ target/debug/ifx --version
$ target/debug/ifxd --help
$ target/debug/ifx-labs list
```

Stacks do not build into local `target/` directories when ifxd owns them. The daemon
uses `${XDG_CACHE_HOME:-~/.cache}/ifx/rust` by default, isolates each Cargo project,
and garbage-collects an idle or oversized shared target. Stable emitter copies live in
each stack's `.ifx/build`. Set `IFX_CACHE_DIR` to move the compiler cache.

For manual work, start one daemon in the stack directory:

```console
$ ifxd --db "surrealkv://$PWD/.ifx/db" --stack "$PWD=default"
```

Then use `ifx build`, `ifx status`, `ifx plan`, and `ifx apply`. The daemon watches the
manifest, source tree, local path dependencies, workspace inputs, Cargo configuration,
toolchain selectors, and `ifx.toml`. Saving starts a coalesced background build; `ifx
build` reports its status or manually retries a failed build.

Lab 8 additionally needs `qemu-system-x86_64`, `qemu-img`, `ssh-keygen`, and one of
`cloud-localds`, `genisoimage`, or `xorriso`. KVM is optional; IFX falls back to TCG.

Continue with [Lab 1](01-first-stack/README.md).
