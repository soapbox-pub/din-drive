//! Two devices on one account, syncing through a relay and a Blossom server.

mod common;

use common::*;
use din_drive::keys::ROOT;
use din_drive::stream::SEGMENT;

#[tokio::test]
async fn two_devices_sync() {
    let env = env().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A creates the drive and some content.
    let a = env.device(dir_a.path(), true).await;
    let docs = a.mkdir(ROOT, "Docs").unwrap();
    let hello = a.create_file(&docs, "hello.txt").unwrap();
    write_file(&a, &hello, b"hello, drive").await;
    let big_bytes = data(5 * SEGMENT as usize + 123, 7);
    let big = a.create_file(ROOT, "big.bin").unwrap();
    write_file(&a, &big, &big_bytes).await;
    a.flush().await.unwrap();

    // The server only ever sees ciphertext.
    for blob in env.blobs.lock().unwrap().values() {
        assert!(!blob.windows(5).any(|w| w == b"hello"));
    }

    // Device B finds the drive through its key event and reads it.
    let b = env.device(dir_b.path(), false).await;
    assert_eq!(b.drive_id, a.drive_id);
    b.sync().await.unwrap();
    let docs_b = b.lookup(ROOT, "Docs").expect("folder synced");
    let hello_b = b.lookup(&docs_b, "hello.txt").expect("file synced");
    assert_eq!(hello_b, hello);
    assert_eq!(read_all(&b, &hello_b).await, b"hello, drive");

    // A range in the middle of a multi-segment file, fetched on demand.
    let mid = b.read(&big, 3 * SEGMENT - 10, 100).await.unwrap();
    assert_eq!(
        mid,
        &big_bytes[(3 * SEGMENT - 10) as usize..(3 * SEGMENT + 90) as usize]
    );
    assert_eq!(read_all(&b, &big).await, big_bytes);

    // B renames and moves; A sees it.
    b.rename(&hello_b, ROOT, "greeting.txt").unwrap();
    b.flush().await.unwrap();
    a.sync().await.unwrap();
    assert_eq!(a.lookup(ROOT, "greeting.txt").as_deref(), Some(hello.as_str()));
    assert!(a.lookup(&docs, "hello.txt").is_none());

    // A edits the file; B gets the new version.
    write_file(&a, &hello, b"edited").await;
    a.flush().await.unwrap();
    b.sync().await.unwrap();
    assert_eq!(read_all(&b, &hello).await, b"edited");
}

#[tokio::test]
async fn atomic_save_keeps_identity() {
    let env = env().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = env.device(dir_a.path(), true).await;

    let doc = a.create_file(ROOT, "notes.txt").unwrap();
    write_file(&a, &doc, b"v1").await;
    a.flush().await.unwrap();

    // An editor writes a temp file and renames it over the original.
    let tmp = a.create_file(ROOT, ".notes.txt.swp").unwrap();
    write_file(&a, &tmp, b"v2").await;
    a.rename(&tmp, ROOT, "notes.txt").unwrap();
    assert_eq!(a.lookup(ROOT, "notes.txt").as_deref(), Some(doc.as_str()));
    assert!(a.lookup(ROOT, ".notes.txt.swp").is_none());
    assert_eq!(read_all(&a, &doc).await, b"v2");
    a.flush().await.unwrap();

    let b = env.device(dir_b.path(), false).await;
    b.sync().await.unwrap();
    assert_eq!(b.children(ROOT).len(), 1, "the temp file never left device A");
    assert_eq!(read_all(&b, &doc).await, b"v2");
}

#[tokio::test]
async fn trash_and_purge() {
    let env = env().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = env.device(dir_a.path(), true).await;

    let folder = a.mkdir(ROOT, "Old").unwrap();
    let f = a.create_file(&folder, "x.txt").unwrap();
    write_file(&a, &f, b"bye").await;
    a.flush().await.unwrap();
    assert_eq!(env.blobs.lock().unwrap().len(), 1);

    a.remove(&f, false).unwrap();
    a.remove(&folder, true).unwrap();
    assert!(a.lookup(ROOT, "Old").is_none());
    assert_eq!(a.trash().len(), 1);
    a.flush().await.unwrap();

    let b = env.device(dir_b.path(), false).await;
    b.sync().await.unwrap();
    assert!(b.children(ROOT).is_empty());
    assert_eq!(b.trash().len(), 1);
    // Restoring brings the folder back; its file was trashed on its own and
    // stays in the trash.
    b.restore(&folder).unwrap();
    assert!(b.lookup(ROOT, "Old").is_some());
    assert!(b.children(&folder).is_empty());
    assert_eq!(b.trash(), vec![f.clone()]);
    b.flush().await.unwrap();

    a.sync().await.unwrap();
    assert!(a.lookup(ROOT, "Old").is_some());
    a.remove(&folder, true).unwrap();
    assert_eq!(a.empty_trash(None).await.unwrap(), 2);
    a.flush().await.unwrap();
    assert!(env.blobs.lock().unwrap().is_empty(), "purging deletes the blobs");

    b.sync().await.unwrap();
    assert!(b.trash().is_empty());
}
