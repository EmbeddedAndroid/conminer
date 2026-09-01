//! Boot-stage tracking (§5).
//!
//! Embedded consoles change dialect mid-stream: BootROM → TF-A → U-Boot →
//! kernel → userspace, or MCUboot → Zephyr. The stage machine watches banner
//! signatures, switches the active profile, and tags every record with its
//! stage — so "did it die in BL31 or after handoff?" is a one-call answer.
//!
//! Reset detection falls out of the same mechanism (§A.0 RESET-MARKER): a banner
//! for a stage at or before the earliest one already seen means the target
//! restarted. That is where boot-loop detection comes from for free.

use super::profile::{Profile, ProfileSet};
use super::StageTransition;
use crate::error::Result;
use std::sync::Arc;

#[derive(Debug)]
pub struct StageMachine {
    set: Arc<ProfileSet>,
    /// When pinned, dialect auto-detection is off entirely (§5).
    pinned: Option<Arc<Profile>>,
    active: Arc<Profile>,
    stage: String,
    /// Rank of the earliest stage seen since the last reset.
    min_rank_seen: Option<i32>,
    /// Rank of the stage currently in force. `None` until the first banner —
    /// the first stage of a capture can never be a reset.
    current_rank: Option<i32>,
    /// Banners already fired in this epoch, as (profile, banner index).
    ///
    /// Keying on the individual banner — not the stage — is what separates
    /// "`Linux version` then `Booting Linux on physical CPU`", which is one boot
    /// printing two banners, from the *same* banner arriving twice, which is the
    /// target having restarted.
    seen_banners: std::collections::BTreeSet<(String, usize)>,
    visited: Vec<String>,
}

impl StageMachine {
    pub fn new(set: Arc<ProfileSet>, pinned: Option<&str>) -> Result<Self> {
        let pinned = match pinned {
            Some(name) => Some(set.require(name)?),
            None => None,
        };
        let active = match &pinned {
            Some(p) => p.clone(),
            None => set.require("raw")?,
        };
        let stage = active.stage.clone();
        Ok(Self {
            set,
            pinned,
            active,
            stage,
            min_rank_seen: None,
            current_rank: None,
            seen_banners: std::collections::BTreeSet::new(),
            visited: Vec::new(),
        })
    }

    pub fn stage(&self) -> &str {
        &self.stage
    }

    pub fn active_profile(&self) -> Arc<Profile> {
        self.active.clone()
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    pub fn visited(&self) -> &[String] {
        &self.visited
    }

    /// Offer a line. Returns a transition when the stage changes or the target
    /// visibly restarted.
    pub fn observe(&mut self, text: &str, line_id: i64, at: i64) -> Option<StageTransition> {
        // Pinned: no dialect switching, but an explicit reset marker is still a
        // fact about the target and must not be swallowed.
        if let Some(p) = self.pinned.clone() {
            if p.is_reset_marker(text) {
                self.begin_epoch(p.stage_rank, &p.stage.clone());
                return Some(StageTransition {
                    name: self.stage.clone(),
                    profile: p.name.clone(),
                    banner_line_id: line_id,
                    at,
                    is_reset: true,
                    stage_changed: false,
                });
            }
            return None;
        }

        // A banner is the strong signal: it names the dialect that follows.
        let hit = self.set.detectors().filter(|p| !p.overlay).find_map(|p| {
            p.banners
                .iter()
                .position(|b| b.re.is_match(text))
                .map(|i| (p.clone(), i, p.banners[i].stage.clone(), p.banners[i].rank))
        });

        if let Some((profile, banner_idx, stage, rank)) = hit {
            let key = (profile.name.clone(), banner_idx);
            // An involuntary reboot, by either of the two signals that mean it:
            //
            //  * the boot order went *backwards* — you do not get from the kernel
            //    back to BL1 without a reset;
            //  * or the same banner arrived twice at or before the earliest stage
            //    of this epoch, which is a bootloader looping in place.
            let went_backwards = self.current_rank.is_some_and(|cur| rank < cur);
            let repeated = self.min_rank_seen.is_some_and(|min| rank <= min)
                && self.seen_banners.contains(&key);
            let is_reset = went_backwards || repeated;
            let stage_changed = stage != self.stage;

            if is_reset {
                self.begin_epoch(rank, &stage);
            } else {
                self.min_rank_seen = Some(self.min_rank_seen.map_or(rank, |m| m.min(rank)));
                if !self.visited.iter().any(|v| v == &stage) {
                    self.visited.push(stage.clone());
                }
            }
            self.seen_banners.insert(key);
            self.active = profile.clone();
            self.stage = stage.clone();
            self.current_rank = Some(rank);

            if !is_reset && !stage_changed {
                // A second banner for the stage we are already in — Linux prints
                // both `Linux version` and `Booting Linux on physical CPU` — is
                // not a transition and must not mint a duplicate stage row.
                return None;
            }
            return Some(StageTransition {
                name: stage,
                profile: profile.name.clone(),
                banner_line_id: line_id,
                at,
                is_reset,
                stage_changed,
            });
        }

        // No banner, but the active dialect says the target is resetting.
        if self.active.is_reset_marker(text) {
            let rank = self.current_rank.unwrap_or(self.active.stage_rank);
            let stage = self.active.stage.clone();
            self.begin_epoch(rank, &stage);
            return Some(StageTransition {
                name: self.stage.clone(),
                profile: self.active.name.clone(),
                banner_line_id: line_id,
                at,
                is_reset: true,
                stage_changed: false,
            });
        }

        None
    }

    fn begin_epoch(&mut self, rank: i32, stage: &str) {
        self.min_rank_seen = Some(rank);
        self.current_rank = Some(rank);
        self.seen_banners.clear();
        self.visited.clear();
        self.visited.push(stage.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> StageMachine {
        let set = Arc::new(ProfileSet::builtin().unwrap());
        StageMachine::new(set, None).unwrap()
    }

    #[test]
    fn full_boot_chain_is_tracked_in_order() {
        let mut m = machine();
        let chain = [
            ("NOTICE:  BL1: v2.11(release):v2.11", "bl1"),
            ("NOTICE:  BL2: v2.11(release):v2.11", "bl2"),
            ("NOTICE:  BL31: v2.11(release):v2.11", "bl31"),
            ("U-Boot 2026.01 (Jan 01 2026 - 00:00:00 +0000)", "uboot"),
            ("Linux version 6.12.9 (build@host) (gcc 14)", "kernel"),
        ];
        for (line, expect) in chain {
            let t = m
                .observe(line, 1, 0)
                .unwrap_or_else(|| panic!("no transition for {line:?}"));
            assert_eq!(t.name, expect, "{line:?}");
            assert!(!t.is_reset);
            assert!(t.stage_changed);
        }
        assert_eq!(m.stage(), "kernel");
    }

    #[test]
    fn earliest_stage_banner_recurrence_is_a_reset() {
        let mut m = machine();
        m.observe("NOTICE:  BL1: v2.11(release):v2.11", 1, 0);
        m.observe("U-Boot 2026.01 (Jan 01 2026 - 00:00:00 +0000)", 2, 1);
        m.observe("Linux version 6.12.9 (build@host) (gcc 14)", 3, 2);

        let t = m
            .observe("NOTICE:  BL1: v2.11(release):v2.11", 4, 3)
            .expect("reset must be reported");
        assert!(t.is_reset, "the earliest banner reappearing is a reboot");
        assert_eq!(t.name, "bl1");

        // …and the epoch restarts, so the same chain is not a reset second time.
        let t = m.observe("U-Boot 2026.01 (x)", 5, 4).unwrap();
        assert!(!t.is_reset);
    }

    #[test]
    fn a_later_stage_banner_is_progress_not_a_reset() {
        let mut m = machine();
        m.observe("U-Boot 2026.01 (x)", 1, 0);
        let t = m.observe("Linux version 6.12.9 (b@h) (gcc)", 2, 1).unwrap();
        assert!(!t.is_reset);
    }

    #[test]
    fn the_same_banner_twice_is_a_reset() {
        // This is the mechanism boot-loop detection is built on (§A.0).
        let mut m = machine();
        assert!(!m.observe("U-Boot 2026.01 (x)", 1, 0).unwrap().is_reset);
        let t = m.observe("U-Boot 2026.01 (x)", 2, 1).expect("reset");
        assert!(t.is_reset);
        assert!(!t.stage_changed);
    }

    #[test]
    fn a_second_banner_for_the_current_stage_is_not_a_transition() {
        // Linux prints two banners on one boot; the second must not mint a
        // duplicate stage row and must not be mistaken for a reboot.
        let mut m = machine();
        assert_eq!(
            m.observe("Linux version 6.12.9 (b@h) (gcc)", 1, 0)
                .unwrap()
                .name,
            "kernel"
        );
        assert!(m
            .observe("Booting Linux on physical CPU 0x0", 2, 1)
            .is_none());
        assert_eq!(m.stage(), "kernel");
    }

    #[test]
    fn explicit_reset_marker_without_a_banner_still_reports() {
        let mut m = machine();
        m.observe("U-Boot 2026.01 (x)", 1, 0);
        let t = m.observe("resetting ...", 2, 1).expect("reset marker");
        assert!(t.is_reset);
        assert!(!t.stage_changed);
    }

    #[test]
    fn pinned_profile_disables_detection() {
        let set = Arc::new(ProfileSet::builtin().unwrap());
        let mut m = StageMachine::new(set, Some("zephyr")).unwrap();
        assert!(m.is_pinned());
        assert_eq!(m.stage(), "zephyr");
        assert!(
            m.observe("Linux version 6.12.9 (b@h) (gcc)", 1, 0)
                .is_none(),
            "a pinned device must not switch dialect on a stray banner"
        );
        assert_eq!(m.stage(), "zephyr");
    }

    #[test]
    fn pinned_profile_still_reports_resets() {
        let set = Arc::new(ProfileSet::builtin().unwrap());
        let mut m = StageMachine::new(set, Some("zephyr")).unwrap();
        let t = m.observe("Halting system", 1, 0).expect("reset marker");
        assert!(t.is_reset);
    }

    #[test]
    fn unknown_pinned_profile_is_a_structured_error() {
        let set = Arc::new(ProfileSet::builtin().unwrap());
        let err = StageMachine::new(set, Some("nope")).unwrap_err();
        assert_eq!(err.code, crate::ErrorCode::InvalidArgument);
        assert!(err.detail.unwrap()["available"].is_array());
    }

    #[test]
    fn ambiguous_banner_resolves_by_stage_order_deterministically() {
        // Two profiles could claim a bare version line; the lowest stage_rank
        // wins, and the choice is stable across runs.
        let mut a = machine();
        let mut b = machine();
        let line = "U-Boot SPL 2026.01 (Jan 01 2026 - 00:00:00 +0000)";
        assert_eq!(
            a.observe(line, 1, 0).map(|t| t.name),
            b.observe(line, 1, 0).map(|t| t.name)
        );
    }
}
