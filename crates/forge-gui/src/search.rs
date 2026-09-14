//! State of the scrollback search bar. The matching happens in the daemon
//! (`ClientMessage::Search`); this module owns the query, the reply
//! bookkeeping and the selection of the current match.

use crate::grid_element::{SearchHighlights, SearchSpan};
use proto_ipc::{SearchMatch, Viewport};
use std::time::{Duration, Instant};

/// Minimum spacing between automatic re-runs while output keeps arriving,
/// so a flood of output does not turn into a flood of scrollback dumps.
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    /// Towards older rows: what Enter does, like most terminals.
    Older,
    Newer,
}

#[derive(Debug, Default)]
pub struct SearchState {
    pub open: bool,
    pub query: String,
    pub options: SearchOptions,
    /// Tab whose scrollback is being searched.
    pub tab_id: Option<u64>,
    /// Matches of the last answered request, oldest row first.
    pub matches: Vec<SearchMatch>,
    pub current: Option<usize>,
    /// Pattern error reported by the daemon for the current query.
    pub error: Option<String>,
    next_request: u64,
    /// Request still unanswered; older replies are ignored.
    pending: Option<u64>,
    /// The scrollback changed since the last request.
    stale: bool,
    last_sent: Option<Instant>,
}

/// How the query is interpreted; toggled from the bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
}

/// What a selection change asks the window to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchAction {
    None,
    /// Scroll so the current match is visible.
    Reveal,
}

impl SearchState {
    /// Starts a request; returns its id to send to the daemon, or `None`
    /// when the query is empty (results are cleared instead).
    pub fn begin_request(&mut self) -> Option<u64> {
        self.stale = false;
        self.error = None;
        if self.query.is_empty() {
            self.pending = None;
            self.matches.clear();
            self.current = None;
            return None;
        }
        self.next_request += 1;
        self.pending = Some(self.next_request);
        self.last_sent = Some(Instant::now());
        Some(self.next_request)
    }

    /// Applies a daemon reply. Returns whether it was the awaited one; a
    /// new query selects the newest match, a refresh keeps the closest row
    /// to the previous selection.
    pub fn apply_results(
        &mut self,
        request_id: u64,
        matches: Vec<SearchMatch>,
        error: Option<String>,
    ) -> bool {
        if self.pending != Some(request_id) {
            return false;
        }
        self.pending = None;
        let previous_row = self
            .current
            .and_then(|index| self.matches.get(index))
            .map(|m| m.row);
        self.matches = matches;
        self.error = error;
        self.current = match previous_row {
            Some(row) => closest_match(&self.matches, row),
            None => self.matches.len().checked_sub(1),
        };
        true
    }

    /// Moves the selection one match in `direction`, wrapping around.
    pub fn step(&mut self, direction: SearchDirection) -> SearchAction {
        let count = self.matches.len();
        if count == 0 {
            return SearchAction::None;
        }
        self.current = Some(match (self.current, direction) {
            (None, SearchDirection::Older) => count - 1,
            (None, SearchDirection::Newer) => 0,
            (Some(index), SearchDirection::Older) => index.checked_sub(1).unwrap_or(count - 1),
            (Some(index), SearchDirection::Newer) => (index + 1) % count,
        });
        SearchAction::Reveal
    }

    /// Row of the current match.
    #[must_use]
    pub fn current_row(&self) -> Option<u64> {
        self.matches.get(self.current?).map(|found| found.row)
    }

    /// Marks the results as outdated (new output, resize/reflow).
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Whether a throttled re-run is due.
    #[must_use]
    pub fn refresh_due(&self) -> bool {
        self.open
            && self.stale
            && self.pending.is_none()
            && !self.query.is_empty()
            && self
                .last_sent
                .is_none_or(|sent| sent.elapsed() >= REFRESH_INTERVAL)
    }

    /// Drops results and pending replies, keeping the query and toggles.
    pub fn clear_results(&mut self) {
        self.pending = None;
        self.matches.clear();
        self.current = None;
        self.error = None;
        self.stale = false;
    }

    /// Highlights for the grid painter.
    #[must_use]
    pub fn highlights(&self) -> SearchHighlights {
        SearchHighlights {
            matches: self
                .matches
                .iter()
                .map(|found| SearchSpan {
                    row: found.row,
                    start: found.start,
                    end: found.end,
                })
                .collect(),
            current: self.current,
        }
    }

    /// "k/n" label for the bar.
    #[must_use]
    pub fn position_label(&self) -> String {
        match (self.current, self.matches.len()) {
            (_, 0) if self.query.is_empty() => String::new(),
            (_, 0) => "0".into(),
            (Some(index), count) => format!("{}/{count}", index + 1),
            (None, count) => format!("{count}"),
        }
    }
}

/// Index of the match whose row is nearest to `row`; ties go to the later
/// (newer) match so a refresh never drifts upwards while output scrolls.
fn closest_match(matches: &[SearchMatch], row: u64) -> Option<usize> {
    let after = matches.partition_point(|found| found.row < row);
    let candidates = [
        after.checked_sub(1),
        (after < matches.len()).then_some(after),
    ];
    candidates
        .into_iter()
        .flatten()
        .min_by_key(|index| (matches[*index].row.abs_diff(row), std::cmp::Reverse(*index)))
}

/// Viewport top that shows `row` centred, unless it is already visible.
#[must_use]
pub fn reveal_row(row: u64, viewport: Viewport) -> Option<u64> {
    let visible = viewport.offset..viewport.offset + viewport.len;
    if visible.contains(&row) && viewport.len > 0 {
        return None;
    }
    Some(row.saturating_sub(viewport.len / 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(row: u64) -> SearchMatch {
        SearchMatch {
            row,
            start: 0,
            end: 1,
            preview: String::new(),
        }
    }

    #[test]
    fn a_new_query_selects_the_newest_match_and_enter_walks_upwards() {
        let mut state = SearchState {
            query: "x".into(),
            ..SearchState::default()
        };
        let id = state.begin_request().unwrap();
        assert!(!state.apply_results(id + 1, vec![found(1)], None));
        assert!(state.apply_results(id, vec![found(2), found(5), found(9)], None));
        assert_eq!(state.current, Some(2));
        assert_eq!(state.position_label(), "3/3");
        assert_eq!(state.step(SearchDirection::Older), SearchAction::Reveal);
        assert_eq!(state.current_row(), Some(5));
        state.step(SearchDirection::Older);
        state.step(SearchDirection::Older);
        assert_eq!(state.current_row(), Some(9), "wraps to the newest");
        state.step(SearchDirection::Newer);
        assert_eq!(state.current_row(), Some(2), "wraps to the oldest");
    }

    #[test]
    fn a_refresh_keeps_the_closest_row_and_empty_queries_clear() {
        let mut state = SearchState {
            query: "x".into(),
            ..SearchState::default()
        };
        let id = state.begin_request().unwrap();
        state.apply_results(id, vec![found(2), found(5), found(9)], None);
        state.step(SearchDirection::Older);
        assert_eq!(state.current_row(), Some(5));
        // Rows are absolute, so a reflow that moved the match keeps the
        // selection on the nearest row (ties go to the newer one).
        let id = state.begin_request().unwrap();
        state.apply_results(id, vec![found(4), found(6), found(11)], None);
        assert_eq!(state.current_row(), Some(6));
        state.query.clear();
        assert_eq!(state.begin_request(), None);
        assert!(state.matches.is_empty() && state.current.is_none());
        assert_eq!(state.position_label(), "");
    }

    #[test]
    fn refresh_is_throttled_and_only_while_open_with_a_query() {
        let mut state = SearchState {
            open: true,
            query: "x".into(),
            ..SearchState::default()
        };
        assert!(!state.refresh_due());
        state.invalidate();
        assert!(state.refresh_due());
        let id = state.begin_request().unwrap();
        state.invalidate();
        assert!(!state.refresh_due(), "waiting for the reply");
        state.apply_results(id, Vec::new(), None);
        state.invalidate();
        assert!(!state.refresh_due(), "sent less than 250 ms ago");
        state.last_sent = Instant::now().checked_sub(Duration::from_secs(1));
        assert!(state.refresh_due());
    }

    #[test]
    fn reveal_centres_rows_outside_the_viewport() {
        let viewport = Viewport {
            total: 100,
            offset: 40,
            len: 10,
        };
        assert_eq!(reveal_row(45, viewport), None);
        assert_eq!(reveal_row(12, viewport), Some(7));
        assert_eq!(reveal_row(2, viewport), Some(0));
        assert_eq!(reveal_row(90, viewport), Some(85));
    }

    #[test]
    fn closest_match_prefers_the_newer_row_on_ties() {
        let matches = [found(2), found(6)];
        assert_eq!(closest_match(&matches, 4), Some(1));
        assert_eq!(closest_match(&matches, 3), Some(0));
        assert_eq!(closest_match(&[], 3), None);
    }
}
