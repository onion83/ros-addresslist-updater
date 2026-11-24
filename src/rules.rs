use crate::{Network, mask, prepare};
use anyhow::{Context, Result, ensure};
use std::net::IpAddr;

#[derive(Clone, Debug)]
pub struct Rule {
    pub network: Network,
    pub bits: u8,
    pub label: String,
    pub comment: String,
}

impl Network {
    pub fn contains(self, other: Self, bits: u8) -> bool {
        self.prefix <= other.prefix && other.address & mask(bits, self.prefix) == self.address
    }
    pub fn overlaps(self, other: Self, bits: u8) -> bool {
        self.contains(other, bits) || other.contains(self, bits)
    }
}

pub fn parse_rule(value: &str) -> Result<Rule> {
    let (label, comment) = value.split_once('#').unwrap_or((value, ""));
    let label = label.trim();
    let comment = comment.trim();
    ensure!(!label.is_empty(), "empty IP/CIDR");
    let (ip, prefix) = match label.split_once('/') {
        Some((ip, prefix)) => (ip, Some(prefix.parse::<u8>().context("invalid prefix")?)),
        None => (label, None),
    };
    let ip: IpAddr = ip.parse().with_context(|| format!("invalid IP: {ip}"))?;
    let (bits, address) = match ip {
        IpAddr::V4(ip) => (32, u32::from(ip) as u128),
        IpAddr::V6(ip) => (128, u128::from(ip)),
    };
    let prefix = prefix.unwrap_or(bits);
    ensure!(prefix <= bits, "invalid prefix for {label}");
    Ok(Rule {
        network: Network {
            address: address & mask(bits, prefix),
            prefix,
        },
        bits,
        label: label.to_string(),
        comment: comment.to_string(),
    })
}

fn merge_comment(target: &mut String, comment: &str) {
    if !comment.is_empty() && !target.split(" | ").any(|s| s == comment) {
        if !target.is_empty() {
            target.push_str(" | ");
        }
        target.push_str(comment);
    }
}

pub fn push_rule(rules: &mut Vec<Rule>, value: &str) -> Result<()> {
    // Lists are user-maintained text: skip blank/comment/invalid lines.
    let Ok(rule) = parse_rule(value) else {
        return Ok(());
    };
    insert_rule(rules, rule);
    Ok(())
}

pub fn insert_rule(rules: &mut Vec<Rule>, rule: Rule) {
    if let Some(existing) = rules
        .iter_mut()
        .find(|r| r.bits == rule.bits && r.network == rule.network)
    {
        merge_comment(&mut existing.comment, &rule.comment);
    } else {
        rules.push(rule);
    }
}

/// Cancel every rule participating in a cross-list overlap, before applying either list.
pub fn cancel_conflicts(white: &mut Vec<Rule>, black: &mut Vec<Rule>) -> Vec<String> {
    let mut cancelled_white = vec![false; white.len()];
    let mut cancelled_black = vec![false; black.len()];
    let mut logs = Vec::new();
    for (i, w) in white.iter().enumerate() {
        for (j, b) in black.iter().enumerate() {
            if w.bits == b.bits && w.network.overlaps(b.network, w.bits) {
                cancelled_white[i] = true;
                cancelled_black[j] = true;
                logs.push(format!("{} (white) @{} (black) SKIP", w.label, b.label));
            }
        }
    }
    let mut i = 0;
    white.retain(|_| {
        let keep = !cancelled_white[i];
        i += 1;
        keep
    });
    let mut i = 0;
    black.retain(|_| {
        let keep = !cancelled_black[i];
        i += 1;
        keep
    });
    logs
}

pub struct Applied {
    pub networks: Vec<Network>,
    pub comments: Vec<String>,
    pub custom: Vec<bool>,
    pub database: usize,
    pub private: usize,
    pub added: usize,
    pub removed: usize,
    pub optimized: usize,
    pub duplicates: usize,
    pub white_logs: Vec<String>,
    pub black_logs: Vec<String>,
}

pub fn apply(
    database: Vec<Network>,
    private: Vec<Network>,
    white: &[Rule],
    black: &[Rule],
    bits: u8,
    optimize: bool,
) -> Applied {
    // Origin priority keeps private first, then whitelist, then database.
    let mut rows: Vec<(Network, u8)> = private.into_iter().map(|n| (n, 0)).collect();
    rows.extend(prepare(database, bits, false).into_iter().map(|n| (n, 2)));
    let mut result = Applied {
        networks: Vec::new(),
        comments: Vec::new(),
        custom: Vec::new(),
        database: 0,
        private: 0,
        added: 0,
        removed: 0,
        optimized: 0,
        duplicates: 0,
        white_logs: Vec::new(),
        black_logs: Vec::new(),
    };
    for rule in white.iter().filter(|r| r.bits == bits) {
        if let Some((cover, _)) = rows.iter().find(|(n, _)| n.contains(rule.network, bits)) {
            if *cover == rule.network || rows.iter().any(|(n, _)| *n == rule.network) {
                result.duplicates += 1;
            } else {
                result.optimized += 1;
            }
            result
                .white_logs
                .push(format!("{} @{} SKIP", rule.label, cover.text(bits)));
        } else {
            rows.push((rule.network, 1));
            result.added += 1;
        }
    }
    for rule in black.iter().filter(|r| r.bits == bits) {
        rows.retain(|(n, _)| {
            if n.overlaps(rule.network, bits) {
                result
                    .black_logs
                    .push(format!("{} @{} REMOVE", rule.label, n.text(bits)));
                result.removed += 1;
                false
            } else {
                true
            }
        });
    }
    let unique = rows
        .iter()
        .map(|(network, _)| *network)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    result.duplicates += rows.len() - unique;
    if optimize {
        let merged = prepare(rows.iter().map(|(n, _)| *n).collect(), bits, true);
        result.optimized += unique - merged.len();
        // Only non-database origins need checking; database can be very large.
        let special: Vec<_> = rows
            .iter()
            .filter(|(_, origin)| *origin < 2)
            .copied()
            .collect();
        rows = merged
            .into_iter()
            .map(|n| {
                let origin = special
                    .iter()
                    .filter(|(s, _)| n.overlaps(*s, bits))
                    .map(|(_, o)| *o)
                    .min()
                    .unwrap_or(2);
                (n, origin)
            })
            .collect();
    }
    // Stable grouping preserves the supplied order within private/white sections.
    rows.sort_by_key(|(_, origin)| *origin);
    result.private = rows.iter().filter(|(_, o)| *o == 0).count();
    result.database = rows.iter().filter(|(_, o)| *o == 2).count();
    result.custom = rows.iter().map(|(_, origin)| *origin == 1).collect();
    result.networks = rows.into_iter().map(|(n, _)| n).collect();
    result.comments = result
        .networks
        .iter()
        .map(|network| {
            let mut comment = String::new();
            for rule in white
                .iter()
                .filter(|r| r.bits == bits && !r.comment.is_empty())
            {
                if network.overlaps(rule.network, bits) {
                    merge_comment(&mut comment, &rule.comment);
                }
            }
            comment
        })
        .collect();
    result
}
