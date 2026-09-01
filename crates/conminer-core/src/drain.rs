//! Drain template mining, without masking (§6).
//!
//! Drain's published preprocessing step rewrites variable-looking tokens (`<IP>`,
//! `<HEX>`, `<NUM>`) before clustering. That is an accuracy aid, not part of the
//! algorithm, and it is exactly what §6 forbids. What we run is the algorithm
//! itself: a fixed-depth prefix tree keyed on token count then leading tokens,
//! with a similarity threshold at the leaf and a `<*>` overflow child once a node
//! exceeds `max_children`. Wildcards appear only where two real lines disagreed.
//!
//! Consequences we accept, and test for:
//!   * messages whose variable part changes token *count* fragment into sibling
//!     templates. That behaviour is locked by test, and surfaced as **merge
//!     suggestions** — data for an agent, never an automatic rewrite.
//!   * high-cardinality hex tokens converge to `<*>` by disagreement rather than
//!     by a `<HEX>` mask, which takes two observations instead of one.
//!
//! Templates are a derived view: `rebuild` from raw reproduces them exactly, so
//! deleting the template store loses nothing but an index.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The wildcard that appears where observed tokens disagreed.
pub const WILDCARD: &str = "<*>";

/// Per-profile tokenizer rules (§6). These change how a line is *split*, never
/// what is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TokenizerRules {
    /// Split `key=value` runs into `key=` and `value` so environment dumps do not
    /// mint one template per variable (§A.10 `KEY_VALUE_RUN`).
    #[serde(default)]
    pub split_key_value: bool,
    /// Extra characters treated as token separators, in addition to whitespace.
    #[serde(default)]
    pub extra_delimiters: Vec<char>,
}

impl TokenizerRules {
    /// A `KEY_VALUE_RUN` needs at least this many `k=v` tokens before splitting
    /// kicks in, so an ordinary sentence containing one `=` is left alone.
    const KV_RUN_MIN: usize = 3;

    /// Tokens for clustering: escapes stripped, literals LEFT ALONE.
    ///
    /// Numeric pre-masking was tried here and reverted. It collapsed the
    /// over-split templates as intended, but it violates the rule this miner is
    /// built on: a wildcard means TWO OBSERVATIONS DISAGREED, not that a regex
    /// judged a token variable. The cost is concrete -- with 64 faulting
    /// addresses drain yields
    ///     "Unhandled fault: at <*> esr 0x96000010"
    /// keeping the constant error code literal, whereas masking blanks BOTH and
    /// throws away the one number an engineer needs. Constants that happen to be
    /// numeric (error codes, versions, fixed addresses) are exactly what
    /// disagreement-based wildcarding preserves.
    ///
    /// The over-splitting in #32 is still real; the fix has to make instances
    /// REACH the same cluster (routing/similarity), not blank their values up
    /// front.
    pub fn tokenize(&self, line: &str) -> Vec<String> {
        self.tokenize_inner(line, false)
    }

    /// Tokens with their VALUES INTACT, for reading what actually sat in a
    /// wildcard slot.
    ///
    /// Masking exists to make lines cluster; it must never be the only copy.
    /// Value extraction aligns raw tokens to a template's slots positionally, so
    /// it needs the same split with the literals still present -- otherwise the
    /// number behind `<*>` reads back as `<*>` and a numeric series (min, max,
    /// drift) becomes null. Both paths must split identically, which is why this
    /// shares the implementation rather than duplicating it.
    pub fn tokenize_values(&self, line: &str) -> Vec<String> {
        self.tokenize_inner(line, false)
    }

    fn tokenize_inner(&self, line: &str, mask: bool) -> Vec<String> {
        // Escapes are DISPLAY CONTROL, not content, and they must not reach the
        // clusterer.
        //
        // They carry per-render variance -- cursor positions, column numbers --
        // so two prints of the same message look like different lines. Measured
        // on a real boot: one systemd banner split into 24 templates and one
        // "read descriptors" line into 14, purely on the escapes wrapped around
        // them. Across the device 403 templates collapsed to 274 once escapes
        // and numerics were normalised, a 32% inflation.
        //
        // The RAW bytes keep their escapes -- storage is verbatim and the web
        // terminal needs them. This strips only the copy that gets mined, so a
        // template reads as the line a human would say it is.
        let cleaned = strip_ansi(line);
        let line: &str = &cleaned;
        let base: Vec<&str> = if self.extra_delimiters.is_empty() {
            line.split_ascii_whitespace().collect()
        } else {
            line.split(|c: char| c.is_ascii_whitespace() || self.extra_delimiters.contains(&c))
                .filter(|s| !s.is_empty())
                .collect()
        };

        let keep = |t: &str| -> String {
            if mask {
                mask_numeric(t)
            } else {
                t.to_string()
            }
        };

        if !self.split_key_value {
            return base.into_iter().map(keep).collect();
        }

        let kv_count = base.iter().filter(|t| is_key_value(t)).count();
        if kv_count < Self::KV_RUN_MIN {
            return base.into_iter().map(keep).collect();
        }

        let mut out = Vec::with_capacity(base.len() + kv_count);
        for t in base {
            if is_key_value(t) {
                let idx = t.find('=').expect("is_key_value checked");
                out.push(t[..=idx].to_string()); // "key="
                out.push(keep(&t[idx + 1..])); // "value"
            } else {
                out.push(keep(t));
            }
        }
        out
    }
}

/// Remove ANSI/VT escape sequences, leaving the text they decorate.
///
/// Handles CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL|ST`) and the short
/// two-character forms, plus a bare `ESC` at end of input. Anything it does not
/// recognise is left alone: dropping bytes we do not understand would lose
/// content, and this runs on the mining path where a wrong guess is silent.
pub fn strip_ansi(line: &str) -> String {
    if !line.contains('\x1b') {
        return line.to_string(); // the common case pays nothing
    }
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match it.peek() {
            // CSI: parameters and intermediates, then a final byte @..~
            Some('[') => {
                it.next();
                for f in it.by_ref() {
                    if ('\x40'..='\x7e').contains(&f) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST (ESC \)
            Some(']') => {
                it.next();
                while let Some(f) = it.next() {
                    if f == '\x07' {
                        break;
                    }
                    if f == '\x1b' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            // Two-character escapes: ESC 7, ESC =, ESC \ and friends.
            Some(_) => {
                it.next();
            }
            // A trailing ESC with nothing after it: drop it.
            None => {}
        }
    }
    out
}

/// Replace numeric and hex literals inside a token with the wildcard.
///
/// Drain only wildcards a position AFTER two instances cluster there, and a
/// token that is different every time prevents the first cluster from ever
/// forming. So the varying part has to be masked before clustering, which is the
/// standard remedy for exactly this failure.
///
/// Measured on a real boot, these each minted their own template per instance:
///     "Elf copied from 0x... to 0x... - size 12345"        16 templates
///     "qhee_hyp_assign_remove_memory: 3/4 -> ret 0"         9 templates
///     "Memory: 1234K/5678K available (900K kernel code...)" 6 templates
/// and kernel timestamps `[ 1.234]` did the same to every line carrying one.
/// Note wildcarding already worked elsewhere ("Fixed dependency cycle(s) with
/// <*>"), so this was inconsistent masking rather than a missing feature.
///
/// Collapsing `CPU0` and `CPU1` into one template is INTENDED: a table of
/// contents wants "this happened 4 times", not four near-identical rows. The
/// verbatim line is always one `get_records` away, and raw bytes are untouched.
fn mask_numeric(tok: &str) -> String {
    if !tok.bytes().any(|b| b.is_ascii_digit()) {
        return tok.to_string(); // the common case allocates nothing extra
    }
    let mut out = String::with_capacity(tok.len());
    let b = tok.as_bytes();
    let mut i = 0;
    let mut prev_is_letter = false;
    while i < b.len() {
        // 0x-prefixed hex is one literal, not "0" then "xffff".
        if b[i] == b'0'
            && i + 2 < b.len()
            && (b[i + 1] | 0x20) == b'x'
            && (b[i + 2] as char).is_ascii_hexdigit()
        {
            i += 2;
            while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                i += 1;
            }
            out.push_str(WILDCARD);
            prev_is_letter = false;
            continue;
        }
        if b[i].is_ascii_digit() {
            // A digit run directly after a LETTER is part of an identifier, not
            // a value: tsens0, eth0, CPU0, ttyUSB5. Masking those collapses
            // "which one" into "one of them" -- absence detection caught this
            // immediately, because "tsens0 missing" and "tsens1 missing" became
            // the same claim. Values are what vary; names are what you look up.
            // Tracked as a CHARACTER, not a byte: the trailing byte of a
            // multi-byte letter is not ascii-alphabetic, so a byte test called
            // "café1" a value and masked it.
            let after_letter = prev_is_letter;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if after_letter {
                out.push_str(&tok[start..i]);
            } else {
                out.push_str(WILDCARD);
            }
            prev_is_letter = false;
            continue;
        }
        // Not a digit: copy the character whole, respecting UTF-8.
        let start = i;
        i += 1;
        while i < b.len() && (b[i] & 0xC0) == 0x80 {
            i += 1;
        }
        let ch = &tok[start..i];
        prev_is_letter = ch.chars().next().is_some_and(char::is_alphabetic);
        out.push_str(ch);
    }
    out
}

fn is_key_value(t: &str) -> bool {
    match t.find('=') {
        Some(0) => false,
        Some(i) => {
            i + 1 < t.len()
                && t[..i]
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-')
        }
        None => false,
    }
}

/// Drain parameters (§16 `[mine]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrainConfig {
    pub similarity: f64,
    pub depth: usize,
    pub max_children: usize,
    pub max_line_tokens: usize,
}

impl Default for DrainConfig {
    fn default() -> Self {
        Self {
            similarity: 0.4,
            depth: 4,
            max_children: 100,
            max_line_tokens: 128,
        }
    }
}

impl From<&crate::config::MineConfig> for DrainConfig {
    fn from(m: &crate::config::MineConfig) -> Self {
        Self {
            similarity: m.similarity,
            depth: m.depth,
            max_children: m.max_children,
            max_line_tokens: m.max_line_tokens,
        }
    }
}

/// A mined template. `id` is stable for the lifetime of the device's store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: u64,
    pub tokens: Vec<String>,
    pub count: u64,
    /// The mined line exceeded `max_line_tokens` and was clustered on its head.
    pub head_only: bool,
}

impl Template {
    pub fn text(&self) -> String {
        self.tokens.join(" ")
    }

    pub fn wildcard_count(&self) -> usize {
        self.tokens.iter().filter(|t| *t == WILDCARD).count()
    }

    /// Non-wildcard tokens, in order — the "shape" used for merge suggestions.
    pub fn skeleton(&self) -> Vec<&str> {
        self.tokens
            .iter()
            .filter(|t| *t != WILDCARD)
            .map(String::as_str)
            .collect()
    }
}

/// What happened when a line was mined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainMatch {
    pub template_id: u64,
    /// First time this template has ever been seen.
    pub created: bool,
    /// An existing template gained a wildcard because of this line.
    pub generalized: bool,
}

/// A suggestion that two templates are the same message fragmented by Drain's
/// token-count layer. Surfaced as data; nothing is ever merged automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeSuggestion {
    pub a: u64,
    pub b: u64,
    pub reason: MergeReason,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeReason {
    /// Identical non-wildcard skeletons at different token counts — the exact
    /// fragmentation mode §6 documents.
    SameSkeleton,
    /// One template's skeleton is a prefix of the other's: the same message with
    /// extra trailing detail ("failed to boot CPU3" vs "…CPU3 (rc -22)"). This is
    /// how token-count variance usually presents in firmware logs.
    PrefixExtension,
    /// Same token count, similarity just below the clustering threshold.
    NearThreshold,
}

#[derive(Debug, Default, Clone)]
struct Node {
    children: BTreeMap<String, Node>,
    /// Leaf payload: indices into `Drain::templates`.
    clusters: Vec<usize>,
}

/// The miner. Deterministic: identical input order yields identical templates
/// and identical ids.
#[derive(Debug, Clone)]
pub struct Drain {
    cfg: DrainConfig,
    rules: TokenizerRules,
    /// token-count → subtree
    roots: BTreeMap<usize, Node>,
    templates: Vec<Template>,
    next_id: u64,
}

impl Drain {
    pub fn new(cfg: DrainConfig) -> Self {
        Self::with_rules(cfg, TokenizerRules::default())
    }

    pub fn with_rules(cfg: DrainConfig, rules: TokenizerRules) -> Self {
        Self {
            cfg,
            rules,
            roots: BTreeMap::new(),
            templates: Vec::new(),
            next_id: 1,
        }
    }

    pub fn config(&self) -> DrainConfig {
        self.cfg
    }

    pub fn rules(&self) -> &TokenizerRules {
        &self.rules
    }

    /// Rehydrate from persisted templates so a restarted minerd keeps stable ids
    /// without replaying raw. Tree placement is a pure function of the template
    /// tokens, so this reproduces the live tree exactly.
    pub fn from_templates(
        cfg: DrainConfig,
        rules: TokenizerRules,
        stored: impl IntoIterator<Item = Template>,
    ) -> Self {
        let mut d = Self::with_rules(cfg, rules);
        let mut stored: Vec<Template> = stored.into_iter().collect();
        stored.sort_by_key(|t| t.id);
        for t in stored {
            let idx = d.templates.len();
            d.next_id = d.next_id.max(t.id + 1);
            let tokens = t.tokens.clone();
            d.templates.push(t);
            d.leaf_for(&tokens, true).unwrap().clusters.push(idx);
        }
        d
    }

    pub fn templates(&self) -> &[Template] {
        &self.templates
    }

    pub fn template(&self, id: u64) -> Option<&Template> {
        self.templates.iter().find(|t| t.id == id)
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Mine one line of text. Returns `None` for a line with no tokens at all —
    /// blank lines carry no message and would otherwise all collapse into one
    /// meaningless template.
    pub fn add_line(&mut self, line: &str) -> Option<DrainMatch> {
        let tokens = self.rules.tokenize(line);
        if tokens.is_empty() {
            return None;
        }
        Some(self.add_tokens(&tokens))
    }

    /// Mine a pre-tokenized line.
    pub fn add_tokens(&mut self, tokens: &[String]) -> DrainMatch {
        let head_only = tokens.len() > self.cfg.max_line_tokens;
        let mined: Vec<String> = if head_only {
            tokens[..self.cfg.max_line_tokens].to_vec()
        } else {
            tokens.to_vec()
        };

        let (candidates, widened) = self.candidates_for(&mined);

        match self.best_match(&candidates, &mined, widened) {
            Some(idx) => {
                let mut generalized = false;
                {
                    let t = &mut self.templates[idx];
                    for (i, tok) in mined.iter().enumerate() {
                        if t.tokens[i] != WILDCARD && &t.tokens[i] != tok {
                            t.tokens[i] = WILDCARD.to_string();
                            generalized = true;
                        }
                    }
                    t.count += 1;
                    t.head_only |= head_only;
                }
                DrainMatch {
                    template_id: self.templates[idx].id,
                    created: false,
                    generalized,
                }
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                let idx = self.templates.len();
                self.templates.push(Template {
                    id,
                    tokens: mined.clone(),
                    count: 1,
                    head_only,
                });
                self.leaf_for(&mined, true).unwrap().clusters.push(idx);
                DrainMatch {
                    template_id: id,
                    created: true,
                    generalized: false,
                }
            }
        }
    }

    /// Clusters this line should be compared against.
    ///
    /// Normally the leaf's own clusters, exactly as Drain intends. The addition
    /// is what happens when the exact route does NOT exist: rather than minting
    /// a fresh branch immediately, the line is compared against the clusters
    /// under the deepest node it did reach.
    ///
    /// WHY: the prefix tree routes on leading tokens, so a line whose routing
    /// prefix holds a VALUE takes a different branch on every occurrence and
    /// never meets its own siblings. Nothing ever disagrees, so no wildcard can
    /// form, and the same message mints a template per value. Measured on a real
    /// boot: "Elf copied from 0x… to 0x… - size N" produced 16 templates,
    /// "[ 1.234] read descriptors" produced 14. Drain's max_children rule is
    /// meant to catch this, but at 100 a position with 16 distinct values never
    /// overflows.
    ///
    /// This keeps the miner's rule intact: the sibling is only joined when the
    /// REST of the line already agrees (seq_distance >= similarity), and the
    /// wildcard still appears because two real observations disagreed at that
    /// position. A constant that merely looks numeric is untouched -- it never
    /// varies, so the exact child always exists and this path never runs. That
    /// is what keeps "Unhandled fault: at <*> esr 0x96000010" holding on to its
    /// error code.
    fn candidates_for(&self, tokens: &[String]) -> (Vec<usize>, bool) {
        let n = tokens.len();
        let Some(root) = self.roots.get(&n) else {
            return (Vec::new(), false);
        };
        let max_prefix = self.cfg.depth.saturating_sub(2);
        let mut cur = root;
        // Clusters gathered from wildcard branches passed on the way down.
        //
        // A GENERALIZED template lives under `<*>` at the position it
        // generalized, while an exact branch for some other value may also
        // exist. Descending only the exact branch walks straight past the
        // template this line belongs to -- which is exactly what the rehydration
        // property caught: replaying the very input that produced a template
        // minted a second one, because after persistence the template sat under
        // `<*>` and the replay followed its literal token instead.
        let mut also: Vec<usize> = Vec::new();
        for token in tokens.iter().take(max_prefix.min(n)) {
            if let Some(w) = cur.children.get(WILDCARD) {
                if token != WILDCARD {
                    also.extend(self.subtree_clusters(w));
                }
            }
            if let Some(next) = cur.children.get(token) {
                cur = next;
                continue;
            }
            if let Some(next) = cur.children.get(WILDCARD) {
                // An overflowed node already funnels here; that is the leaf.
                cur = next;
                continue;
            }
            // No route for this token: widen to everything under the node we
            // did reach, so a line that differs only in a value can still find
            // the sibling it belongs with.
            let mut out = self.subtree_clusters(cur);
            out.append(&mut also);
            out.sort_unstable();
            out.dedup();
            return (out, true);
        }
        let mut out = cur.clusters.clone();
        let widened = !also.is_empty();
        out.append(&mut also);
        out.sort_unstable();
        out.dedup();
        (out, widened)
    }

    /// Every cluster beneath `node`, bounded.
    ///
    /// The bound matters: at a shallow node this can cover a whole message
    /// family, and an unbounded scan would make ingest quadratic on a chatty
    /// console. Past the cap the widening simply does not happen and the line
    /// mints its own template, which is the pre-existing behaviour -- degrading
    /// to "slightly over-split" is acceptable; stalling capture is not.
    fn subtree_clusters(&self, node: &Node) -> Vec<usize> {
        const MAX_SCAN: usize = 512;
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            out.extend_from_slice(&n.clusters);
            if out.len() > MAX_SCAN {
                out.truncate(MAX_SCAN);
                break;
            }
            for c in n.children.values() {
                stack.push(c);
            }
        }
        out
    }

    /// Drain's `fastMatch`: highest similarity wins; ties break toward the more
    /// general template. Below the threshold, nothing matches.
    fn best_match(&self, candidates: &[usize], tokens: &[String], widened: bool) -> Option<usize> {
        // Off the routing path the evidence bar is HIGHER, and this is not
        // optional tuning -- it is what keeps the fix from destroying meaning.
        //
        // The prefix tree is itself evidence: two lines that route to the same
        // leaf already agree on their leading tokens. A widened search throws
        // that away, so at the default similarity (0.40) two UNRELATED lines of
        // the same length matched on ~40% of positions and generalized each
        // other. Measured on a real boot: 3,097 templates collapsed to 241, but
        // 14 of them were pure "<*> <*> <*> <*>" -- rows that say nothing at
        // all, which is worse than the over-splitting they replaced.
        //
        // So a widened match must be nearly identical, and must never be into a
        // template that has already dissolved into wildcards.
        // Similarity ALONE cannot separate "same message, different values"
        // from "two unrelated lines of equal length": the first differs in 3 of
        // 9 positions (0.67) and the second can agree on 40% by accident. The
        // discriminator is how many LITERAL WORDS actually agree -- a real
        // message shares its vocabulary ("Elf", "copied", "from", "to", "size"),
        // whereas coincidental matches share a couple of short tokens.
        const WIDE_SIMILARITY: f64 = 0.6;
        const WIDE_MIN_AGREEING_LITERALS: usize = 3;
        const MAX_WILDCARD_FRACTION: f64 = 0.5;
        let floor = if widened {
            self.cfg.similarity.max(WIDE_SIMILARITY)
        } else {
            self.cfg.similarity
        };
        let mut best: Option<(usize, f64, usize)> = None;
        for &idx in candidates {
            let t = &self.templates[idx];
            if t.tokens.len() != tokens.len() {
                continue;
            }
            if widened && !t.tokens.is_empty() {
                // Never widen INTO a mostly-wildcard template: that is how one
                // vague row swallows every line of its length.
                let wild = t.tokens.iter().filter(|x| *x == WILDCARD).count();
                if wild as f64 / t.tokens.len() as f64 > MAX_WILDCARD_FRACTION {
                    continue;
                }
                // And the two must be the same message, not merely the same
                // shape. The test for that is THE LEADING TOKENS -- the very
                // evidence the prefix tree would have supplied if it had not
                // overflowed.
                //
                // Counting agreeing literal words was the first attempt and it
                // fails on short messages, which is where 810 phantom templates
                // came from on the IQ10. XBL prints ~100 distinct timing labels,
                // each a 3-token line after the structured rewrite
                // (`sbl1_ddr_init B 731939`). Past `max_children` the 3-token
                // root overflows, every one of those lines starts arriving
                // widened, and a bar of "three agreeing literals" is
                // unreachable for a 3-token template: matching
                // `sbl1_ddr_init B <*>` offers two literal positions, and two
                // observations differing in their value agree on two. So the
                // generalized template sat there while every boot minted
                // `label B <new value>` beside it -- and each phantom pushed the
                // tree further past the limit. 2,812 templates became 5,022 in a
                // handful of boots, and every boot reported dozens of "new
                // templates", the exact signal an agent uses to decide what
                // changed.
                //
                // Requiring the routing prefix to agree is both stricter where
                // it matters and satisfiable: it reinstates the tree's own rule
                // (same leading tokens = same branch) instead of guessing at
                // vocabulary. `foo B <*>` and `bar B 12` are then correctly kept
                // apart -- their labels differ -- while `sbl1_ddr_init B <*>`
                // and `sbl1_ddr_init B 999111` meet, which is what the tree
                // would have done unaided.
                // TWO WAYS TO EARN A WIDENED MATCH, because the two failure
                // modes are mirror images and either test alone is wrong:
                //
                //  * shared VOCABULARY (>= 3 agreeing literal words) covers the
                //    case widening was built for -- a value sitting inside the
                //    routing prefix, so the prefixes cannot agree:
                //    "Elf copied from 0x… to 0x… - size N" still shares Elf,
                //    copied, from, to, size.
                //  * an agreeing ROUTING PREFIX covers short messages, where
                //    three literals may not exist at all. It is the tree's own
                //    rule, reinstated by hand for the lines the overflow denied
                //    it to.
                //
                // Requiring both would reject one real case each; requiring
                // neither is the 40%-by-accident merge that produced rows saying
                // nothing at all.
                let agreeing_literals = t
                    .tokens
                    .iter()
                    .zip(tokens.iter())
                    .filter(|(a, b)| a == b && *a != WILDCARD)
                    .count();
                let max_prefix = self.cfg.depth.saturating_sub(2).min(tokens.len());
                let prefix_agrees = max_prefix > 0
                    && t.tokens[..max_prefix]
                        .iter()
                        .zip(tokens[..max_prefix].iter())
                        .all(|(a, b)| a == b || a == WILDCARD);
                if agreeing_literals < WIDE_MIN_AGREEING_LITERALS && !prefix_agrees {
                    continue;
                }
            }
            let (sim, params) = seq_distance(&t.tokens, tokens);
            let better = match best {
                None => true,
                Some((_, bs, bp)) => sim > bs || (sim == bs && params > bp),
            };
            if better {
                best = Some((idx, sim, params));
            }
        }
        best.filter(|&(_, sim, _)| sim >= floor)
            .map(|(idx, _, _)| idx)
    }

    /// Walk (creating if asked) to the leaf node for a token sequence.
    fn leaf_for(&mut self, tokens: &[String], create: bool) -> Option<&mut Node> {
        let n = tokens.len();
        if !create && !self.roots.contains_key(&n) {
            return None;
        }
        let max_children = self.cfg.max_children;
        // depth counts root and leaf; the interior prefix levels are depth - 2.
        let max_prefix = self.cfg.depth.saturating_sub(2);
        let mut cur = self.roots.entry(n).or_default();

        for token in tokens.iter().take(max_prefix.min(n)) {
            if cur.children.contains_key(token) {
                cur = cur.children.get_mut(token).unwrap();
                continue;
            }
            if !create {
                return cur.children.get_mut(WILDCARD);
            }
            // Overflow policy: once a node is full, everything else funnels into
            // the shared `<*>` child. This is Drain's max-children rule and the
            // only place a wildcard enters the *tree* (as opposed to a template).
            let has_wild = cur.children.contains_key(WILDCARD);
            let len = cur.children.len();
            let key = if has_wild {
                if len < max_children {
                    token.clone()
                } else {
                    WILDCARD.to_string()
                }
            } else if len + 1 < max_children {
                token.clone()
            } else {
                WILDCARD.to_string()
            };
            cur = cur.children.entry(key).or_default();
        }
        Some(cur)
    }

    /// How many leading non-wildcard tokens identify a "message family" for the
    /// fragmentation metric and for bounding merge-suggestion comparisons.
    const FAMILY_TOKENS: usize = 3;

    fn family_key(t: &Template) -> String {
        t.skeleton()
            .into_iter()
            .take(Self::FAMILY_TOKENS)
            .collect::<Vec<_>>()
            .join("\u{1}")
    }

    /// Fragmentation health metric (§11): templates per distinct message family.
    ///
    /// 1.0 means every family collapsed to a single template. Climbing well above
    /// that is the no-masking cost of §6 biting — the number minerd exports as a
    /// gauge and `stats` reports, so fragmentation is watched rather than assumed.
    pub fn fragmentation_ratio(&self) -> f64 {
        if self.templates.is_empty() {
            return 1.0;
        }
        let families: std::collections::BTreeSet<String> =
            self.templates.iter().map(Self::family_key).collect();
        self.templates.len() as f64 / families.len().max(1) as f64
    }

    /// Templates that look like the same message split by Drain's token-count and
    /// prefix layers. Pure analysis — calling this never mutates anything, and
    /// nothing is ever merged automatically (§6).
    ///
    /// Comparisons are bounded to within a message family, so this stays cheap on
    /// a store holding tens of thousands of templates.
    pub fn merge_suggestions(&self) -> Vec<MergeSuggestion> {
        let mut families: BTreeMap<String, Vec<&Template>> = BTreeMap::new();
        for t in &self.templates {
            // An all-wildcard template says nothing about identity.
            if t.skeleton().is_empty() {
                continue;
            }
            families.entry(Self::family_key(t)).or_default().push(t);
        }

        let floor = (self.cfg.similarity * 0.6).max(0.1);
        let mut out = Vec::new();
        for group in families.values() {
            for (i, a) in group.iter().enumerate() {
                for b in &group[i + 1..] {
                    let (sa, sb) = (a.skeleton(), b.skeleton());
                    let suggestion = if a.tokens.len() != b.tokens.len() {
                        if sa == sb {
                            Some((MergeReason::SameSkeleton, 1.0))
                        } else {
                            let (short, long) = if sa.len() <= sb.len() {
                                (&sa, &sb)
                            } else {
                                (&sb, &sa)
                            };
                            (!short.is_empty() && long.starts_with(short.as_slice())).then(|| {
                                (
                                    MergeReason::PrefixExtension,
                                    short.len() as f64 / long.len() as f64,
                                )
                            })
                        }
                    } else {
                        // Same length: a near-miss a slightly lower
                        // `mine.similarity` would have clustered.
                        let (sim, _) = seq_distance(&a.tokens, &b.tokens);
                        (sim >= floor && sim < self.cfg.similarity)
                            .then_some((MergeReason::NearThreshold, sim))
                    };

                    if let Some((reason, confidence)) = suggestion {
                        out.push(MergeSuggestion {
                            a: a.id.min(b.id),
                            b: a.id.max(b.id),
                            reason,
                            confidence,
                        });
                    }
                }
            }
        }
        out.sort_by(|x, y| {
            (x.a, x.b)
                .cmp(&(y.a, y.b))
                .then(y.confidence.total_cmp(&x.confidence))
        });
        out.dedup_by_key(|s| (s.a, s.b));
        out
    }
}

/// Drain's similarity: fraction of positions that agree, counting the template's
/// existing wildcards as agreement (`include_params`).
fn seq_distance(template: &[String], tokens: &[String]) -> (f64, usize) {
    debug_assert_eq!(template.len(), tokens.len());
    if template.is_empty() {
        return (1.0, 0);
    }
    let mut sim = 0usize;
    let mut params = 0usize;
    for (a, b) in template.iter().zip(tokens) {
        if a == WILDCARD {
            params += 1;
            continue;
        }
        if a == b {
            sim += 1;
        }
    }
    ((sim + params) as f64 / template.len() as f64, params)
}

/// Mine an entire corpus in one pass. Used by `rebuild_templates` and by the
/// determinism / idempotence property tests.
pub fn mine_all(cfg: DrainConfig, rules: TokenizerRules, lines: &[String]) -> Drain {
    let mut d = Drain::with_rules(cfg, rules);
    for l in lines {
        d.add_line(l);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mine(lines: &[&str]) -> Drain {
        let mut d = Drain::new(DrainConfig::default());
        for l in lines {
            d.add_line(l);
        }
        d
    }

    #[test]
    fn identical_lines_collapse_to_one_template() {
        let d = mine(&["mmc0: new high speed SDHC card at address aaaa"; 5]);
        assert_eq!(d.len(), 1);
        assert_eq!(d.templates()[0].count, 5);
        assert_eq!(d.templates()[0].wildcard_count(), 0);
    }

    #[test]
    fn disagreeing_token_becomes_a_wildcard_not_a_mask() {
        let d = mine(&[
            "smp: Brought up 1 node, 4 CPUs",
            "smp: Brought up 1 node, 8 CPUs",
        ]);
        assert_eq!(d.len(), 1);
        let t = &d.templates()[0];
        assert_eq!(t.text(), "smp: Brought up 1 node, <*> CPUs");
        // The first line alone would have produced no wildcards at all — the
        // wildcard exists because two observations disagreed, not because a
        // regex decided "4" looks variable.
        let one = mine(&["smp: Brought up 1 node, 4 CPUs"]);
        assert_eq!(one.templates()[0].wildcard_count(), 0);
    }

    #[test]
    fn high_cardinality_hex_converges_to_wildcard() {
        let lines: Vec<String> = (0..64)
            .map(|i| format!("Unhandled fault: at 0x{:08x} esr 0x96000010", i * 4096))
            .collect();
        let mut d = Drain::new(DrainConfig::default());
        for l in &lines {
            d.add_line(l);
        }
        assert_eq!(d.len(), 1);
        assert_eq!(
            d.templates()[0].text(),
            "Unhandled fault: at <*> esr 0x96000010"
        );
    }

    #[test]
    fn token_count_variance_fragments_and_is_reported_as_a_merge_suggestion() {
        // Documented cost of no masking (§6): a variable part that changes token
        // *count* lands in a different tree branch entirely.
        let d = mine(&[
            "Freeing unused kernel memory: 2048K",
            "Freeing unused kernel memory: 2048K (aggressive)",
        ]);
        assert_eq!(d.len(), 2, "fragmentation is the documented behaviour");

        let s = d.merge_suggestions();
        assert!(
            s.iter().any(|m| m.reason == MergeReason::PrefixExtension),
            "the fragmentation must at least be surfaced: {s:?}"
        );
        // …and surfacing it must not have changed anything.
        assert_eq!(d.len(), 2);
        assert_eq!(
            d.templates()[0].text(),
            "Freeing unused kernel memory: 2048K"
        );
    }

    #[test]
    fn merge_suggestions_never_mutate_templates() {
        let d = mine(&[
            "alpha bravo charlie 1",
            "alpha bravo charlie 2",
            "alpha bravo charlie 3 delta",
        ]);
        let before: Vec<Template> = d.templates().to_vec();
        let _ = d.merge_suggestions();
        let _ = d.fragmentation_ratio();
        assert_eq!(d.templates(), before.as_slice());
    }

    #[test]
    fn similarity_threshold_boundary_at_0_4() {
        // Five tokens; the two lines agree on exactly two of them = 0.4.
        let at = ["a b x y z", "a b p q r"];
        let mut d = Drain::new(DrainConfig {
            similarity: 0.4,
            ..Default::default()
        });
        for l in at {
            d.add_line(l);
        }
        assert_eq!(d.len(), 1, "0.4 similarity must meet a 0.4 threshold");

        let mut d = Drain::new(DrainConfig {
            similarity: 0.41,
            ..Default::default()
        });
        for l in at {
            d.add_line(l);
        }
        assert_eq!(d.len(), 2, "just above the threshold must not cluster");
    }

    #[test]
    fn max_children_overflow_routes_through_the_wildcard_node() {
        let cfg = DrainConfig {
            max_children: 4,
            ..Default::default()
        };
        let mut d = Drain::new(cfg);
        // Distinct leading tokens far beyond max_children, all same shape.
        for i in 0..50 {
            d.add_line(&format!("tok{i} constant tail here"));
        }
        // Overflowed branches share a leaf, so they cluster together instead of
        // minting 50 templates.
        assert!(
            d.len() < 10,
            "expected wildcard-node convergence, got {}",
            d.len()
        );
        assert!(d.templates().iter().any(|t| t.wildcard_count() > 0));
    }

    #[test]
    fn unicode_tokens_are_ordinary_tokens() {
        let d = mine(&["checking module ✓ thermal", "checking module ✗ thermal"]);
        assert_eq!(d.len(), 1);
        assert_eq!(d.templates()[0].text(), "checking module <*> thermal");
    }

    #[test]
    fn key_value_tokenizer_rule_splits_env_dumps() {
        let rules = TokenizerRules {
            split_key_value: true,
            ..Default::default()
        };
        let lines = [
            "bootargs=console=ttyS0 bootdelay=2 baudrate=115200 ipaddr=10.0.0.5",
            "bootargs=console=ttyS0 bootdelay=0 baudrate=115200 ipaddr=10.0.0.9",
        ];

        // Without the rule the whole `bootdelay=2` assignment is one token, and
        // it sits inside the prefix tree's routing depth — so the two lines take
        // different branches. The widened search does not rescue them: they
        // share only two literal tokens, which is below the vocabulary bar that
        // keeps unrelated lines apart. That is the rule's job, not routing's.
        let plain = mine(&lines);
        assert_eq!(plain.len(), 2);

        // With it, the keys survive as their own tokens and only the values
        // generalize -- which is the whole reason the rule exists.
        let mut d = Drain::with_rules(DrainConfig::default(), rules);
        for l in lines {
            d.add_line(l);
        }
        assert_eq!(d.len(), 1);
        assert_eq!(
            d.templates()[0].text(),
            "bootargs= console=ttyS0 bootdelay= <*> baudrate= 115200 ipaddr= <*>"
        );
    }

    #[test]
    fn key_value_rule_leaves_ordinary_prose_alone() {
        let rules = TokenizerRules {
            split_key_value: true,
            ..Default::default()
        };
        // One `=` is not a KEY_VALUE_RUN.
        assert_eq!(
            rules.tokenize("setting x=1 now"),
            vec!["setting", "x=1", "now"]
        );
    }

    #[test]
    fn blank_lines_are_not_mined() {
        let mut d = Drain::new(DrainConfig::default());
        assert!(d.add_line("").is_none());
        assert!(d.add_line("   \t ").is_none());
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn long_lines_are_mined_on_their_head_and_flagged() {
        let cfg = DrainConfig {
            max_line_tokens: 8,
            ..Default::default()
        };
        let mut d = Drain::new(cfg);
        let long: String = (0..50).map(|i| format!("t{i} ")).collect();
        let m = d.add_line(&long).unwrap();
        assert!(m.created);
        let t = d.template(m.template_id).unwrap();
        assert_eq!(t.tokens.len(), 8);
        assert!(t.head_only);
    }

    #[test]
    fn ids_are_stable_and_assigned_in_first_seen_order() {
        let d = mine(&["first one", "second two", "first one"]);
        assert_eq!(d.templates()[0].id, 1);
        assert_eq!(d.templates()[1].id, 2);
        assert_eq!(d.templates()[0].count, 2);
    }

    #[test]
    fn rehydration_from_templates_reproduces_the_tree() {
        let lines: Vec<String> = (0..40)
            .map(|i| format!("scsi {i}: direct access disk ready"))
            .collect();
        let a = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        let mut b =
            Drain::from_templates(a.config(), a.rules().clone(), a.templates().iter().cloned());
        // Replaying the same input into the rehydrated miner must not create
        // anything new.
        for l in &lines {
            let m = b.add_line(l).unwrap();
            assert!(
                !m.created,
                "rehydrated miner minted a new template for {l:?}"
            );
        }
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn determinism_same_input_same_ids() {
        let lines: Vec<String> = (0..200)
            .map(|i| format!("random-ish line {} of {}", i % 7, i))
            .collect();
        let a = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        let b = mine_all(DrainConfig::default(), TokenizerRules::default(), &lines);
        assert_eq!(a.templates(), b.templates());
    }
}

#[cfg(test)]
mod ansi_tests {
    use super::*;

    /// The exact shape that split one systemd banner into 24 templates: cursor
    /// positioning and colour codes wrapped around one ordinary log line. The
    /// escapes embed column numbers, so every print looked like a new line.
    #[test]
    fn escapes_do_not_split_one_line_into_many_templates() {
        let t = TokenizerRules::default();
        let a = "\x1b[?25l\x1b[2;1H\x1b[0;32m[  OK  ]\x1b[0m Started foo.service";
        let b = "\x1b[?25l\x1b[9;1H\x1b[0;32m[  OK  ]\x1b[0m Started foo.service";
        assert_eq!(
            t.tokenize(a),
            t.tokenize(b),
            "the same line at different cursor positions must tokenize identically"
        );
        assert!(
            !t.tokenize(a).iter().any(|tok| tok.contains('\x1b')),
            "no escape may survive into a template"
        );
    }

    #[test]
    fn the_text_the_escapes_decorate_is_kept() {
        let t = TokenizerRules::default();
        let toks = t.tokenize("\x1b[0;32mStarted\x1b[0m network.target");
        assert_eq!(toks, vec!["Started", "network.target"]);
    }

    /// OSC sequences run to BEL or ST rather than a CSI final byte; getting this
    /// wrong would swallow the rest of the line.
    #[test]
    fn osc_sequences_do_not_eat_the_line() {
        let t = TokenizerRules::default();
        assert_eq!(
            t.tokenize("\x1b]0;title\x07real content"),
            vec!["real", "content"]
        );
        assert_eq!(
            t.tokenize("\x1b]0;title\x1b\\real content"),
            vec!["real", "content"]
        );
    }

    /// A line with no escapes must be untouched -- this runs on every mined
    /// line, so the common case has to stay cheap and lossless.
    #[test]
    fn ordinary_lines_are_unchanged() {
        let t = TokenizerRules::default();
        let plain = "kernel: usb 1-3.1: new high-speed USB device";
        assert_eq!(t.tokenize(plain), t.tokenize(&strip_ansi(plain)));
        assert_eq!(strip_ansi(plain), plain);
    }
}

#[cfg(test)]
mod routing_widening_tests {
    use super::*;

    fn mine_lines(lines: &[String]) -> Drain {
        let mut d = Drain::new(DrainConfig::default());
        for l in lines {
            d.add_line(l);
        }
        d
    }

    /// The measured offenders from a real boot. Each minted a template PER
    /// INSTANCE because the varying value sits in the routing prefix, so the
    /// line took a new branch every time and never met itself.
    #[test]
    fn lines_whose_routing_prefix_holds_a_value_still_converge() {
        let elf: Vec<String> = (0..16)
            .map(|i| {
                format!(
                    "Elf copied from 0x{:08x} to 0x{:08x} - size {}",
                    i * 4096,
                    i * 8192,
                    i * 100
                )
            })
            .collect();
        let d = mine_lines(&elf);
        assert_eq!(d.len(), 1, "16 copies of one message must be one template");

        let ts: Vec<String> = (0..14)
            .map(|i| format!("[ {}.{:03}] read descriptors", i, i * 7))
            .collect();
        assert_eq!(
            mine_lines(&ts).len(),
            1,
            "a timestamp must not fragment a line"
        );
    }

    /// THE PROPERTY THAT MADE ME REVERT PRE-MASKING. A numeric token that never
    /// varies must stay LITERAL: it is often the one value an engineer needs.
    #[test]
    fn a_constant_that_looks_numeric_is_preserved() {
        let faults: Vec<String> = (0..64)
            .map(|i| format!("Unhandled fault: at 0x{:08x} esr 0x96000010", i * 4096))
            .collect();
        let d = mine_lines(&faults);
        assert_eq!(d.len(), 1);
        assert_eq!(
            d.templates()[0].text(),
            "Unhandled fault: at <*> esr 0x96000010",
            "the varying address generalizes; the constant error code must not"
        );
    }

    /// Widening must not merge genuinely different messages. Agreement is still
    /// required -- this only lets a line find the sibling it already matches.
    #[test]
    fn different_messages_still_get_their_own_templates() {
        let d = mine_lines(&[
            "smp: Brought up 1 node, 4 CPUs".to_string(),
            "clk: Disabling unused clocks".to_string(),
            "usb 1-3: new high-speed device".to_string(),
            "Freeing unused kernel memory".to_string(),
        ]);
        assert_eq!(d.len(), 4, "unrelated lines must not collapse into one row");
    }

    /// A wildcard must still be EARNED by disagreement, never assumed. One
    /// observation cannot produce one.
    #[test]
    fn a_single_observation_never_produces_a_wildcard() {
        let d = mine_lines(&["Elf copied from 0xffff8000 to 0xffff9000 - size 4096".to_string()]);
        assert_eq!(d.templates()[0].wildcard_count(), 0);
        assert!(!d.templates()[0].text().contains(WILDCARD));
    }

    /// Convergence must not depend on arrival order: the same set of lines in
    /// any order must yield the same number of templates.
    #[test]
    fn convergence_is_order_independent() {
        let mut lines: Vec<String> = (0..12)
            .map(|i| format!("qhee_hyp_assign_remove_memory: {}/{} -> ret 0", i, i + 1))
            .collect();
        let forward = mine_lines(&lines).len();
        lines.reverse();
        let backward = mine_lines(&lines).len();
        assert_eq!(forward, 1, "one message, one template");
        assert_eq!(
            forward, backward,
            "arrival order must not change the result"
        );
    }

    /// THE GUARD THAT THIS FIX EXISTS UNDER.
    ///
    /// A template of pure "<*> <*> <*>" says NOTHING, and is strictly worse than
    /// the over-splitting it replaced. The first cut of the widened search
    /// produced 14 such rows on a real boot, because unrelated lines of equal
    /// length matched at the default 0.40 similarity and generalized each other
    /// into mush. Widened matches now need real agreeing vocabulary, and may
    /// never merge into an already-vague template.
    #[test]
    fn widening_never_dissolves_a_template_into_pure_wildcards() {
        // Same length, unrelated content -- the exact shape that collapsed.
        let lines: Vec<String> = [
            "alpha bravo charlie delta echo",
            "one two three four five",
            "red green blue cyan magenta",
            "north south east west up",
            "iron copper zinc lead tin",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let d = mine_lines(&lines);
        for t in d.templates() {
            let wild = t.tokens.iter().filter(|x| *x == WILDCARD).count();
            assert!(
                wild < t.tokens.len(),
                "a template dissolved into pure wildcards: {:?}",
                t.text()
            );
        }
        assert_eq!(d.len(), 5, "unrelated lines must not merge at all");
    }

    /// Even under sustained pressure from many same-length lines, no row may
    /// become mostly wildcards -- that is the runaway this bounds.
    #[test]
    fn no_template_becomes_mostly_wildcards_under_pressure() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..200 {
            lines.push(format!("subsys{i} reported state change to ready"));
            lines.push(format!("driver {i} bound to device instance {i}"));
            lines.push(format!("queue depth {i} exceeded soft limit {i}"));
        }
        let d = mine_lines(&lines);
        for t in d.templates() {
            let wild = t.tokens.iter().filter(|x| *x == WILDCARD).count();
            let frac = wild as f64 / t.tokens.len() as f64;
            assert!(
                frac <= 0.75,
                "template is {:.0}% wildcards and says nothing: {:?}",
                frac * 100.0,
                t.text()
            );
        }
    }

    /// Distinct messages that share a routing prefix must stay distinct -- the
    /// widened search must not drag one into the other just because they start
    /// the same way.
    #[test]
    fn a_shared_prefix_does_not_merge_different_tails() {
        let d = mine_lines(&[
            "psci: CPU0 failed to come online".to_string(),
            "psci: CPU1 failed to come online".to_string(),
            "psci: migrate_info_type is not supported".to_string(),
        ]);
        assert_eq!(
            d.len(),
            2,
            "the CPU lines merge; the unrelated psci line does not"
        );
    }

    /// G1, MEASURED ON THE IQ10: once the prefix tree overflows, a SHORT
    /// template can never be matched again, and every occurrence mints a new one.
    ///
    /// XBL prints ~100 distinct timing labels, each a 3-token line after the
    /// structured rewrite (`sbl1_ddr_init B 731939`). Past `max_children` the
    /// 3-token root overflows, so these lines start arriving by the widened
    /// path -- where the guard demanded three agreeing literal words. A template
    /// like `sbl1_ddr_init B <*>` owns only TWO literal positions, so the bar
    /// was arithmetically unreachable: the generalized template sat right there
    /// and every boot minted `sbl1_ddr_init B <new value>` beside it.
    ///
    /// It compounds: each phantom template is another child, pushing the tree
    /// further past the limit. Measured 2,812 -> 5,022 templates in a handful of
    /// boots, 810 of them phantom, and every boot reported dozens of "new
    /// templates" -- the exact signal an agent uses to decide what changed.
    #[test]
    fn a_short_template_is_still_matchable_after_the_tree_overflows() {
        let cfg = DrainConfig::default();
        let mut d = Drain::new(cfg);
        // Overflow the 3-token root the way a boot does: many distinct labels.
        for i in 0..(cfg.max_children + 40) {
            d.add_line(&format!("label{i} B 1000"));
        }
        // One label observed twice with different values: a wildcard forms.
        d.add_line("sbl1_ddr_init B 731939");
        let gen = d.add_line("sbl1_ddr_init B 731940").unwrap();
        assert!(
            gen.generalized || !gen.created,
            "two disagreeing observations must generalize, not split"
        );
        let id = gen.template_id;

        // Now the same message with a third value. It MUST land on that
        // template rather than minting yet another one.
        let again = d.add_line("sbl1_ddr_init B 999111").unwrap();
        assert!(
            !again.created,
            "a third value minted a NEW template ({}) instead of matching the \
             generalized one ({id}); this is the phantom-template loop",
            again.template_id
        );
        assert_eq!(again.template_id, id);
    }

    /// The guard it relaxes must still do its job: a widened match into an
    /// unrelated line of the same length is what produced rows saying nothing at
    /// all ("<*> <*> <*> <*>"), which is worse than the over-splitting it cured.
    #[test]
    fn widening_still_refuses_two_unrelated_lines_of_equal_length() {
        let cfg = DrainConfig::default();
        let mut d = Drain::new(cfg);
        for i in 0..(cfg.max_children + 40) {
            d.add_line(&format!("filler{i} alpha beta gamma"));
        }
        let a = d.add_line("psci: probing for conduit method").unwrap();
        let b = d.add_line("random: crng init done okay").unwrap();
        assert_ne!(
            a.template_id, b.template_id,
            "two unrelated 5-token lines must not merge just because the tree \
             overflowed and they are the same length"
        );
    }
}
