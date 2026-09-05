# Linode provider

The Linode provider manages four Akamai Connected Cloud API v4 resource types. It is
feature-complete within those resource boundaries for normal create/read/update/delete,
adoption, and graph references; it is not a generic wrapper around the full Linode API.

For systemd services, load a credential named `linode-token`. The provider reads
`$CREDENTIALS_DIRECTORY/linode-token` directly; the token never needs to enter the
process environment. A configured credential directory is authoritative: an unreadable,
empty, or invalid file fails without falling back to `LINODE_TOKEN`. Terminal CR/LF
characters are removed; the token must otherwise be nonempty ASCII without whitespace.

Without `CREDENTIALS_DIRECTORY`, set `LINODE_TOKEN` for `ifxd`. Both sources are resolved
lazily, so local-only stacks do not need Linode credentials. Explicit client tokens take
precedence over either source. `LINODE_API_URL` can point tests or private gateways at a
compatible endpoint.

## Exposed resources

| Resource | Coverage |
|---|---|
| `linode.instance` | Image, backup, or offline empty provisioning; authentication; cloud-init and StackScripts; encryption and disk sizing; reserved/private IPv4; legacy or Linode interfaces; placement/firewall selection; backup enrollment and schedule; alerts, maintenance, watchdog, tags, resize, boot/shutdown; SSH connection output |
| `linode.firewall` | Ordered inbound/outbound rules and policies, enabled/disabled state, tags, and attachments to legacy Linodes, Linode interfaces, and NodeBalancers |
| `linode.domain` | Master/slave zones, SOA contact, status, description, TTL/refresh/retry/expiry timing, AXFR allowlist, master addresses, and tags |
| `linode.domain_record` | A, AAAA, CNAME, TXT, MX, SRV, NS, CAA, and PTR fields, including SRV priority/weight/port/service and CAA tag |

Every type adopts by provider identity when IFX state is absent. Instances, firewalls,
and domains use their unique labels/names. DNS record adoption refuses an ambiguous
type/name match rather than selecting one of several valid records arbitrarily.

## Instance boundary

Create-only values are marked `replace`, including image or backup source, disk layout,
encryption, networking generation, interface definitions, keys, cloud-init,
StackScripts, placement, and initial IP selection. Plan type changes use Linode's
in-place resize operation. Label, tags, alerts, backup configuration, maintenance
policy, watchdog, and power state update in place.

Image provisioning requires one of `root_pass`, `authorized_keys`, or
`authorized_users`. StackScripts require an image. An empty instance has no IFX-managed
disk/configuration resources and therefore requires `booted=False`.

`connection` prefers public IPv4 and falls back to IPv6. Pass it directly to
`host.*.on(...)` or `check.Exec.on(...)`.

The `interfaces` input intentionally accepts API-shaped dictionaries. Linode's legacy
and current interface objects are mutually exclusive and evolve independently; IFX
still validates the surrounding instance lifecycle and preserves the object in the
Program IR.

The API's deprecated display-only `group` field is intentionally omitted from instances
and domains; tags are the supported grouping mechanism.

## Firewall boundary

Rules support TCP, UDP, ICMP, and IPENCAP, optional labels/descriptions/ports, IPv4 and
IPv6 CIDRs, and ordered ACCEPT/DROP behavior. Attachments are reconciled independently:

- `linodes` targets instances using legacy configuration interfaces;
- `interfaces` targets current public or VPC Linode interface ids;
- `nodebalancers` targets NodeBalancer ids.

## API families not yet modeled

The provider does not currently declare volumes, VPCs/subnets, Linode interface CRUD,
NodeBalancers/configs/nodes, placement groups, reserved IP resources, LKE, databases,
Object Storage, images, StackScripts, SSH keys, account/IAM objects, or imperative
operations such as rescue, rebuild, clone, restore, migrate, and snapshot.

Those should be added as typed resources with stable adoption keys and lifecycle rules.
A generic `linode.api` escape hatch would bypass planning, replacement semantics,
sensitive-field handling, and dependency inference, so it is intentionally absent.

The authoritative field list is the generated [resource reference](resources.md).
Upstream behavior is defined by the [Akamai Linode API reference](https://techdocs.akamai.com/linode-api/reference).
