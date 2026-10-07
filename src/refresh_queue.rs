/// Coordinates requests across the scan/fetch task completion boundaries.
#[derive(Default)]
pub struct RefreshQueue {
    scan_requested: bool,
    fetch_requested: bool,
}

impl RefreshQueue {
    /// A request arriving during work asks for one later snapshot/check.
    /// Starting that follow-up consumes all requests made before it starts.
    pub fn request_scan(&mut self, running: bool) -> bool {
        self.scan_requested = running;
        !running
    }

    pub fn scan_finished(&mut self, roots_changed: bool) -> bool {
        std::mem::take(&mut self.scan_requested) || roots_changed
    }

    pub fn request_fetch(&mut self, running: bool) -> bool {
        self.fetch_requested = running;
        !running
    }

    pub fn fetch_finished(&mut self, scan_running: bool) -> bool {
        // If a scan is active, its completion requests the latest GitHub check.
        // Otherwise a queued check starts now, after the subprocess has ended.
        std::mem::take(&mut self.fetch_requested) && !scan_running
    }
}

#[cfg(test)]
mod tests {
    use super::RefreshQueue;

    #[test]
    fn review_regression_refresh_during_fetch_queues_one_new_check() {
        let mut queue = RefreshQueue::default();
        assert!(queue.request_fetch(false)); // Older GitHub request starts.
        assert!(queue.request_scan(false)); // User presses Refresh.
        assert!(!queue.scan_finished(false));
        assert!(!queue.request_fetch(true)); // Refresh scan finishes first.
        assert!(!queue.request_fetch(true)); // More requests coalesce.
        // Older request completes (including an error): start the requested check.
        assert!(
            queue.fetch_finished(false),
            "Refresh must perform its own GitHub check after the older request"
        );
        assert!(queue.request_fetch(false));
        assert!(!queue.fetch_finished(false)); // No automatic retry loop.
    }

    #[test]
    fn review_regression_refresh_during_scan_queues_one_new_snapshot() {
        let mut queue = RefreshQueue::default();
        assert!(queue.request_scan(false));
        // A remote changes after the active scan inspected its checkout.
        assert!(!queue.request_scan(true));
        assert!(!queue.request_scan(true));
        assert!(
            queue.scan_finished(false),
            "an active scan must not swallow a later Refresh"
        );
        assert!(queue.request_scan(false));
        assert!(!queue.scan_finished(false));
    }

    #[test]
    fn fetch_finishing_before_scan_waits_for_latest_snapshot() {
        for check_already_queued in [false, true] {
            let mut queue = RefreshQueue::default();
            assert!(queue.request_fetch(false));
            if check_already_queued {
                assert!(!queue.request_fetch(true));
            }
            assert!(queue.request_scan(false));
            assert!(!queue.fetch_finished(true));
            assert!(!queue.scan_finished(false));
            assert!(queue.request_fetch(false));
            assert!(!queue.fetch_finished(false));
        }
    }

    #[test]
    fn changed_roots_require_a_new_scan() {
        let mut queue = RefreshQueue::default();
        assert!(queue.request_scan(false));
        assert!(!queue.request_scan(true));
        assert!(queue.scan_finished(true));
        assert!(queue.request_scan(false));
        assert!(!queue.scan_finished(false));
    }
}
