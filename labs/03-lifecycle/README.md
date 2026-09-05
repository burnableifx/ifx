# Lab 3: lifecycle

[`src/main.rs`](src/main.rs) demonstrates three independent lifecycle controls:
`creates` makes a command idempotent, `triggered_by` runs work only after a dependency
changes, and `protect` blocks replacement or deletion.

```console
$ target/debug/ifx-labs run 3
```

The runner first applies the protected graph. Before destroy it applies a new immutable
revision with `-c protect=false`; protection cannot be bypassed merely by asking for
destruction. This is the same revision-bound safety model used for QEMU disk loss and
instance restarts.

Continue with [Lab 4](../04-health-and-daemon/README.md).
