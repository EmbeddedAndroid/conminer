//! Absence detection: what a boot *should* have printed and did not.
//!
//! Every other query here answers "what appeared". Bring-up failures are usually
//! the opposite shape: DDR training never announced itself, the SMMU never came
//! up, the handoff line is missing. A novel-template list structurally cannot
//! say that, because the evidence is the absence.
//!
//! So this learns the skeleton of a normal boot from reference epochs, and then
//! reports what a suspect epoch is missing from it.
//!
//! Two things keep the answer trustworthy:
//!
//! * **Reliability is carried, not thresholded away.** A template seen in 47 of
//!   47 good boots and one seen in 3 of 47 are different claims, and an agent
//!   needs to know which it is looking at before acting.
//! * **Late is distinguished from absent.** A line that printed three seconds
//!   later than usual is a different fault from one that never printed, so the
//!   learned model carries typical timing and the report separates the two.

use crate::error::Result;
use crate::store::DeviceStore;
use serde::Serialize;
use serde_json::{json, Value};

/// What a normal boot on this device contains.
#[derive(Debug, Clone, Serialize)]
pub struct Expectation {
    pub template_id: i64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Reference epochs that contained it, of how many were examined.
    pub seen_in: i64,
    pub reference_boots: i64,
    /// `seen_in / reference_boots`, the number an agent actually branches on.
    pub reliability: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_offset_ms: Option<i64>,
}

/// Learn the skeleton from a set of reference epochs.
///
/// The caller chooses which epochs are "normal"; that judgement is not something
/// this module should be making silently. Typically the boots that reached a
/// prompt, or the ones sharing a blessed baseline's fingerprint.
pub fn learn(store: &mut DeviceStore, boots: &[i64], now: i64) -> Result<usize> {
    let mut per_template: std::collections::BTreeMap<i64, (i64, Vec<i64>)> = Default::default();

    for &b in boots {
        let boot = store.boot(b)?;
        // Distinct templates per epoch: a line printed forty times in one boot
        // is one observation of "this boot contained it", not forty.
        let mut seen_here: std::collections::BTreeMap<i64, i64> = Default::default();
        for (tid, first_ts) in store.template_first_ts_in_boot(b)? {
            seen_here.insert(tid, (first_ts - boot.opened_at).max(0));
        }
        for (tid, offset) in seen_here {
            let e = per_template.entry(tid).or_insert((0, Vec::new()));
            e.0 += 1;
            e.1.push(offset);
        }
    }

    let rows: Vec<(i64, i64, Option<i64>)> = per_template
        .into_iter()
        .map(|(tid, (count, mut offsets))| {
            offsets.sort_unstable();
            let median = offsets.get(offsets.len() / 2).copied();
            (tid, count, median)
        })
        .collect();
    store.replace_expectations(&rows, boots.len() as i64, now)?;
    Ok(rows.len())
}

/// What `boot` is missing relative to the learned skeleton.
///
/// `min_reliability` is the cut for "normally present"; anything below it is
/// reported separately as `flaky_and_absent` rather than dropped, because a
/// line that appears in half of all good boots is evidence of a different kind.
pub fn missing_in(
    store: &DeviceStore,
    boot: i64,
    min_reliability: f64,
    late_factor: f64,
) -> Result<Value> {
    let expectations = store.expectations()?;
    if expectations.is_empty() {
        return Ok(json!({
            "learned": false,
            "why": "no reference boots have been learned yet; call learn_expectations first",
            "missing": [], "late": [], "flaky_and_absent": [],
        }));
    }

    let b = store.boot(boot)?;
    let present: std::collections::BTreeMap<i64, i64> = store
        .template_first_ts_in_boot(boot)?
        .into_iter()
        .map(|(tid, ts)| (tid, (ts - b.opened_at).max(0)))
        .collect();

    let (mut missing, mut flaky, mut late) = (Vec::new(), Vec::new(), Vec::new());
    for e in &expectations {
        match present.get(&e.template_id) {
            None => {
                let entry = json!({
                    "template_id": e.template_id,
                    "text": e.text,
                    "stage": e.stage,
                    "reliability": e.reliability,
                    "seen_in": format!("{}/{}", e.seen_in, e.reference_boots),
                });
                if e.reliability >= min_reliability {
                    missing.push(entry);
                } else {
                    flaky.push(entry);
                }
            }
            Some(&offset) => {
                // Printed, but late. A different fault from never printing, and
                // one that a set difference would silently call healthy.
                if let Some(expected) = e.median_offset_ms {
                    if expected > 0 && (offset as f64) > (expected as f64) * late_factor {
                        late.push(json!({
                            "template_id": e.template_id,
                            "text": e.text,
                            "expected_offset_ms": expected,
                            "actual_offset_ms": offset,
                            "later_by_ms": offset - expected,
                        }));
                    }
                }
            }
        }
    }
    // Most reliable first: the thing that always prints and did not this time is
    // the strongest signal available.
    missing.sort_by(|a, b| {
        b["reliability"]
            .as_f64()
            .partial_cmp(&a["reliability"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    late.sort_by_key(|v| -v["later_by_ms"].as_i64().unwrap_or(0));

    let reference_boots = expectations.first().map(|e| e.reference_boots).unwrap_or(0);
    Ok(json!({
        "learned": true,
        "boot_id": boot,
        "reference_boots": reference_boots,
        "expectations": expectations.len(),
        "min_reliability": min_reliability,
        "missing": missing,
        "late": late,
        // Never silently dropped: "sometimes absent anyway" is information.
        "flaky_and_absent": flaky,
    }))
}
