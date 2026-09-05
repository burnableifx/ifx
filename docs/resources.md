# Resource types

> Generated from the built-in resource schemas. Run `cargo run -p ifx-cli -- stubs markdown -o docs/resources.md`; do not edit by hand.

## `check.exec`

Command health check: run a shell command on a host; exit status 0 is healthy, anything else unhealthy.

| input | type | flags | description |
|---|---|---|---|
| `on` | `connection` | required | Where to run: `local()` or a host's `connection` output. |
| `command` | `string` | required | Shell command, e.g. `systemctl is-active nginx`. |
| `privileged` | `bool` | default `false` | Run with sudo when the connection allows it. |
| `required` | `bool` | default `false` | Fail `apply` if the check is unhealthy right after the stack is applied. |
| `interval_secs` | `int` |  | How often `ifxd` runs this check; the daemon default applies if unset. |
| `timeout_secs` | `int` | default `10` | Give up and report unhealthy after this many seconds. |

| output | type | description |
|---|---|---|
| `status` | `"healthy" \| "degraded" \| "unhealthy" \| "unknown"` | Result of the most recent run during apply. |
| `message` | `string` | Human-readable detail from the last run. |
| `latency_ms` | `int` | Time the last run took. |
| `checked_at` | `string` | RFC 3339 timestamp of the last run. |

## `check.http`

HTTP health check: fetch a URL and expect a status code and, optionally, a body substring.

| input | type | flags | description |
|---|---|---|---|
| `url` | `string` | required | URL to request, e.g. `"http://" + web.ipv4 + "/healthz"`. |
| `method` | `string` | default `"GET"` | HTTP method. |
| `expect_status` | `int` | default `200` | Expected response status code. |
| `expect_body` | `string` |  | Substring the response body must contain. |
| `headers` | `dict[string, string]` |  | Extra request headers. |
| `insecure` | `bool` | default `false` | Skip TLS certificate verification. |
| `required` | `bool` | default `false` | Fail `apply` if the check is unhealthy right after the stack is applied. |
| `interval_secs` | `int` |  | How often `ifxd` runs this check; the daemon default applies if unset. |
| `timeout_secs` | `int` | default `10` | Give up and report unhealthy after this many seconds. |

| output | type | description |
|---|---|---|
| `status` | `"healthy" \| "degraded" \| "unhealthy" \| "unknown"` | Result of the most recent run during apply. |
| `message` | `string` | Human-readable detail from the last run. |
| `latency_ms` | `int` | Time the last run took. |
| `checked_at` | `string` | RFC 3339 timestamp of the last run. |

## `check.tcp`

TCP health check: a connection to host:port must succeed.

| input | type | flags | description |
|---|---|---|---|
| `host` | `string` | required | Hostname or IP, typically `web.ipv4`. |
| `port` | `int` | required | TCP port. |
| `required` | `bool` | default `false` | Fail `apply` if the check is unhealthy right after the stack is applied. |
| `interval_secs` | `int` |  | How often `ifxd` runs this check; the daemon default applies if unset. |
| `timeout_secs` | `int` | default `10` | Give up and report unhealthy after this many seconds. |

| output | type | description |
|---|---|---|
| `status` | `"healthy" \| "degraded" \| "unhealthy" \| "unknown"` | Result of the most recent run during apply. |
| `message` | `string` | Human-readable detail from the last run. |
| `latency_ms` | `int` | Time the last run took. |
| `checked_at` | `string` | RFC 3339 timestamp of the last run. |

## `host.exec`

Run a shell command on a host. Without guards it runs once and again whenever its inputs change or a trigger fires; with guards it runs whenever they say the work is still needed.

| input | type | flags | description |
|---|---|---|---|
| `on` | `connection` | required, replace | Connection to the target host: `{"kind": "local"}` or an SSH connection, usually an instance's `connection` output. Changing it replaces the resource on the new host. |
| `command` | `string` | required | Shell script run with `sh -c`; a non-zero exit fails the apply. |
| `creates` | `string` |  | Path on the host; the command is skipped while it exists (relative to `cwd`). |
| `unless` | `string` |  | Shell command evaluated on every plan; the main command is skipped when it exits 0. |
| `only_if` | `string` |  | Shell command evaluated on every plan; the main command runs only when it exits 0. |
| `cwd` | `string` |  | Working directory for the command and its guards. |
| `env` | `dict[string, string]` |  | Environment variables exported to the command and its guards. |
| `privileged` | `bool` | default `false` | Run the command and its guards through `sudo -n` when the connection allows sudo. |

| output | type | description |
|---|---|---|
| `stdout` | `string` | Standard output of the last run in this apply; null when the command did not run. |
| `stderr` | `string` | Standard error of the last run in this apply; null when the command did not run. |
| `status` | `int` | Exit status of the last run (always 0, since failures abort the apply). |
| `ran_at` | `string` | RFC 3339 UTC timestamp of the most recent run; null if it has never run. |

## `host.file`

A file or directory on a host. Content, permissions and ownership are reconciled in place; `state: "absent"` removes it.

| input | type | flags | description |
|---|---|---|---|
| `on` | `connection` | required, replace | Connection to the target host: `{"kind": "local"}` or an SSH connection, usually an instance's `connection` output. Changing it replaces the resource on the new host. |
| `path` | `string` | required, replace | Absolute path on the host. Changing it replaces the resource: the old path is removed and the new one created. |
| `content` | `string` |  | Exact file contents as a UTF-8 string. Mutually exclusive with `source`; ignored for directories. |
| `source` | `string` |  | Local file (relative to the working directory) whose bytes are copied to the host, compared by SHA-256. Mutually exclusive with `content`. |
| `mode` | `string` |  | Octal permission bits such as "0644" or "0755". When unset, existing permissions are kept and new files get 0644. |
| `owner` | `string` |  | Owning user name. Changing it needs root: `privileged: true` on a sudo-enabled connection. |
| `group` | `string` |  | Owning group name. Changing it needs root or membership in the target group. |
| `state` | `"present" \| "absent"` | default `"present"` | "present" ensures the path exists as described; "absent" removes it (directories recursively). |
| `directory` | `bool` | default `false`, replace | Manage a directory instead of a regular file (`content`/`source` are ignored). Switching between the two replaces the resource. |
| `privileged` | `bool` | default `false` | Run every command through `sudo -n` when the connection allows sudo; needed for paths the connecting user cannot write. |

| output | type | description |
|---|---|---|
| `path` | `string` | The managed path, as given. |
| `exists` | `bool` | Whether the path existed after the last apply. |
| `sha256` | `string` | Hex SHA-256 of the file contents; null for directories and absent files. |

## `host.package`

System packages installed with the host's package manager (apt, dnf, yum or apk, detected automatically). Commands run non-interactively.

| input | type | flags | description |
|---|---|---|---|
| `on` | `connection` | required, replace | Connection to the target host: `{"kind": "local"}` or an SSH connection, usually an instance's `connection` output. Changing it replaces the resource on the new host. |
| `names` | `list[string]` | required | Package names as the host's package manager knows them. |
| `state` | `"present" \| "absent"` | default `"present"` | "present" installs any that are missing; "absent" removes any that are installed. |
| `update_cache` | `bool` | default `false` | Refresh the package index (`apt-get update` and friends) before installing or removing. |

| output | type | description |
|---|---|---|
| `installed` | `list[string]` | Which of `names` were installed after the last apply. |
| `manager` | `string` | Package manager detected on the host: "apt", "dnf", "yum" or "apk". |

## `host.service`

A systemd unit: whether it is enabled at boot and running now. Restarts (or reloads) when a resource listed in `triggers` changes.

| input | type | flags | description |
|---|---|---|---|
| `on` | `connection` | required, replace | Connection to the target host: `{"kind": "local"}` or an SSH connection, usually an instance's `connection` output. Changing it replaces the resource on the new host. |
| `name` | `string` | required, replace | Unit name such as "nginx" or "nginx.service". Changing it replaces the resource (the old unit is stopped/disabled as `delete` would). |
| `enabled` | `bool` |  | Start the unit at boot (`systemctl enable`/`disable`). Left alone when unset. |
| `state` | `"running" \| "stopped"` |  | Whether the unit should be active right now. Left alone when unset. |
| `daemon_reload` | `bool` | default `false` | Run `systemctl daemon-reload` before acting, for freshly written unit files. |
| `restart_on_trigger` | `bool` | default `true` | Restart (or reload) the unit when one of the resource's `triggers` changes. |
| `reload` | `bool` | default `false` | On trigger, use `systemctl reload-or-restart` instead of a full restart. |

| output | type | description |
|---|---|---|
| `active_state` | `string` | systemd ActiveState after the last apply, e.g. "active" or "inactive". |
| `unit_file_state` | `string` | systemd UnitFileState after the last apply, e.g. "enabled", "disabled", "static". |

## `host.user`

A local user account: supplementary groups, shell, home directory and SSH authorized keys. Needs root on the host for any change.

| input | type | flags | description |
|---|---|---|---|
| `on` | `connection` | required, replace | Connection to the target host: `{"kind": "local"}` or an SSH connection, usually an instance's `connection` output. Changing it replaces the resource on the new host. |
| `name` | `string` | required, replace | Login name. Changing it replaces the resource (old account deleted, new one created). |
| `groups` | `list[string]` |  | Exact set of supplementary groups (the primary group is not included). Left alone when unset. |
| `shell` | `string` |  | Login shell, e.g. "/bin/bash". Left alone when unset. |
| `home` | `string` |  | Home directory path; an existing home is moved when this changes. Left alone when unset. |
| `system` | `bool` | default `false` | Create as a system account (`useradd -r`: low UID, no home directory). Only affects creation. |
| `authorized_keys` | `list[string]` |  | Public key lines written to `~/.ssh/authorized_keys` (mode 0600, owned by the user), replacing its previous contents. Left alone when unset. |
| `state` | `"present" \| "absent"` | default `"present"` | "present" ensures the account exists as described; "absent" deletes it (the home directory is kept). |

| output | type | description |
|---|---|---|
| `uid` | `int` | Numeric user id. |
| `gid` | `int` | Numeric primary group id. |
| `home` | `string` | Home directory as recorded in the passwd database. |
| `shell` | `string` | Login shell as recorded in the passwd database. |
| `exists` | `bool` | Whether the account existed after the last apply. |

## `linode.domain`

A DNS zone served by Linode's nameservers; add records with `linode.domain_record`.

| input | type | flags | description |
|---|---|---|---|
| `domain` | `string` | required, replace | Zone name, e.g. `example.com`; an existing zone with this name is adopted instead of created. |
| `type` | `"master" \| "slave"` | default `"master"` | `master` zones are edited here; `slave` zones copy records from another primary nameserver. |
| `soa_email` | `string` |  | Contact address published in the zone's SOA record; required for `master` zones. |
| `description` | `string` |  | Human-readable description shown in DNS Manager. |
| `status` | `"active" \| "disabled"` | default `"active"` | Whether Linode renders and serves the zone. |
| `axfr_ips` | `list[string]` |  | IP addresses allowed to transfer this zone; leave empty unless AXFR is intentional. |
| `master_ips` | `list[string]` |  | Primary DNS server addresses; required for `slave` zones. |
| `ttl_sec` | `int` |  | Default seconds resolvers may cache records from this zone (0 = Linode default). |
| `refresh_sec` | `int` |  | Seconds before a slave refreshes its copy (0 = Linode default). |
| `retry_sec` | `int` |  | Seconds before retrying a failed slave refresh (0 = Linode default). |
| `expire_sec` | `int` |  | Seconds before an unrefreshed slave zone stops being authoritative (0 = Linode default). |
| `tags` | `list[string]` |  | Free-form labels for grouping and filtering in the Cloud Manager. |

| output | type | description |
|---|---|---|
| `id` | `int` | Numeric domain id; pass as `domain_id` to records. |

## `linode.domain_record`

A single DNS record (A, CNAME, MX, ...) in a `linode.domain` zone.

| input | type | flags | description |
|---|---|---|---|
| `domain_id` | `int` | required, replace | Id of the zone the record lives in, usually a reference like `zone.id`. |
| `type` | `"A" \| "AAAA" \| "CNAME" \| "TXT" \| "MX" \| "SRV" \| "NS" \| "CAA" \| "PTR"` | required, replace | Record type: `A`/`AAAA` map names to addresses, `CNAME` aliases a name, `MX` names mail servers, `TXT` holds text, `SRV`/`NS`/`CAA`/`PTR` as per DNS. |
| `name` | `string` |  | Hostname relative to the zone (`www` for `www.example.com`); empty or omitted for the zone apex. |
| `target` | `string` | required | Record value: an IP for `A`/`AAAA`, a hostname for `CNAME`/`MX`/`NS`, text for `TXT`. |
| `ttl_sec` | `int` |  | Seconds resolvers may cache this record (0 = zone default). |
| `priority` | `int` |  | Preference for `MX`/`SRV` records; lower is tried first. |
| `weight` | `int` |  | Relative weight among SRV records with the same priority. |
| `port` | `int` |  | Target port; required for SRV records. |
| `service` | `string` |  | Service name without surrounding underscores; required for SRV records. |
| `tag` | `"issue" \| "issuewild" \| "iodef"` |  | CAA property tag; required for CAA records. |

| output | type | description |
|---|---|---|
| `id` | `int` | Numeric record id. |

## `linode.firewall`

A Linode Cloud Firewall: network rules enforced outside the instance, attached to one or more instances.

| input | type | flags | description |
|---|---|---|---|
| `label` | `string` | required | Unique firewall name; an existing firewall with this label is adopted instead of created. |
| `status` | `"enabled" \| "disabled"` | default `"enabled"` | Whether this firewall currently enforces its rules. |
| `inbound_policy` | `"ACCEPT" \| "DROP"` | default `"DROP"` | What happens to incoming traffic no `inbound` rule matches. |
| `outbound_policy` | `"ACCEPT" \| "DROP"` | default `"ACCEPT"` | What happens to outgoing traffic no `outbound` rule matches. |
| `inbound` | `list[{label: string, action: "ACCEPT" \| "DROP", protocol: "TCP" \| "UDP" \| "ICMP" \| "IPENCAP", ports: string, addresses: {ipv4: list[string], ipv6: list[string]}, description: string}]` |  | Rules for traffic arriving at the instances, evaluated in order. |
| `outbound` | `list[{label: string, action: "ACCEPT" \| "DROP", protocol: "TCP" \| "UDP" \| "ICMP" \| "IPENCAP", ports: string, addresses: {ipv4: list[string], ipv6: list[string]}, description: string}]` |  | Rules for traffic leaving the instances, evaluated in order. |
| `linodes` | `list[int]` |  | Ids of legacy-interface instances to protect, usually references like `web.id`. |
| `interfaces` | `list[int]` |  | Ids of public or VPC Linode interfaces to protect. |
| `nodebalancers` | `list[int]` |  | Ids of NodeBalancers to protect; only inbound TCP rules apply. |
| `tags` | `list[string]` |  | Free-form labels for grouping and filtering in the Cloud Manager. |

| output | type | description |
|---|---|---|
| `id` | `int` | Numeric firewall id. |
| `status` | `string` | `enabled`, `disabled` or `deleted`. |

## `linode.instance`

A Linode virtual machine. Outputs an SSH connection so `host.*` resources can configure it once it is running.

| input | type | flags | description |
|---|---|---|---|
| `label` | `string` | required | Unique name shown in the Linode Cloud Manager; an existing instance with this label is adopted instead of created. |
| `region` | `"gb-lon" \| "se-sto" \| "es-mad" \| "in-maa" \| "jp-osa" \| "it-mil" \| "us-mia" \| "id-cgk" \| "us-lax" \| "nl-ams" \| "au-mel" \| "in-bom-2" \| "de-fra-2" \| "sg-sin-2" \| "jp-tyo-3" \| "us-iad-2" \| "fr-par-2" \| "br-gru" \| "us-sea" \| "fr-par" \| "us-ord" \| "us-iad" \| "ap-southeast" \| "ca-central" \| "ap-west" \| "us-central" \| "ap-northeast" \| "eu-central" \| "ap-south" \| "eu-west" \| "us-east" \| "us-southeast" \| "us-west" \| string` | required, replace | Data-center slug, e.g. `us-east` or `eu-central` (`linode-cli regions list`). |
| `type` | `"g6-nanode-1" \| "g6-standard-1" \| "g6-standard-2" \| "g6-standard-4" \| "g6-standard-6" \| "g6-standard-8" \| "g6-standard-16" \| "g6-standard-20" \| "g6-standard-24" \| "g6-standard-32" \| "g7-highmem-1" \| "g7-highmem-2" \| "g7-highmem-4" \| "g7-highmem-8" \| "g7-highmem-16" \| "g6-dedicated-2" \| "g6-dedicated-4" \| "g6-dedicated-8" \| "g6-dedicated-16" \| "g6-dedicated-32" \| "g6-dedicated-48" \| "g6-dedicated-50" \| "g6-dedicated-56" \| "g1-gpu-rtx6000-1" \| "g1-gpu-rtx6000-2" \| "g1-gpu-rtx6000-3" \| "g1-gpu-rtx6000-4" \| "g7-premium-2" \| "g7-premium-4" \| "g7-premium-8" \| "g7-premium-16" \| "g7-premium-32" \| "g7-premium-48" \| "g7-premium-50" \| "g7-premium-56" \| "g2-gpu-rtx4000a1-s" \| "g2-gpu-rtx4000a1-m" \| "g2-gpu-rtx4000a1-l" \| "g2-gpu-rtx4000a1-xl" \| "g2-gpu-rtx4000a2-s" \| "g2-gpu-rtx4000a2-m" \| "g2-gpu-rtx4000a2-hs" \| "g2-gpu-rtx4000a4-s" \| "g2-gpu-rtx4000a4-m" \| "g1-accelerated-netint-vpu-t1u1-s" \| "g1-accelerated-netint-vpu-t1u1-m" \| "g1-accelerated-netint-vpu-t1u2-s" \| "g1-accelerated-netint-vpu-t1u8-s" \| "g1-accelerated-netint-vpu-t1u8-m" \| "g1-accelerated-netint-vpu-t1u8-l" \| "g8-dedicated-4-2" \| "g8-dedicated-8-4" \| "g8-dedicated-16-8" \| "g8-dedicated-32-16" \| "g8-dedicated-64-32" \| "g8-dedicated-96-48" \| "g8-dedicated-128-64" \| "g8-dedicated-256-128" \| "g8-dedicated-512-256" \| "g8-dedicated-8-2" \| "g8-dedicated-16-4" \| "g8-dedicated-32-8" \| "g8-dedicated-64-16" \| "g8-dedicated-96-24" \| "g8-dedicated-128-32" \| "g8-dedicated-256-64" \| "g8-dedicated-512-128" \| "g7-dedicated-4-2" \| "g7-dedicated-8-4" \| "g7-dedicated-16-8" \| "g7-dedicated-32-16" \| "g7-dedicated-64-32" \| "g7-dedicated-96-48" \| "g7-dedicated-128-50" \| "g7-dedicated-256-56" \| string` | required | Plan slug that sets CPU/RAM/disk, e.g. `g6-nanode-1`; changing it resizes the instance in place (with a reboot). |
| `image` | `"linode/almalinux10" \| "linode/almalinux8" \| "linode/almalinux9" \| "linode/alpine3.21" \| "linode/alpine3.22" \| "linode/alpine3.23" \| "linode/alpine3.24" \| "linode/arch" \| "linode/centos-stream10" \| "linode/centos-stream9" \| "linode/debian11" \| "linode/debian12" \| "linode/debian13" \| "linode/fedora43" \| "linode/gentoo" \| "linode/kali" \| "linode/debian13-kube-v1.34.9" \| "linode/debian13-kube-v1.35.6" \| "linode/debian13-kube-v1.36.2" \| "linode/debian13-kube-v1.36.3" \| "linode/rocky10" \| "linode/rocky8" \| "linode/rocky9" \| "linode/slackware15.0" \| "linode/ubuntu22.04" \| "linode/ubuntu22.04-kube" \| "linode/ubuntu24.04-kube-vmulti" \| "linode/ubuntu24.04" \| "linode/ubuntu26.04" \| string` | replace | Operating-system image slug the disk is built from, e.g. `linode/debian12`. |
| `backup_id` | `int` | replace | Backup id to restore into the new instance; mutually exclusive with `image`. |
| `root_pass` | `string` | replace, sensitive | Root password set at first boot; never read back from the API. |
| `authorized_keys` | `list[string]` | replace | Public SSH keys written to root's `authorized_keys` at first boot. |
| `authorized_users` | `list[string]` | replace | Linode account usernames whose profile SSH keys are installed at first boot. |
| `disk_encryption` | `"enabled" \| "disabled"` | replace | Local disk encryption policy selected when the instance is created. |
| `boot_size` | `int` | replace | Primary boot disk size in MiB; remaining plan storage stays unallocated. |
| `swap_size` | `int` | replace | Swap disk size in MiB (Linode defaults to 512). |
| `interface_generation` | `"legacy_config" \| "linode"` | replace | Networking model selected at creation; this cannot be changed in place. |
| `interfaces` | `list[any]` | replace | Linode or legacy interface definitions passed to the create API. |
| `ipv4` | `list[string]` | replace | An unassigned reserved public IPv4 address to assign at creation. |
| `kernel` | `string` | replace | Kernel id selected for the initial configuration profile. |
| `network_helper` | `bool` | replace | Enable Network Helper for Linode-interface networking. |
| `placement_group_id` | `int` | replace | Placement group to join at creation; it must be in the selected region. |
| `stackscript_id` | `int` | replace | StackScript id to run during image deployment. |
| `stackscript_data` | `any` | replace, sensitive | StackScript UDF values; treated as sensitive because they commonly contain credentials. |
| `tags` | `list[string]` |  | Free-form labels for grouping and filtering in the Cloud Manager. |
| `private_ip` | `bool` | replace | Also assign a private (192.168.x.x) address reachable from other Linodes in the same region. |
| `backups_enabled` | `bool` |  | Enrol in the paid Linode Backup Service (daily snapshots). |
| `backup_schedule` | `{day: "Scheduling" \| "Sunday" \| "Monday" \| "Tuesday" \| "Wednesday" \| "Thursday" \| "Friday" \| "Saturday" \| string, window: string}` |  | Paid Backup Service schedule. |
| `maintenance_policy` | `"linode/migrate" \| "linode/power_off_on"` |  | Prefer live migration or power-off/on during host maintenance. |
| `watchdog_enabled` | `bool` |  | Enable Lassie to restart an instance that powers off unexpectedly. |
| `alerts` | `{cpu: int, io: int, network_in: int, network_out: int, transfer_quota: int}` |  | Cloud Manager alert thresholds. |
| `booted` | `bool` | default `true` | Keep the instance powered on (`true`) or shut down (`false`). |
| `firewall_id` | `int` | replace | Id of a Cloud Firewall to attach at creation; prefer `linode.firewall(linodes=...)` for attachments that can change later. |
| `user_data` | `string` | replace, sensitive | cloud-init user data (e.g. a `#cloud-config` document) run on first boot; base64-encoded for you. |
| `ssh_user` | `string` | default `"root"` | Login user for the `connection` output; not sent to Linode. |
| `connect_timeout_secs` | `int` |  | Seconds `host.*` resources keep retrying the first SSH connection while the instance finishes booting. |

| output | type | description |
|---|---|---|
| `id` | `int` | Numeric Linode id. |
| `label` | `string` | Instance label. |
| `ipv4` | `string` | First public IPv4 address. |
| `ipv4s` | `list[string]` | Every IPv4 address, public and private. |
| `ipv6` | `string` | Public IPv6 address (without prefix length). |
| `status` | `string` | Lifecycle state reported by Linode, e.g. `running` or `offline`. |
| `region` | `string` | Region slug. |
| `type` | `string` | Plan slug. |
| `disk_encryption` | `string` | Observed disk encryption policy. |
| `interface_generation` | `string` | Observed networking model. |
| `maintenance_policy` | `string` | Observed host maintenance policy. |
| `watchdog_enabled` | `bool` | Whether Lassie is enabled. |
| `connection` | `connection` | SSH connection to the public IPv4 as `ssh_user`; pass as `on=` to `host.*` resources. |

## `memory.value`

Holds a value in memory; outputs it back as `value`.

| input | type | flags | description |
|---|---|---|---|
| `value` | `any` | required | Any JSON value. |
| `key` | `string` | replace | Changing this replaces the resource. |

| output | type | description |
|---|---|---|
| `id` | `string` |  |
| `value` | `any` |  |
| `key` | `string` |  |

## `qemu.image`

A downloaded, SHA-256-verified cloud image cached for local QEMU instances. The cache survives destroy and is adopted on the next apply.

| input | type | flags | description |
|---|---|---|---|
| `url` | `string` | required, replace | HTTPS URL of a qcow2 cloud image; changing it replaces the cache entry. |
| `sha256` | `string` | required | Expected lowercase or uppercase SHA-256 digest of the downloaded image. |
| `dir` | `string` | required, replace | Persistent QEMU working directory containing the image cache. |

| output | type | description |
|---|---|---|
| `path` | `string` | Absolute or stack-relative cached image path. |
| `size` | `int` | Cached image size in bytes. |

## `qemu.instance`

A local QEMU virtual machine with lifecycle-controlled SSH management for `host.*` resources.

| input | type | flags | description |
|---|---|---|---|
| `dir` | `string` | required, replace | Persistent QEMU working directory for disks, seeds, keys, pidfiles, and monitor sockets. |
| `image` | `string` | required, replace | Path to a pristine qcow2 cloud image, normally `qemu.image.path`. |
| `memory_mb` | `int` | default `1024`, replace | Guest memory in MiB; changing it replaces the instance. |
| `cpus` | `int` | default `1`, replace | Virtual CPU count; changing it replaces the instance. |
| `machine` | `"q35" \| "pc" \| string` | default `"q35"`, replace | QEMU x86 machine type; other values supported by the installed QEMU are accepted. |
| `cpu` | `string` | replace | Optional QEMU CPU model; defaults to `host` with KVM and `max` with TCG. |
| `acceleration` | `"auto" \| "kvm" \| "tcg"` | default `"auto"`, replace | Execution accelerator: detect KVM with TCG fallback, require KVM, or force TCG. |
| `disk_gb` | `int` | default `10`, replace | Overlay disk size in GiB; changing it replaces the instance. |
| `authorized_keys` | `list[string]` | default `[]`, replace | Additional public SSH keys installed alongside the provider-managed key. |
| `user_data` | `string` | default `""`, replace, sensitive | Optional cloud-init user data. Provider SSH access is supplied separately as vendor data. |
| `ssh_port` | `int` | replace | Loopback port used by direct or execution-scoped SSH; omit to allocate an available port. |
| `management` | `"direct" \| "via" \| "direct_then_via" \| "direct_then_remove" \| "immutable" \| "none"` | default `"direct"`, replace | Guest-management lifecycle. Transitional modes remove direct host access after bootstrap; `direct_then_remove` reopens it only for changed applies, while `immutable` requires replacement. |
| `management_via` | `connection` |  | Bastion connection for `via` and `direct_then_via`; nested connections retain per-hop SSH identities. |
| `egress` | `"user_nat" \| "none"` | default `"user_nat"`, replace | Steady-state outbound NIC, independent of management exposure. `user_nat` has no SSH host forward unless management is direct or temporarily activated. |
| `bootstrap_revision` | `string` | default `"1"`, replace | Operator-controlled replacement token for immutable guests; increment it to rebuild and bootstrap a new instance. |
| `networks` | `list[string]` | default `[]`, replace | `qemu.network.endpoint` references for additional guest NICs. |
| `port_forwards` | `dict[string, int]` | default `{}`, replace | Additional loopback TCP forwards as `{host_port: guest_port}`; useful for local HTTP/TCP checks. |
| `ssh_user` | `"debian" \| "root" \| string` | default `"debian"`, replace | Cloud guest account used by the SSH connection output. |
| `hostname` | `string` | replace | Guest hostname supplied through NoCloud metadata; omitted values are derived from the resource name. |
| `connect_timeout_secs` | `int` | default `300` | Seconds to wait for SSH during boot and from downstream `host.*` resources. |
| `volumes` | `list[{path: string, read_only: bool, serial: string}]` | default `[]` | Durable data disks, normally built from `qemu.volume.path`; changing attachments updates a stopped VM or performs an approved restart. |
| `restartable` | `bool` | default `false` | Allow IFX to restart a live VM for attachment changes. Both the applied and desired declarations must opt in; otherwise the restart requires approval. |
| `state` | `"running" \| "paused" \| "stopped"` | default `"running"` | Desired VM power state; start, pause, resume, and stop are in-place updates. |

| output | type | description |
|---|---|---|
| `pid` | `int` | Live local QEMU process id, or null while stopped. |
| `ssh_port` | `int` | Allocated loopback SSH port, or null when direct access is never used. |
| `port_forwards` | `dict[string, int]` | Configured host-to-guest TCP port forwards. |
| `connection` | `connection` | SSH connection to the guest; pass it to `host.*.on(...)` exactly like a cloud instance connection. |
| `management` | `string` | Configured management lifecycle. |
| `management_attached` | `bool` | Whether direct host SSH access is currently attached. |
| `management_cleanup_pending` | `bool` | Whether interrupted execution left durable management cleanup intent. |
| `egress` | `string` | Configured steady-state egress policy. |
| `monitor` | `string` | QMP monitor socket path, or `none` where unavailable. |
| `console` | `string` | Serial console log path. |
| `ips` | `list[string]` | Guest IPv4 addresses in the same order as the attached `networks`. |
| `volumes` | `list[{path: string, read_only: bool, serial: string}]` | Observed virtio data-disk attachments in guest device order. |
| `state` | `"running" \| "paused" \| "stopped"` | Observed VM power state. |

## `qemu.network`

A rootless local QEMU network. Socket mode is a multicast Ethernet bus shared by attached VMs.

| input | type | flags | description |
|---|---|---|---|
| `name` | `string` | required, replace | Stable network name used to allocate its endpoint and guest subnet. |
| `mode` | `"socket" \| "user"` | default `"socket"`, replace | `socket` interconnects VMs; `user` adds an isolated outbound-only NIC. |
| `dir` | `string` | required, replace | Persistent QEMU working directory containing the network record. |
| `cidr` | `string` | replace | Optional RFC 1918 /24 guest subnet; omitted values are allocated deterministically. |

| output | type | description |
|---|---|---|
| `id` | `string` | Stable local network identifier. |
| `cidr` | `string` | Allocated guest subnet. |
| `endpoint` | `string` | Provider endpoint consumed by `qemu.instance.networks`; referencing it creates a graph edge. |

## `qemu.volume`

A durable QEMU disk created blank, as a backing overlay, or as an independent image copy.

| input | type | flags | description |
|---|---|---|---|
| `dir` | `string` | required, replace | Persistent QEMU working directory containing owned volumes. |
| `size_gb` | `int` | required | Virtual size in GiB. Growth is in place; shrink replaces the volume after approval. |
| `format` | `"qcow2" \| "raw"` | default `"qcow2"`, replace | On-disk image format. |
| `source` | `string` | replace | Optional source image path, normally `qemu.image.path`. |
| `source_mode` | `"overlay" \| "copy"` | default `"overlay"`, replace | Use the source as a qcow2 backing file, or create an independent copy. |
| `preallocation` | `"off" \| "metadata" \| "falloc" \| "full"` | default `"off"`, replace | qemu-img allocation policy used only when the volume is created. |

| output | type | description |
|---|---|---|
| `path` | `string` | Absolute owned volume path. |
| `format` | `"qcow2" \| "raw"` | Observed image format for typed instance attachments. |
| `virtual_size_bytes` | `int` | Observed virtual capacity in bytes. |
| `allocated_size_bytes` | `int` | Host bytes currently allocated according to qemu-img. |
| `backing_path` | `string` | Observed backing image path for overlay volumes, otherwise null. |
