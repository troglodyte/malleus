# malleus

`malleus` is a Rust CLI for running and managing an Incus-focused Linux VM from macOS.

## What it does

Current implementation scope:

- Provides a CLI surface for:
  - `start`
  - `stop`
  - `status`
  - `delete`
  - `mount`
  - `unmount`
- Reads optional VM config from `~/.malleus/config.toml` (or a custom state dir).
- On `start`, prepares bootstrap artifacts in the state dir:
  - PKI material (`client.crt`, `client.key`, `server.crt`, `server.key`)
  - cloud-init files (`user-data`, `meta-data`)
  - generated `vfkit.args`

> Note: this is currently scaffolding/bootstrap wiring. The command surface is in place, but full VM lifecycle execution and remote/route reconciliation are not fully wired yet.

## Requirements

### Required now

- Rust toolchain (Cargo), edition `2024` compatible

### Expected for full macOS runtime flow

- macOS host
- `vfkit`
- `incus` client
- Permission to manage host routes (typically via `sudo`)

## Build

```bash
cargo build
```

Show CLI help:

```bash
cargo run -- --help
```

## Install

From the project root:

```bash
cargo install --path .
```

This installs `malleus` to `~/.cargo/bin`.

Add Cargo's bin directory to your `PATH`.

For `zsh`:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

For `bash`:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

Verify:

```bash
which malleus
malleus --help
```

## Usage

Default state directory is `~/.malleus`.

```bash
malleus start
malleus status
malleus stop
malleus delete
```

Use a custom state directory:

```bash
malleus --state-dir /path/to/state start
```

Register and remove host-share tags:

```bash
malleus mount /path/to/host/dir
malleus mount /path/to/host/dir my-tag
malleus unmount my-tag
```

Mount/share names must contain only letters, numbers, `-`, `_`, or `.`.

## Using with Incus

`malleus` is designed to keep the normal `incus` CLI front-and-center.

Current state of the implementation:

- `malleus start` prepares bootstrap artifacts (`user-data`, `meta-data`, `vfkit.args`, and PKI files).
- Full VM launch, remote wiring, and route reconciliation are still in progress.

### Manual flow (today)

1. Generate bootstrap material:

   ```bash
   malleus start
   ```

2. Start your Linux VM with `vfkit` using the generated `~/.malleus/vfkit.args`.

3. In the guest, cloud-init uses the generated data to:
   - install Incus,
   - run `incus admin init --preseed`,
   - trust the generated `malleus` client certificate.

4. On macOS, point an `incus` client config at the generated client cert/key, then add the remote:

   ```bash
   export INCUS_CONF="$HOME/.config/incus-malleus"
   mkdir -p "$INCUS_CONF"
   cp ~/.malleus/pki/client.crt "$INCUS_CONF/client.crt"
   cp ~/.malleus/pki/client.key "$INCUS_CONF/client.key"

   incus remote add malleus https://<vm-ip>:8443 --accept-certificate
   incus remote switch malleus
   incus list
   ```

Replace `<vm-ip>` with the guest VM IP address.

After that, use stock `incus` commands (`incus launch`, `incus exec`, `incus list`, etc.) against the `malleus` remote.

## Configuration

Optional file: `~/.malleus/config.toml`

Example:

```toml
cpus = 4
memory_mib = 4096
root_disk_gib = 20
pool_disk_gib = 50
```

Validation rules:

- `cpus >= 1`
- `memory_mib >= 1`
- `root_disk_gib >= 1`
- `pool_disk_gib >= 1`

## State directory artifacts

After `start`, the tool currently writes:

- `pki/client.crt`
- `pki/client.key`
- `pki/server.crt`
- `pki/server.key`
- `user-data`
- `meta-data`
- `vfkit.args`

## Development verification

```bash
cargo test
cargo check --target aarch64-apple-darwin
```