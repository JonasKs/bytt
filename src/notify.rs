//! macOS notifications.
//!
//! Inside `Bytt.app` (how Homebrew installs it) bytt posts them itself, so they carry
//! its name and icon. A bare binary, like a `cargo run`, has no bundle to post as and
//! asks `osascript` instead, which macOS files under Script Editor.
//!
//! `NSUserNotification` is deprecated in favour of `UserNotifications`, which needs an
//! authorization round-trip and a run loop; this one still works for a signed bundle.
#![allow(deprecated)]

use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use objc2_foundation::{NSBundle, NSString, NSUserNotification, NSUserNotificationCenter};

/// Best effort: with notifications turned off, nothing shows and nothing fails.
pub fn send(message: &str) {
    if NSBundle::mainBundle().bundleIdentifier().is_some() {
        let note = NSUserNotification::new();
        note.setTitle(Some(&NSString::from_str("bytt")));
        note.setInformativeText(Some(&NSString::from_str(message)));
        NSUserNotificationCenter::defaultUserNotificationCenter().deliverNotification(&note);
        // Delivery is handed to the notification daemon asynchronously; a process
        // that exits right away can lose it.
        sleep(Duration::from_millis(300));
        return;
    }
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{escaped}\" with title \"bytt\"");
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output();
}
