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

`mount` never creates a drive: if the signer is locked or no relay answers, it
fails and can be retried, and with no drive at all it exits with status 3.

### On NixOS

```nix
{
  inputs.din-drive.url = "git+https://git.shakespeare.diy/npub10qdp2fc9ta6vraczxrcs8prqnv69fru2k6s2dj48gqjcylulmtjsg9arpj/din-drive.git";
  # …
  modules = [
    din-drive.nixosModules.default
    { services.din-drive = { enable = true; users = [ "alice" ]; }; }
  ];
}
```

This mounts each listed user's drive at `~/Drive` (`mountpoint`) with a
systemd user service, once they've run `din-drive init`
(`systemctl --user restart din-drive` starts it right away).

### Control socket

While mounted, the drive answers JSON requests, one per line, at
`$XDG_RUNTIME_DIR/din-drive.sock`: `{"method":"status"}`, `trash`,
`restore` (`{"params":{"id":…}}`) and `empty_trash`, replying
`{"result":…}` or `{"error":"…"}`. `din-drive status --json`, `trash`,
`restore` and `empty-trash` go through it when the drive is running.

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
tags, trash, version events), the FUSE mount, sync between devices, and the
service and control socket that DinOS runs it with (Files' sidebar and
Settings → Drive).

Not yet: shared drives and invites, mount nodes, thumbnails, and browsing
history.

The drive key is cached unencrypted in the local state database, next to the
cache of decrypted files; both rely on disk encryption.

## How it compares

Several projects keep files on Blossom with an index on Nostr. Blossom Drive
takes the best idea from each and fixes what holds them back:

| | Index | Folders | File encryption | Large files | Notes |
| --- | --- | --- | --- | --- | --- |
| **Blossom Drive** (this) | One encrypted event per file or folder | Nodes, parent in a blinded tag | Random key per version; 64 KiB AES-GCM segments | Many blobs per file, seekable | Folders load on demand; per-drive keys for sharing; version events for history |
| [hzrd149/blossom-drive](https://github.com/hzrd149/blossom-drive) (kinds 30563/30564) | One replaceable event for the whole drive | Paths in that event | Password-based (encrypted drives) | One blob | Abandoned: every change rewrites the drive, which doesn't scale |
| [Formstr Drive](https://github.com/formstr-hq/formstr-drive) (NIP-FS, kind 34578) | One encrypted event per file | A `folder` path string on each file | AES-GCM with a key per file, derived through NIP-44's labels | Chunk list | Renaming a folder rewrites every file in it; no empty folders. Good ideas we kept: thumbnails, multi-blob files |
| [Hanami](https://github.com/Letdown2491/hanami) ([NIP proposal #2420](https://github.com/nostr-protocol/nips/issues/2420), kind 30630) | One encrypted event per file or folder | Nodes, parent inside the encrypted content | Random key per file; 64 KiB AES-GCM segments | One blob | The closest design. Listing a folder means fetching the whole drive; no sharing, cycles or name conflicts in the spec; one blob can exceed server limits |
| [hashtree](https://github.com/mmalmi/hashtree) | A Merkle tree; one root event per tree | Tree nodes | Convergent (key from the content's hash) | 2 MiB chunks, each a blob | Free snapshots and shareable subtrees, but identical files encrypt identically (a server can tell who has a known file), and one root per tree means two devices' changes collide |
| [blossomfs](https://github.com/Amperstrand/blossomfs) | Blobs by hash (BUD-12 listing) | Hash-first views | None | One blob | A FUSE view of Blossom servers rather than a drive |
| [zerodrive](https://github.com/portbuster1337/zerodrive) | Encrypted manifests in kind 30078 | Paths in the manifest | AES-GCM, keys from a mnemonic | 40 MiB shards | Its own identity and format; manifests split across events as they grow |

The design choices behind the table are explained in [SPEC.md](SPEC.md).

## Development

```sh
nix develop      # or have cargo and a C compiler on your PATH
cargo test       # unit tests, two-device sync, and a real FUSE mount
```

The tests run a relay and a Blossom server in-process. The FUSE test needs
`/dev/fuse` and `fusermount3`, and is skipped without them.

## License

[MIT](LICENSE).
