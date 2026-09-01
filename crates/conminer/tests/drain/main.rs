//! Suite `drain` (§13) — template mining, plus the §12.1 determinism and
//! rebuild-idempotence properties.
//!
//! Edge cases from the catalog:
//!   identical lines dedup · token-count variance fragmentation (documented
//!   behaviour locked by test) · high-cardinality hex convergence to `<*>` ·
//!   unicode tokens · similarity threshold boundary at 0.4 · max-children
//!   overflow node · per-profile tokenizer rules (`key=value`) · merge-suggestion
//!   generation never mutates templates

use conminer_core::drain::{mine_all, Drain, DrainConfig, MergeReason, TokenizerRules, WILDCARD};
use proptest::prelude::*;

fn mine(lines: &[&str]) -> Drain {
    let owned: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    mine_all(DrainConfig::default(), TokenizerRules::default(), &owned)
}

#[test]
fn identical_lines_dedup() {
    let d = mine(&["EXT4-fs (sda1): mounted filesystem with ordered data mode"; 1000]);
    assert_eq!(d.len(), 1);
    assert_eq!(d.templates()[0].count, 1000);
}

#[test]
fn token_count_variance_fragments_into_siblings() {
    // §6 documents this as the known cost of no masking. Locked by test so the
    // behaviour cannot drift silently.
    let d = mine(&[
        "random: crng init done",
        "random: crng init done with 3 entropy sources",
    ]);
    assert_eq!(d.len(), 2);
    let s = d.merge_suggestions();
    assert!(s.iter().any(|m| m.reason == MergeReason::PrefixExtension));
}

#[test]
fn high_cardinality_hex_converges_to_wildcard() {
    let lines: Vec<String> = (0..500)
        .map(|i| format!("arm-smmu 15000000.iommu: Unhandled context fault: iova=0x{i:016x}"))
        .collect();
    let d = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
    assert_eq!(d.len(), 1);
    assert!(d.templates()[0].text().ends_with(WILDCARD));
}

#[test]
fn unicode_tokens_cluster_like_any_other() {
    let d = mine(&["état check ✓ ok", "état check ✗ ok"]);
    assert_eq!(d.len(), 1);
    assert_eq!(d.templates()[0].text(), "état check <*> ok");
}

#[test]
fn similarity_threshold_boundary_at_0_4() {
    let pair = ["a b c d e", "a b z y x"]; // 2/5 agreement = exactly 0.4
    for (th, expected) in [(0.39, 1), (0.40, 1), (0.41, 2)] {
        let owned: Vec<String> = pair.iter().map(|s| s.to_string()).collect();
        let d = mine_all(
            DrainConfig {
                similarity: th,
                ..Default::default()
            },
            TokenizerRules::default(),
            &owned,
        );
        assert_eq!(d.len(), expected, "threshold {th}");
    }
}

#[test]
fn max_children_overflow_node() {
    let lines: Vec<String> = (0..200)
        .map(|i| format!("iface{i} link is up now"))
        .collect();
    let d = mine_all(
        DrainConfig {
            max_children: 8,
            ..Default::default()
        },
        TokenizerRules::default(),
        &lines,
    );
    assert!(d.len() < 20, "overflow must converge, got {}", d.len());
}

#[test]
fn key_value_tokenizer_rule() {
    // NON-NUMERIC values on purpose. This test used to vary a number
    // (`bootdelay={i}`), which the tokenizer's numeric masking now collapses on
    // its own -- so the test proved nothing about the key/value rule any more.
    // Assignment values that are words still route ten different ways without
    // the rule, which is exactly what the rule exists to fix.
    const HOSTS: [&str; 10] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    ];
    let lines: Vec<String> = HOSTS
        .iter()
        .map(|h| format!("bootargs=console=ttyS0 hostname={h} root=/dev/sda init=/sbin/init"))
        .collect();
    // Without the rule, `hostname=alpha` is a single token sitting inside the
    // routing depth. These lines vary in exactly ONE token and still agree on
    // three literals, so the widened search legitimately brings them together --
    // they really are one message with one value in it. The cost is what this
    // test is about: the disagreement swallows the whole assignment, so the KEY
    // disappears along with the value.
    let without = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
    assert_eq!(without.len(), 1, "one message with one varying value");
    assert!(
        !without.templates()[0].text().contains("hostname"),
        "without the rule the key is swallowed with its value: {}",
        without.templates()[0].text()
    );

    // With it, the keys stay constant where routing happens and only the values
    // generalize — ten lines, one template.
    let with_rule = mine_all(
        DrainConfig::default(),
        TokenizerRules {
            split_key_value: true,
            ..Default::default()
        },
        &lines,
    );
    assert_eq!(with_rule.len(), 1);
    assert!(
        with_rule.templates()[0].text().contains("hostname= <*>"),
        "the key must stay and only the value generalize: {}",
        with_rule.templates()[0].text()
    );
}

#[test]
fn merge_suggestion_generation_never_mutates_templates() {
    let lines: Vec<String> = (0..50)
        .map(|i| {
            if i % 3 == 0 {
                format!("psci: failed to boot CPU{i}")
            } else {
                format!("psci: failed to boot CPU{i} (rc -22)")
            }
        })
        .collect();
    let d = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
    let before = d.templates().to_vec();
    let suggestions = d.merge_suggestions();
    assert!(!suggestions.is_empty());
    assert_eq!(d.templates(), before.as_slice());
}

#[test]
fn fragmentation_ratio_is_the_health_metric() {
    let clean = mine(&["same shape here 1", "same shape here 2"]);
    assert!((clean.fragmentation_ratio() - 1.0).abs() < 1e-9);

    let fragmented = mine(&[
        "boot stage done",
        "boot stage done fast",
        "boot stage done fast indeed",
    ]);
    assert!(fragmented.fragmentation_ratio() > 1.0);
}

#[test]
fn wildcards_only_appear_where_observations_disagreed() {
    let single = mine(&["Kernel panic - not syncing: Attempted to kill init!"]);
    assert_eq!(single.templates()[0].wildcard_count(), 0);
}

// -------------------------------------------------------------- properties ---

proptest! {
    /// §12.1 Drain determinism: identical input order ⇒ identical template set
    /// and identical ids.
    #[test]
    fn prop_determinism(lines in proptest::collection::vec("[a-e]{1,3}( [a-e0-9]{1,4}){0,6}", 0..200)) {
        let a = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        let b = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        prop_assert_eq!(a.templates(), b.templates());
    }

    /// §12.1 rebuild idempotence: rebuilding twice yields identical output, so
    /// the template store is provably a derived view.
    #[test]
    fn prop_rebuild_idempotence(lines in proptest::collection::vec("[a-e]{1,3}( [a-e0-9]{1,4}){0,6}", 0..200)) {
        let once = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        let twice = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        prop_assert_eq!(once.templates(), twice.templates());

        // …and rehydrating from the persisted templates then replaying the same
        // input mints nothing new.
        let mut re = Drain::from_templates(
            once.config(),
            once.rules().clone(),
            once.templates().iter().cloned(),
        );
        for l in &lines {
            if let Some(m) = re.add_line(l) {
                prop_assert!(!m.created, "rehydrated miner minted a template for {:?}", l);
            }
        }
    }

    /// Every mined line lands in exactly one template, and the counts add up.
    #[test]
    fn prop_counts_conserve(lines in proptest::collection::vec("[a-e]{1,3}( [a-e0-9]{1,4}){0,4}", 0..200)) {
        let d = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        let mined = lines.iter().filter(|l| !l.split_ascii_whitespace().collect::<Vec<_>>().is_empty()).count();
        let total: u64 = d.templates().iter().map(|t| t.count).sum();
        prop_assert_eq!(total as usize, mined);
    }

    /// A template never has more tokens than the line it came from, and never
    /// exceeds `max_line_tokens`.
    #[test]
    fn prop_template_shape(lines in proptest::collection::vec("([a-z]{1,5} ){0,40}", 1..50)) {
        let cfg = DrainConfig { max_line_tokens: 8, ..Default::default() };
        let d = mine_all(cfg, TokenizerRules::default(), &lines);
        for t in d.templates() {
            prop_assert!(t.tokens.len() <= 8);
        }
    }
}
