# Local files example

[`src/main.rs`](src/main.rs) manages a directory, a configuration file, and a checksum
file whose content references the first file's observed SHA-256 output. A required
executable check verifies both files.

Run it through ifxd as described in the [examples index](../README.md). Override the
default destination without rebuilding the artifact:

```console
$ ifx plan -c base=/tmp/another-ifx-example
```
