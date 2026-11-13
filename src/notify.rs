//! macOS notifications.
//!
//! Inside `Bytt.app` (how Homebrew installs it, the `bytt` on the PATH included) bytt
//! posts them through `UserNotifications`, so they carry its name and icon. macOS asks
//! once whether to allow them. A bare binary, like a `cargo run`, has no bundle to
//! post as and asks `osascript` instead, which macOS files under Script Editor.
//!
//! The older `NSUserNotification` never asks for permission, and current macOS drops
//! its notifications without a word.
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNUserNotificationCenter,
};

/// Ask macOS for permission to notify; it prompts the first time only.
pub fn authorize() {
    if in_bundle() {
        request(None);
    }
}

/// Best effort: with notifications turned off, nothing shows and nothing fails.
pub fn send(message: &str) {
    if in_bundle() {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str("bytt"));
        content.setBody(&NSString::from_str(message));
        let id = format!("bytt-{}-{}", std::process::id(), unique());
        let note = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&id),
            &content,
            None,
        );
        request(Some(note));
        // Delivery happens on another thread; a CLI that exits right away loses it.
        sleep(Duration::from_millis(500));
        return;
    }
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{escaped}\" with title \"bytt\"");
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output();
}

fn in_bundle() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// Request permission, then post `note` if there is one and it was granted.
fn request(note: Option<Retained<UNNotificationRequest>>) {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let post = center.clone();
    let done = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        if granted.as_bool()
            && let Some(note) = &note
        {
            post.addNotificationRequest_withCompletionHandler(note, None);
        }
    });
    let options = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
    center.requestAuthorizationWithOptions_completionHandler(options, &done);
}

/// Nanoseconds since the epoch, so two notifications never share an identifier.
fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}
