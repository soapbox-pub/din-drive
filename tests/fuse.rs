//! The drive mounted with FUSE, driven with ordinary file operations.
//! Needs /dev/fuse and fusermount3; skipped otherwise.

mod common;

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use common::*;
use din_drive::keys::ROOT;

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fuse_file_operations() {
    if !fuse_available() {
        eprintln!("skipping: no /dev/fuse");
        return;
    }
    let env = env().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mnt = tempfile::tempdir().unwrap();
    let a = env.device(dir_a.path(), true).await;

    let fs_impl = din_drive::fs::DriveFs::new(a.clone(), tokio::runtime::Handle::current());
    let mut config = fuser::Config::default();
    config.mount_options = vec![fuser::MountOption::FSName("din-drive-test".into())];
    config.n_threads = Some(2);
    let session = fuser::spawn_mount(fs_impl, mnt.path(), &config).unwrap();

    let root = mnt.path().to_path_buf();
    let big = data(1_000_000, 3);
    let big2 = big.clone();
    tokio::task::spawn_blocking(move || {
        fs::create_dir(root.join("Photos")).unwrap();
        fs::write(root.join("Photos/a.txt"), b"first").unwrap();
        assert_eq!(fs::read(root.join("Photos/a.txt")).unwrap(), b"first");

        // Append.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(root.join("Photos/a.txt"))
            .unwrap();
        f.write_all(b" second").unwrap();
        drop(f);
        assert_eq!(
            fs::read_to_string(root.join("Photos/a.txt")).unwrap(),
            "first second"
        );

        // Atomic save: write a temp file, rename it over the original.
        fs::write(root.join("Photos/.a.txt.tmp"), b"replaced").unwrap();
        fs::rename(root.join("Photos/.a.txt.tmp"), root.join("Photos/a.txt")).unwrap();
        assert_eq!(fs::read(root.join("Photos/a.txt")).unwrap(), b"replaced");

        // A larger file, read back with a seek.
        fs::write(root.join("big.bin"), &big2).unwrap();
        let mut f = fs::File::open(root.join("big.bin")).unwrap();
        f.seek(SeekFrom::Start(500_000)).unwrap();
        let mut buf = vec![0u8; 1000];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf, big2[500_000..501_000]);

        // Listing, truncating, removing.
        let mut names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Photos", "big.bin"]);
        fs::write(root.join("scratch"), b"tmp").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("scratch"))
            .unwrap()
            .set_len(1)
            .unwrap();
        assert_eq!(fs::read(root.join("scratch")).unwrap(), b"t");
        fs::remove_file(root.join("scratch")).unwrap();
        assert!(fs::create_dir(root.join("Photos")).is_err());
        assert!(fs::remove_dir(root.join("Photos")).is_err(), "not empty");
        fs::create_dir(root.join("Empty")).unwrap();
        fs::remove_dir(root.join("Empty")).unwrap();
        assert!(!root.join("Empty").exists());
    })
    .await
    .unwrap();

    // Let the filesystem release its handles, then upload and publish.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    a.flush().await.unwrap();

    let b = env.device(dir_b.path(), false).await;
    b.sync().await.unwrap();
    let photos = b.lookup(ROOT, "Photos").unwrap();
    let f = b.lookup(&photos, "a.txt").unwrap();
    assert_eq!(read_all(&b, &f).await, b"replaced");
    let bigd = b.lookup(ROOT, "big.bin").unwrap();
    assert_eq!(read_all(&b, &bigd).await, big);
    assert!(b.lookup(ROOT, "scratch").is_none());
    assert_eq!(b.children(ROOT).len(), 2);

    drop(session);
}
