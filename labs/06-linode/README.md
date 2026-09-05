# Lab 6: Linode

This optional paid exercise adapts [`examples/linode-web`](../../examples/linode-web)
to create Linode instances, configure nginx through each instance's `connection`
output, attach a firewall, and run HTTP and remote executable checks.

Export `LINODE_TOKEN`, create `id_ed25519.pub` in the example directory, and supply a
strong root password as configuration. Start ifxd with that directory as a trusted
stack, inspect `ifx plan`, and apply only after reviewing the billed resources.

Use [Lab 8](../08-qemu/README.md) for routine development. It exercises the same
instance → connection → host resources → checks chain without an account or bill.
