# Lab 2: references, loops, and components

[`src/main.rs`](src/main.rs) uses an ordinary Rust function to declare a node, loops to
create several nodes, and returns typed `Handle<memory::Value>` values. `vm.id()` and
`vm.value().select("port")` are deferred outputs: the emitter records references rather
than trying to read values that do not exist yet.

```console
$ target/debug/ifx-labs run 2
```

`ifx_program::concat!` combines literals and deferred values into a serializable
expression. Those references add dependency edges automatically. Try changing
`nodes = 2` in `ifx.toml`, save, and watch `ifx status`; the daemon rebuilds before the
next plan. Configuration supplied with `-c nodes=4` re-emits the cached artifact and
does not invoke Cargo.

Continue with [Lab 3](../03-lifecycle/README.md).
