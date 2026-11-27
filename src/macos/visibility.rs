//! Tracks macOS application hide/unhide (`⌘H`) notifications per app instance.
//!
//! When an application unhides, macOS brings every one of its windows back on
//! screen, clamping windows Spaceman had parked off-screen on other workspaces
//! back onto the display edge. Recording hide and unhide events per app lets
//! parking treat those non-focused windows as system returns (re-parked) rather
//! than user-summoned windows (re-homed and eventually parking-exempt).

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::AppInstanceId;

use super::running_application;

/// How long an application's unhide timestamp stays in the visibility log.
const APP_UNHIDE_RETENTION: Duration = Duration::from_secs(10);

static APP_VISIBILITY: OnceLock<Mutex<AppVisibilityLog>> = OnceLock::new();

#[derive(Default)]
struct AppVisibilityLog {
    hidden: HashSet<AppInstanceId>,
    unhidden_at: HashMap<AppInstanceId, Instant>,
}

impl AppVisibilityLog {
    fn note_hidden(&mut self, app: AppInstanceId) {
        self.hidden.insert(app);
    }

    fn note_unhidden(&mut self, app: AppInstanceId, now: Instant) {
        self.hidden.remove(&app);
        self.prune(now);
        self.unhidden_at.insert(app, now);
    }

    fn note_terminated(&mut self, app: &AppInstanceId) {
        self.hidden.remove(app);
        self.unhidden_at.remove(app);
    }

    /// When `app` was last unhidden. If a hide notification arrived and the
    /// caller already observes `app` unhidden before the main thread delivers
    /// the matching unhide notification, record the unhide now so the worker
    /// never races ahead of AppKit's notification queue.
    fn last_unhide(&mut self, app: &AppInstanceId, now: Instant) -> Option<Instant> {
        if self.hidden.remove(app) {
            self.prune(now);
            self.unhidden_at.insert(app.clone(), now);
        }
        self.unhidden_at.get(app).copied()
    }

    fn prune(&mut self, now: Instant) {
        self.unhidden_at
            .retain(|_, at| now.saturating_duration_since(*at) < APP_UNHIDE_RETENTION);
    }
}

fn app_visibility() -> &'static Mutex<AppVisibilityLog> {
    APP_VISIBILITY.get_or_init(|| Mutex::new(AppVisibilityLog::default()))
}

pub(super) fn note_app_hidden(app: AppInstanceId) {
    app_visibility()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .note_hidden(app);
}

pub(super) fn note_app_unhidden(app: AppInstanceId) {
    app_visibility()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .note_unhidden(app, Instant::now());
}

pub(super) fn note_app_terminated(app: &AppInstanceId) {
    app_visibility()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .note_terminated(app);
}

/// When `app` was last unhidden (after ⌘H / Hide Application), if recently.
#[must_use]
pub fn last_app_unhide(app: &AppInstanceId) -> Option<Instant> {
    if running_application(app).is_some_and(|application| application.isHidden()) {
        return None;
    }
    app_visibility()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .last_unhide(app, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(pid: i32) -> AppInstanceId {
        AppInstanceId {
            pid,
            launched_at: 1,
        }
    }

    #[test]
    fn records_unhide_promotes_hidden_race_and_forgets_terminated_apps() {
        let mut log = AppVisibilityLog::default();
        let now = Instant::now();
        let zen = app(10);
        let other = app(20);

        assert_eq!(log.last_unhide(&zen, now), None);

        // Normal hide -> unhide records the unhide timestamp for that app only.
        log.note_hidden(zen.clone());
        log.note_unhidden(zen.clone(), now);
        assert_eq!(log.last_unhide(&zen, now), Some(now));
        assert_eq!(log.last_unhide(&other, now), None);

        // Worker racing ahead of `NSWorkspaceDidUnhideApplicationNotification`
        // after a prior hide promotes the pending hidden entry to an unhide now.
        let later = now + Duration::from_secs(2);
        log.note_hidden(other.clone());
        assert_eq!(log.last_unhide(&other, later), Some(later));
        // A second query keeps that timestamp rather than refreshing it.
        assert_eq!(
            log.last_unhide(&other, later + Duration::from_secs(1)),
            Some(later)
        );

        // Old unhide entries are pruned on subsequent writes, and terminated apps are forgotten.
        let much_later = later + APP_UNHIDE_RETENTION + Duration::from_secs(1);
        log.note_unhidden(zen.clone(), much_later);
        assert_eq!(log.last_unhide(&other, much_later), None);
        log.note_terminated(&zen);
        assert_eq!(log.last_unhide(&zen, much_later), None);
    }
}
