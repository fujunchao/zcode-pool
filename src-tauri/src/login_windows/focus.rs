//! 关闭前的焦点交接策略。原生适配层提供即时状态，策略不等待、不强制抢焦点。

pub(super) trait Backend {
    type Window: Copy + Eq;

    fn foreground(&mut self) -> Option<Self::Window>;
    fn available(&mut self, window: Self::Window) -> bool;
    fn activate(&mut self, window: Self::Window);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Handoff<W> {
    NotForeground,
    NoCandidate,
    Transferred(W),
    Interrupted,
    Rejected,
}

pub(super) fn before_close<B: Backend>(
    backend: &mut B,
    closing: B::Window,
    candidates: impl IntoIterator<Item = B::Window>,
) -> Handoff<B::Window> {
    if backend.foreground() != Some(closing) {
        return Handoff::NotForeground;
    }
    let mut attempted = false;
    for candidate in candidates {
        if candidate == closing || !backend.available(candidate) {
            continue;
        }
        // 枚举窗口期间用户可能已经切走；不能沿用枚举前的焦点快照。
        if backend.foreground() != Some(closing) {
            return Handoff::Interrupted;
        }
        attempted = true;
        backend.activate(candidate);
        match backend.foreground() {
            Some(actual) if actual == candidate => return Handoff::Transferred(candidate),
            Some(actual) if actual == closing => {}
            _ => return Handoff::Interrupted,
        }
    }
    if attempted {
        Handoff::Rejected
    } else {
        Handoff::NoCandidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    struct Desktop {
        foreground: Option<u32>,
        available: BTreeSet<u32>,
        refused: BTreeSet<u32>,
        activated: Vec<u32>,
        switch_during_check: Option<Option<u32>>,
        switch_after_activation: Option<Option<u32>>,
    }

    impl Desktop {
        fn new(foreground: Option<u32>, available: &[u32]) -> Self {
            Self {
                foreground,
                available: available.iter().copied().collect(),
                refused: BTreeSet::new(),
                activated: Vec::new(),
                switch_during_check: None,
                switch_after_activation: None,
            }
        }
    }

    impl Backend for Desktop {
        type Window = u32;

        fn foreground(&mut self) -> Option<u32> {
            self.foreground
        }

        fn available(&mut self, window: u32) -> bool {
            if let Some(next) = self.switch_during_check.take() {
                self.foreground = next;
            }
            self.available.contains(&window)
        }

        fn activate(&mut self, window: u32) {
            self.activated.push(window);
            if let Some(next) = self.switch_after_activation.take() {
                self.foreground = next;
            } else if !self.refused.contains(&window) {
                self.foreground = Some(window);
            }
        }
    }

    #[test]
    fn focused_close_hands_off_to_a_never_activated_replacement() {
        let mut desktop = Desktop::new(Some(6), &[7]);
        assert_eq!(before_close(&mut desktop, 6, [7]), Handoff::Transferred(7));
        assert_eq!(desktop.foreground, Some(7));
        assert_eq!(desktop.activated, [7]);
    }

    #[test]
    fn closing_a_background_window_preserves_the_users_foreground() {
        // 另一扇登录窗、主界面、外部程序、暂时无前台都不能被后台关窗打断。
        for foreground in [Some(7), Some(100), Some(200), None] {
            let mut desktop = Desktop::new(foreground, &[7, 8]);
            assert_eq!(
                before_close(&mut desktop, 6, [7, 8]),
                Handoff::NotForeground
            );
            assert_eq!(desktop.foreground, foreground);
            assert!(desktop.activated.is_empty());
        }
    }

    #[test]
    fn skips_self_and_unavailable_windows() {
        let mut desktop = Desktop::new(Some(6), &[6, 9]);
        assert_eq!(
            before_close(&mut desktop, 6, [6, 7, 8, 9]),
            Handoff::Transferred(9)
        );
        assert_eq!(desktop.activated, [9]);
    }

    #[test]
    fn last_window_or_hidden_candidates_do_not_activate_anything() {
        for candidates in [vec![], vec![6], vec![7, 8]] {
            let mut desktop = Desktop::new(Some(6), &[]);
            assert_eq!(
                before_close(&mut desktop, 6, candidates),
                Handoff::NoCandidate
            );
            assert_eq!(desktop.foreground, Some(6));
            assert!(desktop.activated.is_empty());
        }
    }

    #[test]
    fn switching_away_during_candidate_selection_cancels_handoff() {
        for next in [Some(100), Some(200), None] {
            let mut desktop = Desktop::new(Some(6), &[7, 8]);
            desktop.switch_during_check = Some(next);
            assert_eq!(before_close(&mut desktop, 6, [7, 8]), Handoff::Interrupted);
            assert!(desktop.activated.is_empty());
            assert_eq!(desktop.foreground, next);
        }
    }

    #[test]
    fn rejection_can_try_another_candidate_only_while_source_stays_foreground() {
        let mut desktop = Desktop::new(Some(6), &[7, 8]);
        desktop.refused.insert(7);
        assert_eq!(
            before_close(&mut desktop, 6, [7, 8]),
            Handoff::Transferred(8)
        );
        assert_eq!(desktop.activated, [7, 8]);
    }

    #[test]
    fn all_rejected_candidates_do_not_trigger_a_forced_retry() {
        let mut desktop = Desktop::new(Some(6), &[7, 8]);
        desktop.refused.extend([7, 8]);
        assert_eq!(before_close(&mut desktop, 6, [7, 8]), Handoff::Rejected);
        assert_eq!(desktop.activated, [7, 8]);
        assert_eq!(desktop.foreground, Some(6));
    }

    #[test]
    fn foreground_changing_after_activation_stops_all_further_attempts() {
        let mut desktop = Desktop::new(Some(6), &[7, 8]);
        desktop.switch_after_activation = Some(Some(200));
        assert_eq!(before_close(&mut desktop, 6, [7, 8]), Handoff::Interrupted);
        assert_eq!(desktop.activated, [7]);
        assert_eq!(desktop.foreground, Some(200));
    }
}
