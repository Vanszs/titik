//! Scroll methods on [`super::AppStateRest`].
//!
//! `scroll` is an offset-from-top used only when NOT following;
//! `follow` pins the view to the bottom. `last_max_scroll` (set by the
//! renderer) lets these clamp without knowing the viewport here.

use super::rest::AppStateRest;

impl AppStateRest {
    pub fn scroll_up(&mut self) {
        if self.follow {
            // Leave follow starting from the current bottom offset.
            self.follow = false;
            self.scroll = self.last_max_scroll.get();
        }
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        if self.follow {
            return; // already pinned to the bottom
        }
        self.scroll = self.scroll.saturating_add(1);
        if self.scroll >= self.last_max_scroll.get() {
            self.follow = true; // back at the bottom → resume following
        }
    }

    pub fn reset_scroll(&mut self) {
        self.follow = true;
        self.scroll = 0;
    }

    /// Scroll the full-screen sub-agent viewer up by `n` lines.
    ///
    /// If currently following (pinned to bottom), detaches follow and seeds the
    /// offset from the current bottom so the view doesn't jump to the top.
    /// Clamped at 0.
    pub fn agent_viewer_scroll_up(&mut self, n: u16) {
        if self.agent_viewer_follow {
            // Detach: start from the current bottom offset.
            self.agent_viewer_follow = false;
            self.agent_viewer_scroll = self.last_max_scroll.get();
        } else if self.agent_viewer_scroll > self.last_max_scroll.get() {
            self.agent_viewer_scroll = self.last_max_scroll.get();
        }
        self.agent_viewer_scroll = self.agent_viewer_scroll.saturating_sub(n);
    }

    /// Scroll the full-screen sub-agent viewer down by `n` lines, clamped to the
    /// last-published max. Re-attaches follow when the offset reaches the bottom.
    pub fn agent_viewer_scroll_down(&mut self, n: u16) {
        if self.agent_viewer_follow {
            return; // already pinned to the bottom
        }
        let max = self.last_max_scroll.get();
        self.agent_viewer_scroll = self.agent_viewer_scroll.saturating_add(n).min(max);
        if self.agent_viewer_scroll >= max {
            self.agent_viewer_follow = true; // back at the bottom -> resume following
        }
    }
}
