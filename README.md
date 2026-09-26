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
priosun create base jail --version 15.1 15.1 --set FreeBSD-set-base-jail
priosun create jail --version 15.1 myjail --base 15.1
priosun create vm windows \
  --disk 32G \
  --os freebsd \
  --iso /var/vm/Windows.iso \
  --cpus 8 \
  --memory 32G \
  --vnc-port 5900 \
  --tpm

priosun create vm freebsd15 \
  --disk 32G \
  --memory 4G \
  --os freebsd \
  --version 15.1 \
  --cloud-init

priosun create vm ubuntu24 \
  --disk 32G \
  --memory 4G \
  --os ubuntu \
  --version 24.04 \
  --cloud-init

priosun create vm fedora44 \
  --disk 32G \
  --memory 4G \
  --os fedora \
  --version 44-1.7 \
  --cloud-init

priosun create vm debian13 \
  --disk 32G \
  --memory 4G \
  --os debian \
  --version 13 \
  --cloud-init

priosun init web
priosun init --provisioner ansible --container vm web-vm
cd web && priosun up
cd web && priosun down
cd web && priosun destroy
```

`create base jail` installs a reusable base jail at `/var/priosun/base/<name>` and
creates its `@base` ZFS snapshot. `create jail --base <name>` clones that
snapshot into the new jail's root dataset. Base jails have no jail
configuration and are not managed as running jails.

`init <service>` creates a service directory with a `service.toml` manifest.
Services default to running in a jail; use `--container vm` to select a VM.
The manifest also contains `develop = false`; setting it to `true` mounts the
service directory at `/usr/src` and disables automatic startup for that jail.
Provisioners are optional. The first supported provisioner is Ansible:

```sh
priosun init --provisioner ansible myservice
```

This creates the Ansible requirements, playbook, inventory, group variables, and
roles directories inside the service directory.

Run `priosun up` from an initialized jail service directory to create and start
its jail through the Priosun daemon. Run `priosun down` in the same directory to
stop it, or `priosun destroy` to stop and remove it. VM services are not
supported by `up`, `down`, or manifest-based `destroy` yet.

The optional `--version MAJOR.MINOR` selects the PkgBase release and records
the matching release metadata for the jail, such as `15.0` or `15.1`.

VM creation requires `--os`; `freebsd`, `ubuntu`, `fedora`, and `debian` are supported. VMs may
enable cloud-init with the flag `--cloud-init`. The selected OS determines the
cloud image format and import process. Both currently get a per-VM FAT32
`cidata` volume under `/var/priosun/seed`, attached as an additional NVMe
device.

FreeBSD cloud images are downloaded as compressed raw images. Ubuntu, Fedora, and
Debian cloud images are downloaded as bootable QCOW2 images and converted to raw
before being written to the VM's NVMe zvol. Images are cached in
`/var/priosun/images` and reused for later VM creations with the same release
and architecture. Ubuntu versions may be specified as a release number such as
`26.04` or its codename, such as `resolute`. Fedora versions use the release
format `MAJOR-RELEASE`, such as `44-1.7`. Debian versions may be specified as
`13`, `12`, or `11`, or by codename (`trixie`, `bookworm`, or `bullseye`).

Ubuntu, Fedora, and Debian VM creation requires the `qemu-tools` package, which provides
`qemu-img`.

Ubuntu cloud-init VMs receive serial-console configuration automatically:
GRUB is configured for `ttyS0`, `serial-getty@ttyS0.service` is enabled, and
bhyve is started with `-l com1,stdio` connected to a daemon-owned PTY. Use
`priosun attach <name>` to connect to it.

Fedora cloud-init VMs receive equivalent serial-console configuration through
`grubby` and `serial-getty@ttyS0.service`; their serial console is also
available through `priosun attach <name>`.

Debian cloud-init VMs receive equivalent serial-console configuration through
GRUB and `serial-getty@ttyS0.service`; their serial console is also available
through `priosun attach <name>`.

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
priosun start windows --attach
priosun stop windows
priosun destroy vm windows
priosun destroy base jail 15.1
priosun attach myjail
priosun dependencies jail myjail network,storage-vm
priosun enable myjail
priosun disable storage-vm
priosun network-init
```

List resources:

```sh
priosun list datasets
priosun list volumes
priosun list jails
priosun list vms
priosun list all
```

The supported commands are `create`, `destroy`, `start`, `stop`, `attach`, `up`,
`down`, `enable`, `disable`, `dependencies`, `network-init`, `version`, and `list`.

The `network-init` command configures the bridge in `/etc/rc.conf` and activates
it immediately, then creates the managed `network` jail when necessary, installs Kea
DHCP and Knot DNS, writes their configuration, and starts the jail. Its address
is controlled by `network_ip` and `network_ip6`; the bridge gateway is supplied
by `bridge_ip` and `bridge_ip6`. The host's system hostname is used as the DNS
domain for the managed network. It also configures the host's `local_unbound`
service and `resolvconf` to forward the managed domain and reverse zones to Knot in the network jail. It
also enables the host NFS server and exports `projects_dir` to the jail/VM
network for development mounts. `projects_dir` is the host-side root directory
containing service source trees; a VM service with `develop = true` uses this
export to access its source directory at `/usr/src`. It defaults to
`/var/empty`, which provides an empty export until a projects directory is
configured.
Set `use_ipv4` or `use_ipv6` to `false` to disable that address family; at
least one must remain enabled.

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
