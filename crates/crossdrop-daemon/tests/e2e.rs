//! Two daemons on loopback, driven only through the public SDK: pairing,
//! file transfer, permissions and a third-party extension.

use std::path::Path;
use std::time::Duration;

use crossdrop_daemon::{start, Handle, Options};
use crossdrop_sdk::{files::send_files, Client, Paths};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn daemon(dir: &Path, name: &str) -> Handle {
    start(Options {
        home: Some(dir.join(name).join("home")),
        name: Some(name.to_string()),
        port: Some(0),
        mdns: Some(false),
        download_dir: Some(dir.join(name).join("downloads")),
    })
    .await
    .unwrap()
}

async fn client(h: &Handle) -> Client {
    Client::connect(Paths::resolve(Some(h.paths.home.clone())).unwrap()).await.unwrap()
}

/// Pairs `a` with `b` the way two users would: A starts, both compare codes, B accepts.
async fn pair(a: &Handle, b: &Handle) {
    let mut ca = client(a).await;
    let (pair_id, code, peer_id, _) = ca.pair_start(&format!("127.0.0.1:{}", b.port)).await.unwrap();
    assert_eq!(peer_id, b.id.to_string());

    let confirm = tokio::spawn(async move { ca.pair_confirm(&pair_id, true).await.unwrap() });

    let mut cb = client(b).await;
    let request = loop {
        let mut requests = cb.pair_requests().await.unwrap();
        if let Some(r) = requests.pop() {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(request.sas, code, "both devices must show the same code");
    assert_eq!(request.peer_id, a.id.to_string());
    cb.pair_answer(&request.pair_id, true).await.unwrap();

    let (paired, _, peer_name) = confirm.await.unwrap();
    assert!(paired);
    assert_eq!(peer_name, "bravo");
}

#[tokio::test]
async fn pair_send_files_and_extension() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;

    // Unpaired devices cannot open anything.
    let mut ca = client(&a).await;
    let err = ca.open_channel(&b.id.to_string(), "crossdrop/files").await.unwrap_err();
    assert!(err.to_string().contains("not paired"), "{err}");

    pair(&a, &b).await;

    // Both sides now list each other as paired.
    let devices = client(&a).await.devices().await.unwrap();
    assert!(devices.iter().any(|d| d.paired && d.name == "bravo"));
    let devices = client(&b).await.devices().await.unwrap();
    assert!(devices.iter().any(|d| d.paired && d.name == "alpha"));

    // Files: A sends two files, by device name.
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    std::fs::write(src.join("note.txt"), "bonjour").unwrap();
    let mut stream = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    let mut progress = |_: &str, _: u64, _: u64| {};
    let n = send_files(&mut stream, &[src.join("big.bin"), src.join("note.txt")], &mut progress).await.unwrap();
    assert_eq!(n, 2);
    let downloads = dir.path().join("bravo").join("downloads");
    assert_eq!(std::fs::read(downloads.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(downloads.join("note.txt")).unwrap(), "bonjour");

    // Extension: B serves "test/echo", but A is not allowed to use it yet.
    let mut reg = client(&b).await.register("test/echo").await.unwrap();
    let server = tokio::spawn(async move {
        let incoming = reg.next().await.unwrap().unwrap();
        assert_eq!(incoming.service, "test/echo");
        let mut s = incoming.accept().await.unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        s.write_all(&buf).await.unwrap();
        s.shutdown().await.unwrap();
        assert_eq!(&buf, b"ping");
    });

    let err = client(&a).await.open_channel("bravo", "test/echo").await.unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");

    client(&b).await.set_permission("alpha", "test/echo", true).await.unwrap();
    let mut s = client(&a).await.open_channel("bravo", "test/echo").await.unwrap();
    s.write_all(b"ping").await.unwrap();
    let mut back = [0u8; 4];
    s.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"ping");
    server.await.unwrap();

    // Unpairing revokes access.
    client(&b).await.unpair("alpha").await.unwrap();
    let err = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap_err();
    assert!(err.to_string().contains("not paired"), "{err}");

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn rejected_pairing_trusts_no_one() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;

    let mut ca = client(&a).await;
    let (pair_id, _, _, _) = ca.pair_start(&format!("127.0.0.1:{}", b.port)).await.unwrap();
    let confirm = tokio::spawn(async move { ca.pair_confirm(&pair_id, true).await.unwrap() });
    let mut cb = client(&b).await;
    let request = loop {
        if let Some(r) = cb.pair_requests().await.unwrap().pop() {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    cb.pair_answer(&request.pair_id, false).await.unwrap();
    let (paired, _, _) = confirm.await.unwrap();
    assert!(!paired);
    assert!(client(&a).await.devices().await.unwrap().iter().all(|d| !d.paired));
    assert!(client(&b).await.devices().await.unwrap().iter().all(|d| !d.paired));

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn second_daemon_on_same_home_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let again = start(Options {
        home: Some(dir.path().join("alpha").join("home")),
        port: Some(0),
        mdns: Some(false),
        ..Default::default()
    })
    .await;
    assert!(again.is_err());
    a.shutdown();
}
