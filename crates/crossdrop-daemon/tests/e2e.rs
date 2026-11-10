//! Two daemons on loopback, driven only through the public SDK: pairing,
//! file transfer, permissions and a third-party extension.

use std::path::Path;
use std::time::Duration;

use crossdrop_daemon::{start, Handle, Options};
use crossdrop_sdk::files::{send_files, Declined, SendEvent};
use crossdrop_sdk::{Client, Paths};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn daemon(dir: &Path, name: &str) -> Handle {
    start(Options {
        home: Some(dir.join(name).join("home")),
        name: Some(name.to_string()),
        port: Some(0),
        mdns: Some(false),
        download_dir: Some(dir.join(name).join("downloads")),
        clipboard_sync: Some(false),
        desktop_dir: Some(dir.join(name).join("desktop")),
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

    // B stores A once A has read the answer, a moment after A is done.
    let a_id = a.id.to_string();
    for _ in 0..250 {
        if cb.devices().await.unwrap().iter().any(|d| d.id == a_id && d.paired) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("bravo never stored the pairing");
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

    // Files: A sends a file and a folder, by device name. B accepts by hand.
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("photos").join("2026")).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    std::fs::write(src.join("photos").join("2026").join("a.jpg"), "jpeg").unwrap();
    let accept = tokio::spawn(answer_next_offer(client(&b).await, true));
    let mut stream = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    let mut events = |_: SendEvent<'_>| {};
    let report = send_files(&mut stream, &[src.join("big.bin"), src.join("photos")], &mut events).await.unwrap();
    assert_eq!(report.received, 2);
    let offer = accept.await.unwrap();
    assert_eq!((offer.files, offer.peer_name.as_str()), (2, "alpha"));
    let downloads = dir.path().join("bravo").join("downloads");
    assert_eq!(std::fs::read(downloads.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(downloads.join("photos/2026/a.jpg")).unwrap(), "jpeg");

    // Declined transfer.
    let decline = tokio::spawn(answer_next_offer(client(&b).await, false));
    let mut stream = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    let err = send_files(&mut stream, &[src.join("big.bin")], &mut events).await.unwrap_err();
    assert!(err.downcast_ref::<Declined>().is_some(), "{err}");
    decline.await.unwrap();

    // Auto-accept: no question asked.
    client(&b).await.set_auto_accept("alpha", true).await.unwrap();
    let mut stream = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    send_files(&mut stream, &[src.join("big.bin")], &mut events).await.unwrap();
    assert_eq!(std::fs::read(downloads.join("big (1).bin")).unwrap(), big);

    // Desktop drop: lands on the desktop without asking once A may drive B.
    client(&b).await.set_auto_accept("alpha", false).await.unwrap();
    client(&b).await.set_permission("alpha", "interdesk/input", true).await.unwrap();
    let mut stream = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    let opts = crossdrop_sdk::files::SendOptions { to_desktop: true };
    crossdrop_sdk::files::send_files_with(&mut stream, &[src.join("photos")], opts, &mut events).await.unwrap();
    let desktop = dir.path().join("bravo").join("desktop");
    assert_eq!(std::fs::read_to_string(desktop.join("photos/2026/a.jpg")).unwrap(), "jpeg");

    // Clipboard: refused until B allows it, then delivered.
    let err = client(&a).await.clipboard_send("secret", Some("bravo")).await.unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
    client(&b).await.set_permission("alpha", "crossdrop/clipboard", true).await.unwrap();
    assert_eq!(client(&a).await.clipboard_send("bonjour", Some("bravo")).await.unwrap(), 1);
    let (text, from) = client(&b).await.clipboard_get().await.unwrap().unwrap();
    assert_eq!((text.as_str(), from.as_str()), ("bonjour", "alpha"));
    // Broadcast from B reaches alpha only once alpha allows bravo too.
    assert_eq!(client(&b).await.clipboard_send("salut", None).await.unwrap(), 0);
    client(&a).await.set_permission("bravo", "crossdrop/clipboard", true).await.unwrap();
    assert_eq!(client(&b).await.clipboard_send("salut", None).await.unwrap(), 1);
    let (text, from) = client(&a).await.clipboard_get().await.unwrap().unwrap();
    assert_eq!((text.as_str(), from.as_str()), ("salut", "bravo"));

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

    // Unpairing revokes access, on both sides.
    client(&b).await.unpair("alpha").await.unwrap();
    let err = client(&a).await.open_channel(&b.id.to_string(), "crossdrop/files").await.unwrap_err();
    assert!(err.to_string().contains("not paired"), "{err}");

    a.shutdown();
    b.shutdown();
}

/// Waits for the next incoming transfer on `c` and answers it.
async fn answer_next_offer(mut c: Client, accept: bool) -> crossdrop_proto::ipc::OfferInfo {
    loop {
        if let Some(o) = c.offers().await.unwrap().pop() {
            c.offer_answer(&o.offer_id, accept).await.unwrap();
            return o;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

#[tokio::test]
async fn peer_restart_does_not_hang_the_next_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;
    pair(&a, &b).await;
    client(&b).await.set_auto_accept("alpha", true).await.unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hi").unwrap();
    let mut events = |_: SendEvent<'_>| {};
    let mut s = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    send_files(&mut s, std::slice::from_ref(&file), &mut events).await.unwrap();

    // bravo restarts on the same port; alpha still holds the old connection.
    let port = b.port;
    b.shutdown();
    // In one process the old endpoint's UDP socket closes only once its last
    // task has stopped, and Windows refuses the port until then: retry.
    let mut tries = 0;
    let b = loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = start(Options {
            home: Some(dir.path().join("bravo").join("home")),
            name: Some("bravo".into()),
            port: Some(port),
            mdns: Some(false),
            download_dir: Some(dir.path().join("bravo").join("downloads")),
            clipboard_sync: Some(false),
            desktop_dir: None,
        })
        .await;
        match started {
            Ok(b) => break b,
            Err(e) if tries < 25 && format!("{e:#}").contains("binding UDP") => tries += 1,
            Err(e) => panic!("bravo did not restart: {e:#}"),
        }
    };

    let started = std::time::Instant::now();
    let mut s = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    send_files(&mut s, std::slice::from_ref(&file), &mut events).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn unpairing_is_mutual_and_works_offline() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;
    pair(&a, &b).await;

    // bravo forgets alpha: alpha is told and forgets bravo too.
    client(&b).await.unpair("alpha").await.unwrap();
    let devices = client(&a).await.devices().await.unwrap();
    assert!(devices.iter().all(|d| !d.paired), "alpha still trusts bravo: {devices:?}");

    // With the other device gone, unpairing still works locally.
    pair(&a, &b).await;
    b.shutdown();
    client(&a).await.unpair("bravo").await.unwrap();
    assert!(client(&a).await.devices().await.unwrap().iter().all(|d| !d.paired));
    a.shutdown();
}

#[tokio::test]
async fn clipboard_can_flow_one_way() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;
    pair(&a, &b).await;

    // bravo takes alpha's copies but keeps its own; alpha takes nothing.
    client(&b).await.set_permission("alpha", "crossdrop/clipboard", true).await.unwrap();
    client(&b).await.set_clipboard_send("alpha", false).await.unwrap();
    client(&a).await.set_clipboard_send("bravo", true).await.unwrap();

    assert_eq!(client(&a).await.clipboard_send("from alpha", None).await.unwrap(), 1);
    let got = client(&b).await.clipboard_get().await.unwrap();
    assert_eq!(got.map(|(text, _)| text).as_deref(), Some("from alpha"));

    let err = client(&b).await.clipboard_send("from bravo", None).await.unwrap_err();
    assert!(err.to_string().contains("no device"), "{err}");
    assert!(client(&a).await.clipboard_get().await.unwrap().is_none());

    let devices = client(&a).await.devices().await.unwrap();
    assert!(devices.iter().any(|d| d.name == "bravo" && d.clipboard_send));
    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn oversized_clipboard_text_is_refused_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;
    pair(&a, &b).await;
    client(&b).await.set_permission("alpha", "crossdrop/clipboard", true).await.unwrap();

    let big = "x".repeat(2 * 1024 * 1024);
    let err = client(&a).await.clipboard_send(&big, Some("bravo")).await.unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
    assert_eq!(client(&a).await.clipboard_send("small", Some("bravo")).await.unwrap(), 1);
    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn abandoned_partial_files_are_removed_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let partial = dir.path().join("alpha").join("downloads").join(".crossdrop-partial");
    std::fs::create_dir_all(&partial).unwrap();
    let week_ago = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
    for name in ["old.part", "fresh.part"] {
        std::fs::write(partial.join(name), "x").unwrap();
    }
    std::fs::File::options().write(true).open(partial.join("old.part")).unwrap().set_modified(week_ago).unwrap();

    let a = daemon(dir.path(), "alpha").await;
    for _ in 0..100 {
        if !partial.join("old.part").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!partial.join("old.part").exists(), "a partial file untouched for a week must go");
    assert!(partial.join("fresh.part").exists(), "a recent one may still be resumed");
    a.shutdown();
}

#[tokio::test]
async fn a_desktop_drop_without_the_input_permission_waits_for_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;
    pair(&a, &b).await;
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hi").unwrap();

    // bravo has not allowed alpha to drive it: asking for the desktop must not
    // skip bravo's consent, nor put anything there.
    let answer = tokio::spawn(answer_next_offer(client(&b).await, true));
    let mut stream = client(&a).await.open_channel("bravo", "crossdrop/files").await.unwrap();
    let opts = crossdrop_sdk::files::SendOptions { to_desktop: true };
    let mut events = |_: SendEvent<'_>| {};
    crossdrop_sdk::files::send_files_with(&mut stream, std::slice::from_ref(&file), opts, &mut events).await.unwrap();
    let offer = answer.await.unwrap();
    assert_eq!(offer.files, 1, "bravo was asked");
    assert_eq!(std::fs::read_to_string(dir.path().join("bravo/downloads/note.txt")).unwrap(), "hi");
    assert!(!dir.path().join("bravo/desktop/note.txt").exists(), "nothing lands on the desktop");
    a.shutdown();
    b.shutdown();
}
