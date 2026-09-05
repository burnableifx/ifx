# Lab 5: reusable Rust components

[`src/main.rs`](src/main.rs) packages a group of declarations into `declare_node` and
returns its typed handle. This is the component model: normal Rust modules, functions,
traits, tests, crates, and editor tooling around generated resource builders.

```console
$ target/debug/ifx-labs run 5
```

The schema remains authoritative. `ifx-gen` creates resource builders and typed output
methods in `ifx-program`; component code composes those types but does not duplicate
provider schemas. Invalid output selection or missing required inputs fails before a
Program can be accepted by ifxd.

Continue with the primary full-system target, [Lab 8](../08-qemu/README.md).
