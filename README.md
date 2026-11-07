# Din Drive

An encrypted drive on Nostr and Blossom, mounted into the filesystem with
FUSE. Files are encrypted on your machine and stored on
[Blossom](https://github.com/hzrd149/blossom) servers; the folder tree is kept
as encrypted events on your Nostr relays. Relays and servers see only
ciphertext.

The protocol is **Blossom Drive**, specified in [SPEC.md](SPEC.md). Din Drive is
its implementation for Linux.

## Using it

```sh
din-drive init          # create your drive, or find the one you have
din-drive mount         # mount at ~/Drive and keep it in sync until stopped
```

While mounted, `~/Drive` works like any folder. Changes from your other devices
show up within seconds, and files you save are uploaded in the background a
couple of seconds after they're closed. Deleting moves things to the trash;
trashed items are deleted for good after 30 days.

Other commands work without mounting:

```sh
din-drive ls [folder]
din-drive put FILE [folder]
din-drive get PATH OUT
din-drive mkdir PATH
din-drive rm PATH
din-drive trash
din-drive empty-trash
din-drive status
```

### Keys

Din Drive signs and decrypts through the NIP-5F signer socket that
[nostrd](https://git.shakespeare.diy) provides (`$DIN_NOSTRD_SOCKET`, else
`~/.local/share/nostr/signer.sock`), so it never holds your Nostr key. For
testing, `--nsec-file` signs with a key from a file instead.

### Relays and servers

By default the drive lives on your NIP-65 write relays and your kind 10063
Blossom servers, looked up on a few well-known relays. Override them with
`--relay` and `--server` (repeatable), or in `~/.config/din-drive/config.json`:

```json
{
  "relays": ["wss://relay.example.com"],
  "servers": ["https://blossom.example.com"],
  "mountpoint": "/home/me/Drive",
  "cache_limit_gb": 10
}
```

## How it works

- **Metadata is local.** Every node is kept in SQLite
  (`~/.local/share/din-drive/<pubkey>/drive.db`), so listing folders is instant
  and works offline. The daemon fetches what changed since its last sync, and
  follows live changes.
- **Contents load on demand.** Reading a file fetches only the 64 KiB segments
  it needs (plus a little read-ahead) with HTTP range requests, decrypts them,
  and caches them, up to `cache_limit_gb`.
- **Writes go to a local copy first.** When a file has been closed for two
  seconds, it's encrypted with a fresh key, uploaded to every server, and its
  node is published along with a version event. The queue survives restarts
  and going offline.
- **Editors' atomic saves keep the file's identity.** Renaming a file over
  another keeps the target's node id, so its history continues.

## Status

Working: the spec's home drive (folders, files, multi-part encryption, blinded
tags, trash, version events), the FUSE mount, and sync between devices.

Not yet: shared drives and invites, mount nodes, thumbnails, browsing history,
and Din integration (systemd service, Files sidebar, Settings).

The drive key is cached unencrypted in the local state database, next to the
cache of decrypted files; both rely on disk encryption.

## Development

```sh
nix develop      # or have cargo and a C compiler on your PATH
cargo test       # unit tests, two-device sync, and a real FUSE mount
```

The tests run a relay and a Blossom server in-process. The FUSE test needs
`/dev/fuse` and `fusermount3`, and is skipped without them.
