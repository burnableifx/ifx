# Linode web example

[`src/main.rs`](src/main.rs) creates configurable Linode instances, configures nginx
over SSH, attaches a firewall, and declares HTTP and remote service checks.

This creates billed resources. Export `LINODE_TOKEN`, create `id_ed25519.pub`, and pass
a strong root password as sensitive configuration:

```console
$ ifx plan -c root_pass='a-long-random-secret'
$ ifx apply -y -c root_pass='a-long-random-secret'
```

Prefer [Lab 8](../../labs/08-qemu/README.md) for normal development and CI.
