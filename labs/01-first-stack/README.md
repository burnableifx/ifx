# Lab 1: first stack

[`src/main.rs`](src/main.rs) declares a directory and two files using generated Rust
builders. `Context::config("base")` reads the value in `ifx.toml`; `Connection::local()`
selects the local host transport. Handles passed to `depends_on` become graph edges.

```console
$ target/debug/ifx-labs run 1
```

The important sequence is build → plan (exit 2) → apply → plan (exit 0) → destroy.
The first plan observes reality rather than trusting state. The second proves
idempotency. File modes are part of desired state, so changing a mode or content plans
an update.

Run the narrated version with `target/debug/ifx-labs demo 1`, then continue with
[Lab 2](../02-references-and-loops/README.md).
