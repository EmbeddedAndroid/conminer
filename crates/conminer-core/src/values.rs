//! Wildcard-slot values: reading the numbers back out of a template.
//!
//! No-masking means a `<*>` slot is not a discarded detail, it is *the*
//! measurement. `Boot took <*> ms`, `MemTotal: <*> kB`, `probe failed with
//! <*>` — the template says what happened, and the slot says how much. Drain
//! computes those positions while clustering and then throws them away, so
//! without this module the one number an agent came for is the one thing the
//! table of contents cannot give it.
//!
//! Extraction is a re-derivation, never a second copy: the record's raw line is
//! passed back through the same profile `mine_key` and tokenizer that produced
//! the template in the first place, and the slot is read positionally. That
//! keeps the store free of denormalised parse results that could disagree with
//! the bytes.
//!
//! **Alignment is checked, not assumed.** A value is reported only when the
//! re-tokenised line has exactly the template's token count. Anything else
//! (a template generalised by the `<*>` overflow node, a profile changed since
//! the record was mined) is counted as `unaligned` and reported as such rather
//! than guessed at, because a silently misaligned number is worse than no
//! number.

use crate::drain::WILDCARD;
use crate::error::{ErrorCode, Result, ToolError};
use crate::framer::ProfileSet;
use crate::store::DeviceStore;
use serde::Serialize;
use serde_json::{json, Value};

/// One occurrence's value for one slot.
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub record_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<i64>,
    pub ts: i64,
    pub value: String,
}

/// A slot across the queried scope.
#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    /// Token index within the template.
    pub slot: usize,
    /// The literal tokens either side, so the agent can tell which `<*>` this is
    /// without counting tokens itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    pub distinct: usize,
    /// Present only when *every* sampled value parsed as a number: a mixed slot
    /// reports no statistics rather than statistics over the subset that
    /// happened to parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numeric: Option<Numeric>,
    /// Most common values, descending. Bounded so a high-cardinality slot (an
    /// address, a timestamp) summarises instead of dumping.
    pub top: Vec<TopValue>,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Numeric {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// First and last in time order: the shape of a drift, without the series.
    pub first: f64,
    pub last: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopValue {
    pub value: String,
    pub count: usize,
}

/// Extract slot values for one template.
///
/// `slot` restricts to a single wildcard index; `None` returns every slot.
#[allow(clippy::too_many_arguments)]
pub fn template_values(
    store: &DeviceStore,
    profiles: &ProfileSet,
    template_id: i64,
    slot: Option<usize>,
    session: Option<i64>,
    boot: Option<i64>,
    max_records: usize,
    max_samples: usize,
) -> Result<Value> {
    let t = store.template(template_id)?;
    let wildcards: Vec<usize> = t
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, tok)| tok.as_str() == WILDCARD)
        .map(|(i, _)| i)
        .collect();

    if let Some(s) = slot {
        if !wildcards.contains(&s) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!("template {template_id} has no wildcard at token {s}"),
            )
            .with_hint(format!(
                "its wildcard slots are {wildcards:?}; call without `slot` to see them all"
            )));
        }
    }
    let wanted: Vec<usize> = match slot {
        Some(s) => vec![s],
        None => wildcards.clone(),
    };

    if wanted.is_empty() {
        return Ok(json!({
            "template": {"id": t.id, "text": t.text},
            "slots": [],
            "why": "this template has no wildcard slots: every occurrence is byte-identical",
            "records_examined": 0,
            "unaligned": 0,
        }));
    }

    let records = store.records_for_template(template_id, session, boot, max_records, 0)?;

    // Per slot, in record order.
    let mut series: Vec<Vec<Sample>> = vec![Vec::new(); wanted.len()];
    let mut unaligned = 0usize;

    for r in &records {
        let Some(head) = store.record_lines(r.id)?.into_iter().next() else {
            continue;
        };
        let text = head.lossy();
        let tokens = match profiles.get(&r.profile) {
            Some(p) => p.tokenizer.tokenize_values(&p.mine_key(&text)),
            // A profile that has since been removed cannot be re-derived; that
            // is an alignment failure, not an excuse to tokenize differently.
            None => {
                unaligned += 1;
                continue;
            }
        };
        if tokens.len() != t.tokens.len() {
            unaligned += 1;
            continue;
        }
        for (out, &idx) in series.iter_mut().zip(&wanted) {
            out.push(Sample {
                record_id: r.id,
                boot_id: r.boot_id,
                ts: head.ts_wall,
                value: tokens[idx].clone(),
            });
        }
    }

    let slots: Vec<Slot> = wanted
        .iter()
        .zip(series)
        .map(|(&idx, samples)| summarise(&t.tokens, idx, samples, max_samples))
        .collect();

    Ok(json!({
        "template": {"id": t.id, "text": t.text, "severity": t.severity},
        "slots": slots,
        "records_examined": records.len(),
        // Surfaced rather than hidden: if this is non-zero the numbers below
        // describe a subset, and the agent needs to know that before trusting a
        // trend drawn from them.
        "unaligned": unaligned,
    }))
}

fn summarise(tokens: &[String], idx: usize, samples: Vec<Sample>, max_samples: usize) -> Slot {
    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for s in &samples {
        *counts.entry(s.value.as_str()).or_default() += 1;
    }
    let mut top: Vec<TopValue> = counts
        .iter()
        .map(|(v, n)| TopValue {
            value: (*v).to_string(),
            count: *n,
        })
        .collect();
    top.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
    let distinct = top.len();
    top.truncate(10);

    let nums: Option<Vec<f64>> = samples.iter().map(|s| parse_number(&s.value)).collect();
    let numeric = nums.filter(|v| !v.is_empty()).map(|v| Numeric {
        min: v.iter().cloned().fold(f64::INFINITY, f64::min),
        max: v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        mean: v.iter().sum::<f64>() / v.len() as f64,
        first: v[0],
        last: v[v.len() - 1],
    });

    // Keep the newest samples: a trend is read from where it ended up.
    let mut samples = samples;
    if samples.len() > max_samples {
        samples.drain(..samples.len() - max_samples);
    }

    Slot {
        slot: idx,
        before: idx
            .checked_sub(1)
            .and_then(|i| tokens.get(i))
            .filter(|t| t.as_str() != WILDCARD)
            .cloned(),
        after: tokens
            .get(idx + 1)
            .filter(|t| t.as_str() != WILDCARD)
            .cloned(),
        distinct,
        numeric,
        top,
        samples,
    }
}

/// Parse a console token as a number, accepting the forms consoles actually
/// print: decimal, hex (`0x…`), and a trailing unit or punctuation
/// (`1234ms`, `47.8GB,`). Returns `None` when the token is not a measurement,
/// which is what keeps a slot of symbol names from reporting a mean.
fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | ')' | ']' | '"' | '\''));
    let neg = t.starts_with('-');
    let t2 = t.strip_prefix(['-', '+']).unwrap_or(t);

    if let Some(hex) = t2.strip_prefix("0x").or_else(|| t2.strip_prefix("0X")) {
        let digits: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        if digits.is_empty() || digits.len() != hex.trim_end_matches(is_unit).len() {
            return None;
        }
        let v = u64::from_str_radix(&digits, 16).ok()? as f64;
        return Some(if neg { -v } else { v });
    }

    let numeric: String = t2
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if numeric.is_empty() {
        return None;
    }
    // Whatever follows must be a unit, not more content: `3.14foo` is a number
    // with a unit, `1.2.3` is a version and must not read as 1.2.
    let rest = &t2[numeric.len()..];
    if !rest.chars().all(is_unit) || numeric.matches('.').count() > 1 {
        return None;
    }
    let v: f64 = numeric.parse().ok()?;
    Some(if neg { -v } else { v })
}

fn is_unit(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '%' || c == '/'
}

/// The numbers in one slot of a template, within one epoch (§F3).
///
/// Shares `template_values`' alignment rule exactly -- tokenize the record's key
/// line with ITS OWN profile, require the token count to match the template, and
/// read the slot -- so a pinned metric can never drift from what the interactive
/// query would have said. A record that does not align is skipped rather than
/// guessed at.
pub fn slot_numbers(
    store: &DeviceStore,
    profiles: &ProfileSet,
    template_id: i64,
    slot: usize,
    boot: i64,
) -> Result<Vec<f64>> {
    let t = store.template(template_id)?;
    let mut out = Vec::new();
    for r in store.records_for_template(template_id, None, Some(boot), 500, 0)? {
        let Some(head) = store.record_lines(r.id)?.into_iter().next() else {
            continue;
        };
        let text = head.lossy();
        let Some(p) = profiles.get(&r.profile) else {
            continue;
        };
        let tokens = p.tokenizer.tokenize_values(&p.mine_key(&text));
        if tokens.len() != t.tokens.len() || slot >= tokens.len() {
            continue;
        }
        // Only actual numbers: a slot that holds a word is not a metric, and
        // silently coercing one would invent a series.
        if let Ok(v) = tokens[slot].trim().parse::<f64>() {
            out.push(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_parse_in_the_forms_consoles_print() {
        assert_eq!(parse_number("1234"), Some(1234.0));
        assert_eq!(parse_number("1.472100"), Some(1.4721));
        assert_eq!(parse_number("0x88e1000"), Some(143_527_936.0));
        assert_eq!(parse_number("115200,"), Some(115200.0));
        assert_eq!(parse_number("47.8GB"), Some(47.8));
        assert_eq!(parse_number("-110"), Some(-110.0));
        assert_eq!(parse_number("8us"), Some(8.0));
    }

    #[test]
    fn things_that_are_not_measurements_do_not_become_numbers() {
        // A version is not a quantity, and averaging it would be nonsense.
        assert_eq!(parse_number("2.11.0"), None);
        assert_eq!(parse_number("v2.11"), None);
        assert_eq!(parse_number("kernel"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("0xZZ"), None);
    }
}
