#![no_main]

use libfuzzer_sys::fuzz_target;
#[path = "../../src/crypto.rs"]
mod crypto;
use crypto::MsgSplitter;

fuzz_target!(|data: &[u8]| {
    let selector = data.first().copied().unwrap_or(0);
    let proto = match selector % 3 {
        0 => 0xEEEE_EEEE,
        1 => 0xDDDD_DDDD,
        _ => 0xEFEF_EFEF,
    };
    let init = [0u8; 56];
    let mut splitter = MsgSplitter::new(&init, proto).expect("fixed-size init is valid");
    let payload = data.get(1..).unwrap_or(&[]);
    let chunk_size = 1 + (selector as usize % 255);
    for chunk in payload.chunks(chunk_size) {
        let _ = splitter.split(chunk);
    }
    let _ = splitter.flush();
});
