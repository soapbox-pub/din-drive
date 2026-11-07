# Blossom Drive

`draft` `optional`

A private file drive on Nostr. File bytes are encrypted and stored on
[Blossom](https://github.com/hzrd149/blossom) servers; the folder tree is a set
of encrypted addressable events on Nostr relays. Relays and servers only see
ciphertext and never learn names, contents, or which blob belongs to which
file.

Design goals:

- **Usable before it's synced.** A client can list any folder with one relay
  query, without downloading the rest of the drive.
- **Writes don't contend.** Each file and folder is its own event. Adding a
  file doesn't touch its folder, and two devices editing different files never
  conflict.
- **Cheap moves.** A node points at its parent, so moving or renaming a folder
  is one event no matter how much is inside it.
- **Seekable, bounded-memory encryption** for files of any size, split across
  as many blobs as server size limits require.
- **Shareable.** A folder can become its own drive with its own key, shared
  with other people, writable by several of them, and re-keyed to remove one.
- **History where it's available**, never at the cost of making current state
  slow or large.

> The kind numbers in this document (`30440`, `30441`, `30442`, `1440`, `440`)
> are provisional and will be finalized when the NIP is submitted.

## Overview

| Kind    | Name        | Type        | Encrypted with      |
| ------- | ----------- | ----------- | ------------------- |
| `30441` | Drive key   | addressable | NIP-44 to self      |
| `30440` | Node        | addressable | drive content key   |
| `1440`  | Version     | regular     | drive content key   |
| `30442` | Drive info  | addressable | drive content key   |
| `440`   | Drive invite| rumor in a NIP-59 gift wrap | NIP-59 |

A **drive** is a tree of **nodes** (files, folders, and mounts) sharing one
32-byte random **drive key** (`DK`). Every user has a home drive. Any folder can
be turned into a separate drive to share it.

## Keys and blinded tags

Two keys are derived from `DK`:

```
content_key = HMAC-SHA256(key = DK, msg = "blossom-drive/content")
tag_key     = HMAC-SHA256(key = DK, msg = "blossom-drive/tags")
```

`content_key` is used directly as a [NIP-44](44.md) v2 conversation key to
encrypt and decrypt the `content` of every node, version, and drive info event.

**Blinded tags** let relays index the tree without learning it:

```
blind(label) = lowercase hex of the first 16 bytes of HMAC-SHA256(key = tag_key, msg = label)
```

| Tag | Value                         | Meaning                                  |
| --- | ----------------------------- | ---------------------------------------- |
| `j` | `blind("drive")`              | which drive an event belongs to          |
| `y` | `blind("parent:" + parent_id)`| which folder a node is in                |
| `z` | `blind("node:" + node_id)`    | which node a version event is a copy of  |

`y` is the **only** record of a node's parent. To list a folder with id `F`, a
client queries `{"kinds": [30440], "#j": [j], "#y": [blind("parent:" + F)]}`.
The top-level folder of a drive has the reserved id `root`.

To resolve the parent of a node it already holds, a client computes
`blind("parent:" + id)` for every folder id it knows (and `root`), and looks up
the node's `y` in that table.

Using an HMAC rather than the parent's id means relays can see which nodes are
siblings, but not which node is their parent, so they can't reconstruct the
tree's shape beyond one level.

## Drive key (`30441`)

```jsonc
{
  "kind": 30441,
  "tags": [["d", "<drive id>"]],
  "content": "<NIP-44 encrypted to the author's own pubkey>"
}
```

The drive id is 16 random bytes, hex encoded. Decrypted content:

```jsonc
{
  "key": "<64-hex DK>",
  "name": "My Drive",
  "owner": "<hex pubkey of the drive's owner>",
  "role": "owner",          // "owner" | "editor" | "viewer"
  "home": true,             // the author's home drive; at most one
  "previous": ["<64-hex>"]  // earlier keys of this drive, newest first (optional)
}
```

This is the only event decrypted with the user's own Nostr key. Clients fetch it
once and derive everything else from `DK`.

Losing this event loses the drive. Clients MUST publish it to every write relay
in the user's [NIP-65](65.md) list, and SHOULD offer the user a way to export
`DK` for recovery.

## Node (`30440`)

```jsonc
{
  "kind": 30440,
  "tags": [
    ["d", "<node id>"],
    ["j", "<blind('drive')>"],
    ["y", "<blind('parent:' + parent id)>"]
  ],
  "content": "<NIP-44 with content_key>"
}
```

Node ids are 8 random bytes, hex encoded (16 characters). `root` is reserved.

The decrypted content is a JSON object with a type `t`.

### Folder

```json
{ "t": "dir", "name": "Taxes", "mtime": 1762531200 }
```

### File

```jsonc
{
  "t": "file",
  "name": "2025 return.pdf",
  "mtime": 1762534800,          // modification time, unix seconds
  "size": 148897792,            // plaintext bytes
  "mime": "application/pdf",    // optional
  "sha256": "<hex sha256 of the plaintext>",
  "enc": { "v": 1, "key": "<64-hex file key>" },
  "parts": [
    { "x": "<hex sha256 of the blob>", "size": 67125248 },
    { "x": "<hex sha256 of the blob>", "size": 47788032 }
  ],
  "servers": ["https://blossom.example.com", "https://cdn.example.org"],
  "thumb": {                    // optional
    "x": "<hex sha256 of the blob>",
    "key": "<64-hex>",
    "mime": "image/webp",
    "w": 256,
    "h": 331
  }
}
```

- `parts` is the file's ciphertext (see [File encryption](#file-encryption)),
  cut into blobs. Each blob is stored on every server in `servers`.
- `thumb` is a small preview image, encrypted the same way as a file (a single
  part), so file browsers can show thumbnails without downloading the file.
- A file MUST get a fresh random `enc.key` every time its content changes.

### Mount

```json
{ "t": "mount", "name": "Family photos", "mtime": 1762531200, "drive": "<drive id>" }
```

Shown as a folder whose contents are the root of another drive, whose key the
user holds in a `30441` with that drive id.

### Tombstone

```json
{ "t": "gone", "mtime": 1765192000 }
```

A tombstone keeps the `y` tag of the node it replaces, so clients listing that
folder see the deletion.

### Trash

Any node may carry `"trashed": <unix seconds>`. A trashed node and everything
beneath it is hidden from the tree and shown in the client's Trash, from where it
can be restored by republishing without the field. Clients SHOULD replace
trashed nodes with tombstones after 30 days, deleting their blobs.

### Common rules

- `name` MUST NOT be empty, `.` or `..`, and MUST NOT contain `/` or NUL.
- Clients MAY add fields, and MUST preserve fields they don't understand when
  republishing a node.
- A rename, move, edit, trash, or restore is a republish of the same `d` with
  a newer `created_at` (strictly greater than the one it replaces).
- Clients SHOULD omit identifying tags (such as `client`) from drive events.

## File encryption

Format version 1 (`"enc": {"v": 1}`) is a STREAM construction, the same shape
as [age](https://age-encryption.org/v1)'s payload:

- The plaintext is split into **segments** of 65536 bytes. Every segment but the
  last is exactly that size; the last is shorter, and may be empty. An empty
  file is a single empty last segment.
- Each segment is sealed with AES-256-GCM under the file key, with no
  associated data, and a 12-byte nonce: an 11-byte big-endian segment counter
  starting at 0, followed by `0x01` for the last segment and `0x00` for every
  other.
- The ciphertext is the concatenation of the sealed segments (each 16 bytes
  longer than its plaintext).

The ciphertext is then cut into **parts**, one blob each. Every part except the
last MUST contain a whole number of sealed segments. Clients SHOULD use 1024
segments per part (65,552 KiB), and MAY use fewer to fit server size limits.

Because segment boundaries are fixed, a reader can map any plaintext byte range
to a part and a byte range within it, fetch just that range with an HTTP
`Range` request, and decrypt it on its own. The counter binds each segment to
its position, and the last-segment flag detects truncation.

Clients MUST verify a blob's sha256 when they download it whole. Segments read
by range are authenticated by their GCM tags.

## Version (`1440`)

Every time a client publishes a node, it SHOULD also publish a version event:

```jsonc
{
  "kind": 1440,
  "tags": [
    ["j", "<blind('drive')>"],
    ["z", "<blind('node:' + node id)>"]
  ],
  "content": "<NIP-44 with content_key>"
}
```

whose content is the node's content plus its id and parent tag at that moment:

```json
{ "d": "<node id>", "y": "<y tag>", "t": "file", "name": "…", "…": "…" }
```

Version events are regular events, so relays keep all of them instead of
replacing them. They make history available on any relay:

- A file's history: `{"kinds": [1440], "#j": [j], "#z": [blind("node:" + id)]}`.
- The drive at time T: for each node id, the newest version at or before T, with
  the tree rebuilt from its `y` values.

Relays that keep superseded addressable events can serve history from `30440`
directly as well.

History is best-effort. Clients MAY thin out old versions (for example: all
versions for a day, one a day for a month, one a week after that) by deleting
their version events with [NIP-09](09.md) and deleting their blobs. Clients MUST
NOT delete a blob referenced by any node's current state, SHOULD keep the blobs
of replaced and deleted versions for a retention period, and MUST NOT assume
history is complete. A version whose blobs are gone is shown as unavailable.

## Resolving the tree

A node's identity is its drive and `d`, across authors. For each `d`, clients
use the event with the highest `created_at` from an **authorized author**; on a
tie, the lowest event id. A home drive's only authorized author is its owner.
A shared drive's authorized authors are its owner and its members with the
`editor` role, as listed in its drive info. Events that fail to decrypt, or
whose author isn't authorized, are ignored.

Concurrent changes resolve as follows:

- **Same node changed on two devices:** newest wins. An edit newer than a
  tombstone brings the node back.
- **Missing parent, a parent that isn't a folder, or a cycle** (two folders
  moved into each other at the same time): the node is shown at the root of
  its drive. Nothing is lost, and moving it anywhere fixes it.
- **Duplicate names in one folder:** the node with the lowest `d` keeps its
  name. The others are shown with ` (<first 6 characters of d>)` inserted
  before the extension, e.g. `report (9f2c6a).pdf`, until renamed.

## Sharing

A folder is shared by turning it into its own drive:

1. Generate a new `DK` and drive id, and publish a `30441` for it.
2. Republish the folder's descendants under the new drive's key, tags, and
   `root`. File blobs are not re-uploaded; each file keeps its own key.
3. Tombstone the old nodes, and put a mount node where the folder was.
4. Publish drive info, and invite members.

### Drive info (`30442`)

```jsonc
{
  "kind": 30442,
  "tags": [["d", "<drive id>"], ["j", "<blind('drive')>"]],
  "content": "<NIP-44 with content_key>"
}
```

```jsonc
{
  "name": "Family photos",
  "owner": "<hex pubkey>",
  "members": [
    { "pubkey": "<hex>", "role": "editor" },
    { "pubkey": "<hex>", "role": "viewer" }
  ],
  "relays": ["wss://relay.example.com"],
  "servers": ["https://blossom.example.com"]
}
```

Only drive info signed by the owner (as named in the user's `30441` for the
drive) is valid. Nodes and versions of a shared drive are published to its
`relays`.

### Invite (`440`)

A [NIP-59](59.md) gift wrap to each member, containing a rumor:

```jsonc
{
  "kind": 440,
  "content": "{\"drive\":\"<drive id>\",\"key\":\"<64-hex DK>\",\"owner\":\"<hex>\",\"name\":\"Family photos\",\"role\":\"editor\",\"relays\":[\"wss://…\"]}"
}
```

A member who accepts publishes their own `30441` with the drive id and key, and
may add a mount node for it to their home drive.

### Removing a member

1. Generate a new `DK` (same drive id). This changes `j`, `y`, and `z`.
2. Republish every node of the drive under the new key. Nodes written by other
   members are republished by the owner.
3. Publish new drive info, and send new invites to the remaining members. Their
   clients move the old key into `previous`, which keeps history readable.
4. Delete the old events.

A removed member keeps the keys of files that existed when they were removed,
and those blobs remain on the servers. Everything added or changed afterwards is
protected. Clients MAY offer to re-encrypt and re-upload every file for full
revocation.

## Storage

- Drive events are published to the user's NIP-65 write relays (for a shared
  drive, to its `relays`).
- Blobs are uploaded with [BUD-02](https://github.com/hzrd149/blossom/blob/master/buds/02.md)
  and [BUD-11](https://github.com/hzrd149/blossom/blob/master/buds/11.md)
  authorization to the servers in the user's
  [kind 10063](https://github.com/hzrd149/blossom/blob/master/buds/03.md) list.
  Clients SHOULD store each blob on at least two servers.
- Blobs are fetched per [BUD-01](https://github.com/hzrd149/blossom/blob/master/buds/01.md),
  trying each server in `servers`.

## Privacy

Relays can see:

- that the author uses a drive (from the kinds);
- how many nodes each drive has, which nodes are siblings, and roughly how big
  each node's content is;
- when nodes change.

Blossom servers can see opaque blobs, their sizes, and who uploaded them.
Neither can see names, folder structure, contents, or the link between a blob
and a known file. File keys are random, so identical files produce unrelated
ciphertexts.
