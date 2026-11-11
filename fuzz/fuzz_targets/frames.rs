//! Any bytes a peer or a local client sends: reading frames from them must
//! return messages or errors, never panic or allocate past the size limit.
#![no_main]

use crossdrop_proto::{ipc, read_frame, read_frame_max, wire, MAX_SMALL_FRAME_LEN};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        // Each type reads the same bytes, frame after frame, until an error.
        macro_rules! drain {
            ($t:ty, $read:expr) => {{
                let mut r = data;
                while $read(&mut r).await.map(|_: $t| ()).is_ok() {}
            }};
        }
        drain!(wire::OpenChannel, |r| read_frame_max(r, MAX_SMALL_FRAME_LEN));
        drain!(wire::Pair, |r| read_frame_max(r, MAX_SMALL_FRAME_LEN));
        drain!(wire::Files, read_frame);
        drain!(wire::Clipboard, read_frame);
        drain!(ipc::Request, read_frame);
        drain!(ipc::Response, read_frame);
    });
});
