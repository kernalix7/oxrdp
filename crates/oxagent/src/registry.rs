use std::collections::HashMap;

/// First channel number available for per-window video (mirrors `oxproto::channel::VIDEO_BASE`).
pub const VIDEO_BASE: u16 = 16;

/// A window that the registry is tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackedWindow {
    /// Protocol window id, unique for the lifetime of the session.
    pub window_id: u32,
    /// Channel this window's frames travel on.
    pub video_channel: u16,
}

/// Assigns protocol ids and video channels to native windows.
#[derive(Debug, Default)]
pub struct WindowRegistry {
    handle_to_window: HashMap<isize, TrackedWindow>,
    id_to_handle: HashMap<u32, isize>,
    next_window_id: u32,
    free_channels: Vec<u16>,
    next_channel: u16,
}

impl WindowRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            handle_to_window: HashMap::new(),
            id_to_handle: HashMap::new(),
            next_window_id: 1,
            free_channels: Vec::new(),
            next_channel: VIDEO_BASE,
        }
    }

    /// Register `handle` if it is new, returning its tracking info and whether this call
    /// created it. An already-tracked handle keeps its existing id and channel.
    pub fn track(&mut self, handle: isize) -> (TrackedWindow, bool) {
        if let Some(&tracked) = self.handle_to_window.get(&handle) {
            return (tracked, false);
        }

        let window_id = self.next_window_id;
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window id space exhausted");

        let video_channel = if let Some(channel) = self.free_channels.pop() {
            channel
        } else {
            let channel = self.next_channel;
            self.next_channel = self
                .next_channel
                .checked_add(1)
                .expect("video channel space exhausted");
            channel
        };

        let tracked = TrackedWindow {
            window_id,
            video_channel,
        };
        self.handle_to_window.insert(handle, tracked);
        self.id_to_handle.insert(window_id, handle);
        (tracked, true)
    }

    /// Look up a handle that is already tracked.
    pub fn get(&self, handle: isize) -> Option<TrackedWindow> {
        self.handle_to_window.get(&handle).copied()
    }

    /// Look up by protocol window id.
    pub fn by_id(&self, window_id: u32) -> Option<TrackedWindow> {
        let handle = self.id_to_handle.get(&window_id).copied()?;
        self.handle_to_window.get(&handle).copied()
    }

    /// Stop tracking a handle, freeing its video channel for reuse. Returns what it was
    /// tracking, or `None` if it was not tracked.
    pub fn forget(&mut self, handle: isize) -> Option<TrackedWindow> {
        let tracked = self.handle_to_window.remove(&handle)?;
        self.id_to_handle.remove(&tracked.window_id);
        self.free_channels.push(tracked.video_channel);
        Some(tracked)
    }

    /// Reconcile against the current set of live handles: every tracked handle absent from
    /// `live` is forgotten. Returns the windows that disappeared, so the caller can emit a
    /// `WindowClosed` for each.
    pub fn retain_live(&mut self, live: &[isize]) -> Vec<TrackedWindow> {
        let mut to_remove: Vec<isize> = self
            .handle_to_window
            .keys()
            .copied()
            .filter(|h| !live.contains(h))
            .collect();

        to_remove.sort_unstable_by_key(|h| {
            self.handle_to_window
                .get(h)
                .map(|w| w.window_id)
                .unwrap_or(0)
        });

        let mut removed = Vec::with_capacity(to_remove.len());
        for handle in to_remove {
            if let Some(tracked) = self.forget(handle) {
                removed.push(tracked);
            }
        }

        removed
    }

    /// Number of tracked windows.
    pub fn len(&self) -> usize {
        self.handle_to_window.len()
    }

    /// Whether nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.handle_to_window.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_and_reuses_the_same_entry() {
        let mut r = WindowRegistry::new();
        let (a, created) = r.track(0x1000);
        assert!(created);
        assert_eq!(a.window_id, 1);
        assert_eq!(a.video_channel, VIDEO_BASE);

        let (again, created) = r.track(0x1000);
        assert!(!created, "an already-tracked handle is not re-created");
        assert_eq!(again, a);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn window_ids_are_never_reused_even_when_a_handle_is() {
        let mut r = WindowRegistry::new();
        let (first, _) = r.track(0x1000);
        r.forget(0x1000);
        // The OS hands the same handle to a different window later.
        let (second, created) = r.track(0x1000);
        assert!(created);
        assert_ne!(
            second.window_id, first.window_id,
            "ids must not be recycled"
        );
        assert!(second.window_id > first.window_id);
    }

    #[test]
    fn video_channels_are_compact_and_reused() {
        let mut r = WindowRegistry::new();
        let (a, _) = r.track(1);
        let (b, _) = r.track(2);
        let (c, _) = r.track(3);
        assert_eq!(a.video_channel, VIDEO_BASE);
        assert_eq!(b.video_channel, VIDEO_BASE + 1);
        assert_eq!(c.video_channel, VIDEO_BASE + 2);

        r.forget(2);
        let (d, _) = r.track(4);
        assert_eq!(
            d.video_channel,
            VIDEO_BASE + 1,
            "the freed channel is reused"
        );
        assert!(d.window_id > c.window_id);
    }

    #[test]
    fn lookups_work_both_ways() {
        let mut r = WindowRegistry::new();
        let (w, _) = r.track(0xABCD);
        assert_eq!(r.get(0xABCD), Some(w));
        assert_eq!(r.by_id(w.window_id), Some(w));
        assert_eq!(r.get(0x1), None);
        assert_eq!(r.by_id(999), None);
    }

    #[test]
    fn forget_reports_what_it_removed() {
        let mut r = WindowRegistry::new();
        let (w, _) = r.track(7);
        assert_eq!(r.forget(7), Some(w));
        assert_eq!(r.forget(7), None);
        assert!(r.is_empty());
    }

    #[test]
    fn retain_live_reports_closures_in_id_order() {
        let mut r = WindowRegistry::new();
        let (a, _) = r.track(1);
        let (b, _) = r.track(2);
        let (c, _) = r.track(3);

        // Only handle 2 survives.
        let gone = r.retain_live(&[2]);
        assert_eq!(gone.len(), 2);
        assert_eq!(gone[0].window_id, a.window_id);
        assert_eq!(gone[1].window_id, c.window_id);
        assert_eq!(r.len(), 1);
        assert_eq!(r.get(2), Some(b));
    }

    #[test]
    fn retain_live_with_everything_alive_reports_nothing() {
        let mut r = WindowRegistry::new();
        r.track(1);
        r.track(2);
        assert!(r.retain_live(&[1, 2]).is_empty());
        assert_eq!(r.len(), 2);
    }
}
