//! Symbolization (§15.5) — best-effort, and never mutating raw.
//!
//! Crash records carry raw addresses; agents want function names. Resolution is
//! deliberately **best-effort and explicit about it**: a wrong symbol is worse
//! than none, so a lookup that cannot be justified returns nothing rather than
//! the nearest guess beyond a sane bound.
//!
//! Results are stored as derived annotations attached to the record. The record's
//! raw bytes are never rewritten (§6), so a wrong symbol table can be corrected
//! by re-running with a right one.

use crate::error::{ErrorCode, Result, ToolError};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// How far past a symbol's start an address may sit and still be attributed to
/// it. Beyond this the gap is more likely a missing symbol than a huge function.
const MAX_OFFSET: u64 = 1 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub address: String,
    pub symbol: Option<String>,
    pub offset: Option<u64>,
    /// Why nothing was resolved, when nothing was.
    pub note: Option<String>,
}

/// A sorted address→name table, as `System.map` or `nm -n` produces.
#[derive(Debug, Default)]
pub struct SymbolTable {
    entries: Vec<(u64, String)>,
}

static ADDR: Lazy<Regex> = Lazy::new(|| Regex::new(r"0x[0-9a-fA-F]{6,16}").expect("addr regex"));
static BRACKETED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[<([0-9a-fA-F]{6,16})>\]").expect("bracketed addr regex"));

impl SymbolTable {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ToolError::new(
                ErrorCode::NoSuchPath,
                format!("cannot read symbols from {}: {e}", path.display()),
            )
        })?;
        Self::parse(&text)
    }

    /// Parse `System.map` / `nm -n` lines: `<hex addr> <type> <name>`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut entries = Vec::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(addr), Some(_kind), Some(name)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if let Ok(a) = u64::from_str_radix(addr.trim_start_matches("0x"), 16) {
                entries.push((a, name.to_string()));
            }
        }
        if entries.is_empty() {
            return Err(
                ToolError::new(ErrorCode::InvalidArgument, "no symbols could be parsed")
                    .with_hint("expected System.map or `nm -n` format: <hex addr> <type> <name>"),
            );
        }
        entries.sort_by_key(|(a, _)| *a);
        entries.dedup_by_key(|(a, _)| *a);
        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve one address to the symbol containing it.
    pub fn resolve(&self, addr: u64) -> Option<(&str, u64)> {
        let idx = match self.entries.binary_search_by_key(&addr, |(a, _)| *a) {
            Ok(i) => i,
            Err(0) => return None, // below the first symbol
            Err(i) => i - 1,
        };
        let (start, name) = &self.entries[idx];
        let offset = addr - start;
        // Beyond a sane function size, silence beats a confident wrong answer.
        (offset <= MAX_OFFSET).then_some((name.as_str(), offset))
    }

    /// Find every address in a crash record and resolve what it can.
    pub fn annotate(&self, text: &str, relocation_base: u64) -> Vec<Frame> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        let mut consider = |raw: &str, value: u64| {
            if !seen.insert(value) {
                return;
            }
            let adjusted = value.saturating_sub(relocation_base);
            match self.resolve(adjusted) {
                Some((sym, off)) => out.push(Frame {
                    address: raw.to_string(),
                    symbol: Some(sym.to_string()),
                    offset: Some(off),
                    note: None,
                }),
                None => out.push(Frame {
                    address: raw.to_string(),
                    symbol: None,
                    offset: None,
                    note: Some(
                        "no symbol covers this address; wrong image, missing module, or a \
                         relocation base that has not been supplied"
                            .into(),
                    ),
                }),
            }
        };

        for c in BRACKETED.captures_iter(text) {
            let raw = c.get(0).expect("whole match").as_str();
            if let Ok(v) = u64::from_str_radix(&c[1], 16) {
                consider(raw, v);
            }
        }
        for m in ADDR.find_iter(text) {
            if let Ok(v) = u64::from_str_radix(m.as_str().trim_start_matches("0x"), 16) {
                consider(m.as_str(), v);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = "\
ffffffff81000000 T _stext
ffffffff81001000 T do_page_fault
ffffffff81002000 T schedule
ffffffff81003000 T really_probe
";

    #[test]
    fn a_system_map_parses_and_sorts() {
        let t = SymbolTable::parse(MAP).unwrap();
        assert_eq!(t.len(), 4);
        assert_eq!(
            t.resolve(0xffffffff81001010).unwrap(),
            ("do_page_fault", 0x10)
        );
        assert_eq!(t.resolve(0xffffffff81002000).unwrap(), ("schedule", 0));
    }

    #[test]
    fn an_address_below_the_first_symbol_resolves_to_nothing() {
        let t = SymbolTable::parse(MAP).unwrap();
        assert!(t.resolve(0x1000).is_none());
    }

    #[test]
    fn an_address_far_past_the_last_symbol_is_not_guessed_at() {
        let t = SymbolTable::parse(MAP).unwrap();
        assert!(
            t.resolve(0xffffffff9fffffff).is_none(),
            "a wrong symbol is worse than none"
        );
    }

    #[test]
    fn a_crash_record_is_annotated_without_being_modified() {
        let t = SymbolTable::parse(MAP).unwrap();
        let record = "Internal error: Oops\n\
                      Call trace:\n\
                      [<ffffffff81003040>] really_probe+0x40/0x3a0\n\
                      pc : 0xffffffff81001010\n";
        let frames = t.annotate(record, 0);
        let named: Vec<&str> = frames.iter().filter_map(|f| f.symbol.as_deref()).collect();
        assert!(named.contains(&"really_probe"), "{frames:?}");
        assert!(named.contains(&"do_page_fault"), "{frames:?}");
        // The record text is untouched: annotation is a *derived* view.
        assert!(record.contains("[<ffffffff81003040>]"));
    }

    #[test]
    fn kaslr_is_handled_by_subtracting_the_supplied_relocation_base() {
        let t = SymbolTable::parse(MAP).unwrap();
        let base = 0x1000_0000u64;
        let shifted = format!("pc : 0x{:x}", 0xffffffff81001010u64.wrapping_add(base));
        // Without the base, nothing sensible resolves…
        let blind = t.annotate(&shifted, 0);
        assert!(blind.iter().all(|f| f.symbol.is_none()) || blind.is_empty());
        // …with it, the frame lands.
        let aware = t.annotate(&shifted, base);
        assert_eq!(aware[0].symbol.as_deref(), Some("do_page_fault"));
    }

    #[test]
    fn an_unresolvable_address_says_why_rather_than_going_silent() {
        let t = SymbolTable::parse(MAP).unwrap();
        let frames = t.annotate("pc : 0x0000000000001234", 0);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].symbol.is_none());
        assert!(frames[0].note.as_deref().unwrap().contains("no symbol"));
    }

    #[test]
    fn an_empty_or_bogus_symbol_file_is_a_structured_error() {
        assert_eq!(
            SymbolTable::parse("").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let err = SymbolTable::parse("this is not a symbol table\n").unwrap_err();
        assert!(err.hint.contains("System.map"));
    }

    #[test]
    fn each_address_is_reported_once_even_when_it_repeats() {
        let t = SymbolTable::parse(MAP).unwrap();
        let text = "0xffffffff81002000 0xffffffff81002000 0xffffffff81002000";
        assert_eq!(t.annotate(text, 0).len(), 1);
    }
}
