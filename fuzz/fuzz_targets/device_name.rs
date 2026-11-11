//! Names received from other devices are cleaned before being shown.
#![no_main]

use crossdrop_proto::{clean_device_name, MAX_NAME_CHARS};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|raw: &str| {
    let Some(name) = clean_device_name(raw) else { return };
    assert!(!name.is_empty());
    assert!(name.chars().count() <= MAX_NAME_CHARS);
    assert!(!name.chars().any(char::is_control), "{name:?}");
    assert!(!name.starts_with(char::is_whitespace) && !name.ends_with(char::is_whitespace), "{name:?}");
    assert!(!name.contains("  "), "{name:?}");
    // Cleaning a clean name changes nothing.
    assert_eq!(clean_device_name(&name).as_deref(), Some(name.as_str()));
});
