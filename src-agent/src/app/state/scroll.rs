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
}
