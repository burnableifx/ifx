# Minimal Rust emitter

This standalone Cargo project demonstrates the smallest IFX stack crate. It depends
only on `ifx-program`: the Program IR, stack collector, generated builders, and emitter
runtime. Provider implementations, transports, persistence, and async execution remain
inside `ifxd`.

Start the daemon from the repository root:

```console
$ cargo run -p ifxd -- \
    --db surrealkv://$PWD/.ifx/example-db \
    --stack "$PWD/examples/rust-program=default"
```

Then build and inspect the stack from another terminal:

```console
$ cargo run -p ifx-cli -- -C examples/rust-program build
$ cargo run -p ifx-cli -- -C examples/rust-program plan
$ cargo run -p ifx-cli -- -C examples/rust-program graph --format html --output graph.html
```

`ifxd` compiles the emitter into the shared, locked Cargo cache, copies one stable
runtime artifact under `.ifx/build`, executes it with the current configuration, and
persists the validated Program revision. The CLI never compiles or submits a Program.

Set a different destination without changing source:

```console
$ cargo run -p ifx-cli -- \
    -C examples/rust-program \
    -c base=/tmp/another-path \
    plan
```

Configuration-only resolution re-executes the current emitter and does not invoke
Cargo. Add ordinary Rust dependencies to `Cargo.toml` when the stack needs reusable
components or data transformations.

The original compile-size spike led to the shared garbage-collected cache and the
`ifx-program` extraction. Current cold, warm, edit, stack-size, binary, and QEMU results
are recorded in [Performance measurements](../../docs/performance.md).
