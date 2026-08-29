//! Regression guard for debug session `countdown-no-audio`.
//!
//! Root cause (confirmed by controlled A/B experiment): a launchd LaunchAgent
//! with `ProcessType Background` runs in launchd's background scheduling band,
//! where CoreAudio speaker playback is suppressed. `say`-based workflows (e.g.
//! `workflows/scripts/countdown.sh`) then hang ~7s per call and produce NO
//! audio while still exiting 0. The daemon is a user-facing voice agent whose
//! whole purpose is audible output, so it must NOT be registered in the
//! background band.
//!
//! This test locks the packaging plist's `ProcessType` to `Standard` so the
//! regression cannot silently reappear.

use std::fs;
use std::path::PathBuf;

/// The macOS launchd plist template `make install` seds + installs into
/// `~/Library/LaunchAgents/`. Resolved relative to this crate's manifest dir.
fn packaging_plist_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("packaging/macos/com.wk-voice-agent.orchestratord.plist")
}

/// Extract the string value that immediately follows the `<key>ProcessType</key>`
/// element in a launchd plist. Returns the trimmed contents of the next
/// `<string>...</string>`.
fn process_type_value(plist: &str) -> Option<String> {
    let after_key = plist.split("<key>ProcessType</key>").nth(1)?;
    let open = after_key.find("<string>")? + "<string>".len();
    let rest = &after_key[open..];
    let close = rest.find("</string>")?;
    Some(rest[..close].trim().to_string())
}

#[test]
fn orchestratord_plist_is_not_in_launchd_background_band() {
    let path = packaging_plist_path();
    let plist = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read packaging plist at {}: {e}", path.display()));

    let process_type = process_type_value(&plist).unwrap_or_else(|| {
        panic!(
            "packaging plist at {} has no <key>ProcessType</key><string>…</string>",
            path.display()
        )
    });

    assert_ne!(
        process_type, "Background",
        "orchestratord LaunchAgent must not use ProcessType Background: the background \
         scheduling band suppresses CoreAudio speaker output, so `say`-based workflows \
         run silently (debug session countdown-no-audio). Use `Standard`."
    );
    assert_eq!(
        process_type, "Standard",
        "expected ProcessType Standard (proven audible + fast in the A/B experiment), got {process_type:?}"
    );
}
