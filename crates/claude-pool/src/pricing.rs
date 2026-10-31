//! Cost estimation. Prices are USD per million tokens and are *estimates*:
//! override them in the `[pricing]` config table to match your contract.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    /// Defaults to 10% of `input` when omitted.
    pub cache_read: Option<f64>,
    /// 5-minute cache write; defaults to 1.25x `input`.
    pub cache_write_5m: Option<f64>,
    /// 1-hour cache write; defaults to 2x `input`.
    pub cache_write_1h: Option<f64>,
}

impl ModelPrice {
    const fn new(input: f64, output: f64, cache_read: Option<f64>) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write_5m: None,
            cache_write_1h: None,
        }
    }
}

/// Token counts for one request, as reported by the API's `usage` object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

/// Built-in defaults (first-party API list prices at the time of writing).
const DEFAULTS: &[(&str, ModelPrice)] = &[
    ("claude-fable-5-1", ModelPrice::new(10.0, 50.0, Some(0.25))),
    ("claude-mythos-5-1", ModelPrice::new(10.0, 50.0, Some(0.25))),
    ("claude-fable-5", ModelPrice::new(10.0, 50.0, None)),
    ("claude-opus-5-5", ModelPrice::new(4.0, 20.0, Some(0.20))),
    ("claude-opus-5", ModelPrice::new(5.0, 25.0, None)),
    ("claude-opus-4", ModelPrice::new(5.0, 25.0, None)),
    ("claude-sonnet-5-5", ModelPrice::new(2.0, 10.0, Some(0.20))),
    ("claude-sonnet-5", ModelPrice::new(2.0, 10.0, None)),
    ("claude-sonnet-4", ModelPrice::new(3.0, 15.0, None)),
    ("claude-haiku-5-5", ModelPrice::new(0.10, 0.50, None)),
    ("claude-haiku-4", ModelPrice::new(1.0, 5.0, None)),
];

/// Fallback when the model is unknown: priced like the default Opus tier.
const UNKNOWN: ModelPrice = ModelPrice::new(5.0, 25.0, None);

#[derive(Debug, Clone)]
pub struct PriceTable {
    /// Sorted longest-prefix-first so the most specific entry wins.
    entries: Vec<(String, ModelPrice)>,
}

impl PriceTable {
    pub fn new(overrides: &HashMap<String, ModelPrice>) -> Self {
        let mut map: HashMap<String, ModelPrice> =
            DEFAULTS.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        for (k, v) in overrides {
            map.insert(k.clone(), *v);
        }
        let mut entries: Vec<_> = map.into_iter().collect();
        entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
        Self { entries }
    }

    pub fn price_for(&self, model: &str) -> ModelPrice {
        // Bedrock-style ids ("anthropic.claude-...") still match.
        let model = model.strip_prefix("anthropic.").unwrap_or(model);
        self.entries
            .iter()
            .find(|(prefix, _)| model.starts_with(prefix.as_str()))
            .map(|(_, p)| *p)
            .unwrap_or(UNKNOWN)
    }

    /// Estimated cost in micro-USD (integer so it can live in an atomic).
    pub fn cost_micros(&self, model: &str, u: &TokenUsage) -> u64 {
        let p = self.price_for(model);
        let usd = (u.input as f64 * p.input
            + u.output as f64 * p.output
            + u.cache_read as f64 * p.cache_read.unwrap_or(p.input * 0.1)
            + u.cache_write_5m as f64 * p.cache_write_5m.unwrap_or(p.input * 1.25)
            + u.cache_write_1h as f64 * p.cache_write_1h.unwrap_or(p.input * 2.0))
            / 1_000_000.0;
        (usd * 1_000_000.0).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_specific_prefix_wins() {
        let t = PriceTable::new(&HashMap::new());
        assert_eq!(t.price_for("claude-opus-5-5").input, 4.0);
        assert_eq!(t.price_for("claude-opus-5").input, 5.0);
        assert_eq!(t.price_for("claude-sonnet-4-6").input, 3.0);
        assert_eq!(t.price_for("anthropic.claude-haiku-5-5").input, 0.10);
    }

    #[test]
    fn cost_math() {
        let t = PriceTable::new(&HashMap::new());
        let u = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        };
        // 4 + 20 + 0.20 USD
        assert_eq!(t.cost_micros("claude-opus-5-5", &u), 24_200_000);
    }

    #[test]
    fn overrides_apply() {
        let mut o = HashMap::new();
        o.insert("claude-opus-5-5".into(), ModelPrice::new(1.0, 2.0, None));
        let t = PriceTable::new(&o);
        assert_eq!(t.price_for("claude-opus-5-5").output, 2.0);
    }
}
