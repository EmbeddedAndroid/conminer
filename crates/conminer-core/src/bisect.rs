//! Bisect over an ordered list of builds.
//!
//! "Which of these forty BL31 builds introduced the hang" is the most expensive
//! question in bring-up and almost entirely mechanical: flash, boot, classify,
//! halve. conminer already holds the two hard parts — a *semantic* verdict for a
//! boot (fingerprints, outcomes, templates) and the flash/power hooks — so what
//! is missing is only the bookkeeping that keeps a bisect correct.
//!
//! Deliberately **not** a flashing tool (§ non-goals): this decides which
//! candidate to try next and when the answer is pinned; the lab's own tooling
//! does the flashing, through the same hooks everything else uses.
//!
//! Semantics follow `git bisect`, including the parts that are easy to get
//! wrong:
//!
//! * The search is over the half-open range `(last good, first bad]`, so the
//!   answer is the *first bad* candidate rather than the last good one.
//! * `skip` does not collapse the range. An unflashable or wedged candidate is
//!   stepped around, and if the range is exhausted with only skips left the
//!   result is `inconclusive` — never a guess.
//! * A verdict that contradicts an earlier one is reported rather than
//!   overwritten silently, because on flaky hardware that is the finding.

use crate::error::{ErrorCode, Result, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BisectResult {
    pub idx: usize,
    /// `good` | `bad` | `skip`
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<i64>,
    pub at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bisect {
    pub id: i64,
    pub name: String,
    /// Ordered oldest first: the axis being searched.
    pub candidates: Vec<String>,
    pub predicate: Value,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub culprit: Option<String>,
    /// `running` | `done` | `aborted` | `inconclusive`
    pub state: String,
    pub results: Vec<BisectResult>,
}

/// What to do next.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Step {
    /// Flash and boot this candidate, then report its verdict.
    Test {
        index: usize,
        candidate: String,
        /// Candidates still in the range, so an agent can see the search shrink.
        remaining: usize,
        /// Upper bound on further tests, which is what makes waiting bearable.
        max_further_tests: usize,
    },
    /// The first bad candidate is pinned.
    Found {
        index: usize,
        candidate: String,
        /// The last known-good candidate, when one was tested.
        last_good: Option<String>,
    },
    /// Only skipped candidates remain between the last good and the first bad.
    Inconclusive {
        between: (Option<String>, Option<String>),
        skipped: Vec<String>,
    },
    /// Nothing has been established yet: the ends must be tested first.
    NeedEnds { untested_ends: Vec<usize> },
}

impl Bisect {
    fn verdict(&self, idx: usize) -> Option<&str> {
        self.results
            .iter()
            .find(|r| r.idx == idx)
            .map(|r| r.verdict.as_str())
    }

    /// Highest index known good, and lowest index known bad.
    fn bounds(&self) -> (Option<usize>, Option<usize>) {
        let good = self
            .results
            .iter()
            .filter(|r| r.verdict == "good")
            .map(|r| r.idx)
            .max();
        let bad = self
            .results
            .iter()
            .filter(|r| r.verdict == "bad")
            .map(|r| r.idx)
            .min();
        (good, bad)
    }

    /// Detect a verdict that contradicts the established ordering: a `good`
    /// after a known `bad`, or a `bad` before a known `good`. On flaky hardware
    /// this is the finding, not an error to paper over.
    pub fn contradiction(&self) -> Option<String> {
        let (good, bad) = self.bounds();
        match (good, bad) {
            (Some(g), Some(b)) if g >= b => Some(format!(
                "candidate {} tested good but candidate {} (at or before it) tested bad: \
                 the failure is not monotonic along this axis, which usually means it is \
                 intermittent",
                self.candidates.get(g).cloned().unwrap_or_default(),
                self.candidates.get(b).cloned().unwrap_or_default(),
            )),
            _ => None,
        }
    }

    /// The next thing to do.
    pub fn next_step(&self) -> Step {
        let n = self.candidates.len();
        let (good, bad) = self.bounds();

        // Both ends must be established before a bisect means anything: without
        // a known-good and a known-bad there is no range to halve.
        if good.is_none() || bad.is_none() {
            let mut ends = Vec::new();
            if good.is_none() && self.verdict(0).is_none() {
                ends.push(0);
            }
            if bad.is_none() && n > 0 && self.verdict(n - 1).is_none() {
                ends.push(n - 1);
            }
            if !ends.is_empty() {
                let idx = ends[0];
                return Step::Test {
                    index: idx,
                    candidate: self.candidates[idx].clone(),
                    remaining: n,
                    max_further_tests: bits(n) + 1,
                };
            }
            return Step::NeedEnds {
                untested_ends: ends,
            };
        }

        let (g, b) = (good.unwrap(), bad.unwrap());
        if g >= b {
            // Contradictory; report the culprit as the first bad anyway, but the
            // caller surfaces `contradiction` alongside it.
            return Step::Found {
                index: b,
                candidate: self.candidates[b].clone(),
                last_good: self.candidates.get(g).cloned(),
            };
        }

        // Untested, unskipped candidates strictly between the bounds.
        let open: Vec<usize> = ((g + 1)..b)
            .filter(|i| self.verdict(*i).is_none())
            .collect();
        if open.is_empty() {
            let skipped: Vec<String> = ((g + 1)..b)
                .filter(|i| self.verdict(*i) == Some("skip"))
                .filter_map(|i| self.candidates.get(i).cloned())
                .collect();
            if skipped.is_empty() {
                return Step::Found {
                    index: b,
                    candidate: self.candidates[b].clone(),
                    last_good: self.candidates.get(g).cloned(),
                };
            }
            // Everything between is skipped: the answer is somewhere in there and
            // this search cannot say where. Saying so beats naming a candidate.
            return Step::Inconclusive {
                between: (
                    self.candidates.get(g).cloned(),
                    self.candidates.get(b).cloned(),
                ),
                skipped,
            };
        }

        // Middle of the open range, then the nearest untested candidate to it, so
        // a skipped midpoint steps aside instead of stalling.
        let mid = (g + b) / 2;
        let idx = *open
            .iter()
            .min_by_key(|i| (**i as i64 - mid as i64).abs())
            .expect("non-empty");
        Step::Test {
            index: idx,
            candidate: self.candidates[idx].clone(),
            remaining: open.len(),
            max_further_tests: bits(open.len()),
        }
    }

    /// Resolve a candidate reference to its index, accepting either the exact
    /// string or the position.
    pub fn index_of(&self, reference: &str) -> Result<usize> {
        if let Some(i) = self.candidates.iter().position(|c| c == reference) {
            return Ok(i);
        }
        if let Ok(i) = reference.parse::<usize>() {
            if i < self.candidates.len() {
                return Ok(i);
            }
        }
        Err(ToolError::new(
            ErrorCode::InvalidArgument,
            format!("{reference:?} is not one of this bisect's candidates"),
        )
        .with_detail(serde_json::json!({"candidates": self.candidates})))
    }
}

/// Tests still needed to halve `n` candidates.
fn bits(n: usize) -> usize {
    let mut n = n;
    let mut c = 0;
    while n > 1 {
        n /= 2;
        c += 1;
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(candidates: &[&str], verdicts: &[(usize, &str)]) -> Bisect {
        Bisect {
            id: 1,
            name: "t".into(),
            candidates: candidates.iter().map(|s| s.to_string()).collect(),
            predicate: Value::Null,
            started_at: 0,
            finished_at: None,
            culprit: None,
            state: "running".into(),
            results: verdicts
                .iter()
                .map(|(i, v)| BisectResult {
                    idx: *i,
                    verdict: (*v).to_string(),
                    boot_id: None,
                    at: 0,
                    note: None,
                })
                .collect(),
        }
    }

    const C: &[&str] = &["v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8"];

    #[test]
    fn the_ends_are_tested_before_anything_is_halved() {
        // Without a known-good and a known-bad there is no range, and halving
        // nothing would just pick an arbitrary build to flash.
        let s = b(C, &[]);
        assert!(matches!(s.next_step(), Step::Test { index: 0, .. }));
        let s = b(C, &[(0, "good")]);
        assert!(matches!(s.next_step(), Step::Test { index: 7, .. }));
    }

    #[test]
    fn a_bounded_range_is_halved() {
        let s = b(C, &[(0, "good"), (7, "bad")]);
        match s.next_step() {
            Step::Test { index, .. } => assert_eq!(index, 3, "the midpoint of (0,7)"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_answer_is_the_first_bad_not_the_last_good() {
        let s = b(C, &[(3, "good"), (4, "bad")]);
        match s.next_step() {
            Step::Found {
                index, candidate, ..
            } => {
                assert_eq!(index, 4);
                assert_eq!(candidate, "v5");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_skip_steps_aside_without_collapsing_the_range() {
        let s = b(C, &[(0, "good"), (7, "bad"), (3, "skip")]);
        match s.next_step() {
            Step::Test { index, .. } => {
                assert_ne!(index, 3, "the skipped candidate is not offered again");
                assert!((1..7).contains(&index));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_range_of_only_skips_is_inconclusive_not_a_guess() {
        let s = b(C, &[(2, "good"), (5, "bad"), (3, "skip"), (4, "skip")]);
        match s.next_step() {
            Step::Inconclusive { skipped, between } => {
                assert_eq!(skipped, vec!["v4", "v5"]);
                assert_eq!(between, (Some("v3".into()), Some("v6".into())));
            }
            other => panic!("naming a culprit here would be a guess: {other:?}"),
        }
    }

    #[test]
    fn a_non_monotonic_result_is_reported_rather_than_hidden() {
        // good *after* bad: the failure is intermittent, and silently trusting
        // the halving would pin an innocent build.
        let s = b(C, &[(5, "good"), (2, "bad")]);
        let c = s.contradiction().expect("a contradiction");
        assert!(c.contains("not monotonic"), "{c}");
    }

    #[test]
    fn candidates_resolve_by_name_or_position() {
        let s = b(C, &[]);
        assert_eq!(s.index_of("v3").unwrap(), 2);
        assert_eq!(s.index_of("2").unwrap(), 2);
        let e = s.index_of("nope").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.detail.is_some(), "the valid set is attached");
    }
}
