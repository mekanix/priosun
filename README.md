# Priosun

Priosun is a Rust tool for managing FreeBSD jails and bhyve virtual machines.
It provides a client/daemon architecture: `priosun` sends requests over a Unix
socket, while `priosund` executes them with the required privileges.

## Features

- Jail lifecycle management
- Native bhyve VM lifecycle management
- NVMe-backed VM disks
- Optional ISO, VNC, and TPM support for VMs
- Shared lifecycle commands for jails and VMs
- ZFS dataset and volume inspection
- nvtree-based Unix-socket protocol
- Public Rust client library

## Requirements

- FreeBSD 13 or newer
- Root privileges for jail, VM, storage, and network operations
- Rust toolchain when building from source

## Configuration

Priosun reads TOML from `/etc/local/etc/priosun.toml`. For sample of the configuration look at
`priosun.toml.sample`.

## Daemon

Start the executor daemon:

```sh
priosund
```

The daemon listens on:

```text
/var/run/priosun/socket
```

By default, `priosund` detaches into the background. Use `--no-daemon` to keep
it in the foreground:

```sh
priosund --no-daemon
```

Use an alternate configuration file with:

```sh
priosund --config /path/to/priosun.toml
```

## Commands

Create resources:

```sh
priosun create dataset data/mydataset
priosun create volume windows --size 32G
priosun create jail myjail
priosun create jail app --set FreeBSD-set-minimal-jail
priosun create base 15.1 --set FreeBSD-set-base-jail
priosun create jail myjail --base 15.1
priosun create vm windows \
  --disk 32G \
  --iso /var/vm/Windows.iso \
  --cpus 8 \
  --memory 32G \
  --vnc-port 5900 \
  --tpm
```

`create base` installs a reusable base jail at `/var/priosun/base/<name>` and
creates its `@base` ZFS snapshot. `create jail --base <name>` clones that
snapshot into the new jail's root dataset. Base jails have no jail
configuration and are not managed as running jails.

Managed jail and VM configuration files contain a `dependencies` array. Dependencies
are started first, and cycles are rejected:

```toml
dependencies = ["network", "storage-vm"]
enabled = true
```

Dataset and volume names are relative to `zfs_pool` unless they already include
that pool name. Volumes require a ZFS size such as `32G`.

Datasets, volumes, jails, and VMs can be destroyed with the same command. Jails
and VMs also share the start and stop commands:

```sh
priosun start windows
priosun stop windows
priosun destroy vm windows
priosun destroy base 15.1
priosun login myjail
priosun dependencies jail myjail network,storage-vm
priosun enable myjail
priosun disable storage-vm
```

List resources:

```sh
priosun list datasets
priosun list volumes
priosun list jails
priosun list vms
priosun list all
```

The supported commands are `create`, `destroy`, `start`, `stop`, `login`, `enable`,
`disable`, `dependencies`, `version`, and `list`.

## Rust client

The package also builds a public `priosun` library crate. Responses are
`nvtree::Nvtree` values with either an `error` field or a request-specific
`response` field:

```rust
use priosun::protocol::{Client, Resource};

let client = Client::default();
let result = client.list(Resource::Vm)?;
```

Requests also use named nvtree fields. For example, creating a volume sends
`command`, `type`, `volume`, and numeric `size` fields rather than an unnamed
argument array.
