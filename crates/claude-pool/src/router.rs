//! Account selection.
//!
//! * **Affinity** (prompt-cache aware): requests that share a prompt prefix are
//!   sent to the same account with weighted rendezvous hashing, so the
//!   provider-side prompt cache keeps hitting. Adding/removing an account only
//!   remaps that account's share of conversations.
//! * **Least-loaded**: otherwise pick the account with the lowest
//!   `(inflight + 1) / (weight * headroom)`, which spreads load and steers away
//!   from accounts close to their rate limits before they start returning 429s.
//!
//! The scan is O(accounts) over atomics with no allocation or locking.

use crate::account::Account;
use xxhash_rust::xxh3::xxh3_64_with_seed;

pub struct Router<'a> {
    pub accounts: &'a [Account],
    pub min_affinity_headroom: f64,
}

impl Router<'_> {
    /// `excluded` is a bitmask of account indexes already tried for this request.
    pub fn pick(&self, affinity: Option<u64>, excluded: u64, now: u64) -> Option<usize> {
        if let Some(key) = affinity
            && let Some(i) = self.pick_affinity(key, excluded, now)
        {
            return Some(i);
        }
        self.pick_least_loaded(excluded, now)
    }

    fn pick_affinity(&self, key: u64, excluded: u64, now: u64) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (i, a) in self.accounts.iter().enumerate() {
            if excluded & (1 << i) != 0
                || !a.available(now)
                || a.headroom(now) < self.min_affinity_headroom
            {
                continue;
            }
            // Weighted rendezvous: score = -weight / ln(u), u uniform in (0,1).
            let h = xxh3_64_with_seed(&key.to_le_bytes(), i as u64);
            let u = ((h >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
            let score = -(a.weight as f64) / u.ln();
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((i, score));
            }
        }
        best.map(|(i, _)| i)
    }

    fn pick_least_loaded(&self, excluded: u64, now: u64) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        // Random start breaks ties fairly without a second pass.
        let n = self.accounts.len();
        let start = if n > 1 {
            rand::random::<usize>() % n
        } else {
            0
        };
        for k in 0..n {
            let i = (start + k) % n;
            let a = &self.accounts[i];
            if excluded & (1 << i) != 0 || !a.available(now) {
                continue;
            }
            let inflight = a.inflight.load(std::sync::atomic::Ordering::Relaxed) as f64;
            let load = (inflight + 1.0) / (a.weight as f64 * a.headroom(now).max(0.01));
            if best.is_none_or(|(_, l)| load < l) {
                best = Some((i, load));
            }
        }
        best.map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::now_ms;
    use crate::config::AccountConfig;
    use std::sync::atomic::Ordering::Relaxed;

    fn accounts(n: usize) -> Vec<Account> {
        (0..n)
            .map(|i| {
                Account::new(&AccountConfig {
                    name: format!("a{i}"),
                    api_key: "k".into(),
                    admin_key: None,
                    weight: 1,
                    max_concurrency: 0,
                    monthly_budget_usd: None,
                })
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn affinity_is_sticky_and_spread() {
        let accts = accounts(4);
        let r = Router {
            accounts: &accts,
            min_affinity_headroom: 0.05,
        };
        let now = now_ms();
        let mut counts = [0usize; 4];
        for key in 0..4000u64 {
            let a = r.pick(Some(key), 0, now).unwrap();
            assert_eq!(
                a,
                r.pick(Some(key), 0, now).unwrap(),
                "same key, same account"
            );
            counts[a] += 1;
        }
        for c in counts {
            assert!((700..1300).contains(&c), "uneven spread: {counts:?}");
        }
    }

    #[test]
    fn affinity_only_remaps_lost_account() {
        let accts = accounts(4);
        let r = Router {
            accounts: &accts,
            min_affinity_headroom: 0.05,
        };
        let now = now_ms();
        let before: Vec<_> = (0..1000u64)
            .map(|k| r.pick(Some(k), 0, now).unwrap())
            .collect();
        accts[2].disable_for(60);
        for (k, b) in before.iter().enumerate() {
            let after = r.pick(Some(k as u64), 0, now_ms()).unwrap();
            if *b != 2 {
                assert_eq!(after, *b, "keys on healthy accounts must not move");
            } else {
                assert_ne!(after, 2);
            }
        }
    }

    #[test]
    fn least_loaded_prefers_idle_and_respects_exclusion() {
        let accts = accounts(3);
        accts[0].inflight.store(10, Relaxed);
        accts[1].inflight.store(1, Relaxed);
        let r = Router {
            accounts: &accts,
            min_affinity_headroom: 0.05,
        };
        assert_eq!(r.pick(None, 0, now_ms()), Some(2));
        assert_eq!(r.pick(None, 0b100, now_ms()), Some(1));
        assert_eq!(r.pick(None, 0b111, now_ms()), None);
    }
}
