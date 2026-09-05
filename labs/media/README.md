# Lab media

Recordings are generated from the Rust lab runner so demonstrations and assertions use
the same workflow.

```console
$ cargo build -p ifx-cli -p ifxd -p ifx-labs
$ target/debug/ifx-labs record 8
$ target/debug/ifx-labs gif 8
```

`demo` and `record` print `#` commentary before every command and pause at reading
speed. `record --speed 2` is available for development, but published captures should
use the default competent-human pace.
