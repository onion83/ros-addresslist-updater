use ros_addresslist_updater::{
    Network,
    rules::{apply, cancel_conflicts, parse_rule, push_rule},
};

fn net(s: &str) -> Network {
    parse_rule(s).unwrap().network
}

#[test]
fn parse_normalize_deduplicate_and_validate() {
    let mut rules = Vec::new();
    for s in [
        "223.5.5.5/24",
        "223.5.5.0/24",
        "1.1.1.1",
        "1.1.1.1/32",
        "2400:3200::/8",
    ] {
        push_rule(&mut rules, s).unwrap();
    }
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[0].network.text(32), "223.5.5.0/24");
    assert_eq!(rules[2].network.text(128), "2400::/8");
    for s in ["", "1.2.3.4/33", "::/129", "bad", "::/-1"] {
        assert!(parse_rule(s).is_err());
    }
}

#[test]
fn overlaps_cancel_both_sides_without_affecting_original_database() {
    let mut white = vec![
        parse_rule("223.5.5.5").unwrap(),
        parse_rule("1.1.1.1").unwrap(),
        parse_rule("::1").unwrap(),
    ];
    let mut black = vec![
        parse_rule("223.0.0.0/8").unwrap(),
        parse_rule("1.1.1.1/32").unwrap(),
    ];
    let logs = cancel_conflicts(&mut white, &mut black);
    assert_eq!(logs.len(), 2);
    assert_eq!(white.len(), 1);
    assert!(black.is_empty());
    let original = vec![net("223.0.0.0/8")];
    let output = apply(original.clone(), vec![], &white, &black, 32, false);
    assert_eq!(output.networks, original);
    assert_eq!((output.added, output.removed), (0, 0));
}

#[test]
fn white_skip_then_black_removes_whole_entry_and_does_not_restore_white() {
    let output = apply(
        vec![net("223.0.0.0/8")],
        vec![],
        &[parse_rule("223.5.5.5").unwrap()],
        &[parse_rule("223.6.6.6").unwrap()],
        32,
        true,
    );
    assert!(output.networks.is_empty());
    assert_eq!((output.added, output.removed), (0, 1));
    assert_eq!(output.white_logs, ["223.5.5.5 @223.0.0.0/8 SKIP"]);
    assert_eq!(output.black_logs, ["223.6.6.6 @223.0.0.0/8 REMOVE"]);
}

#[test]
fn blacklist_applies_before_merging_and_covers_all_overlapping_rows() {
    let output = apply(
        vec![net("1.0.0.0/25"), net("1.0.0.128/25")],
        vec![],
        &[],
        &[parse_rule("1.0.0.1").unwrap()],
        32,
        true,
    );
    assert_eq!(output.networks, [net("1.0.0.128/25")]);
    let output = apply(
        vec![net("1.0.0.0/25"), net("1.0.0.128/25")],
        vec![net("10.0.0.0/8")],
        &[],
        &[parse_rule("0.0.0.0/0").unwrap()],
        32,
        false,
    );
    assert!(output.networks.is_empty());
    assert_eq!(output.removed, 3);
}

#[test]
fn section_order_and_final_cross_section_optimization() {
    let output = apply(
        vec![net("1.0.0.0/25")],
        vec![net("10.0.0.0/8")],
        &[parse_rule("1.0.0.128/25").unwrap()],
        &[],
        32,
        false,
    );
    assert_eq!(
        output.networks,
        [net("10.0.0.0/8"), net("1.0.0.128/25"), net("1.0.0.0/25")]
    );
    let output = apply(
        vec![net("1.0.0.0/25")],
        vec![net("10.0.0.0/8")],
        &[parse_rule("1.0.0.128/25").unwrap()],
        &[],
        32,
        true,
    );
    assert_eq!(output.networks, [net("10.0.0.0/8"), net("1.0.0.0/24")]);
    assert_eq!(output.database, 0);
    assert_eq!(output.private, 1);
}

#[test]
fn ipv6_normalization_and_whole_entry_removal() {
    let output = apply(
        vec![net("2400::/8"), net("2001:db8::/32")],
        vec![],
        &[parse_rule("::1").unwrap()],
        &[parse_rule("2400:3200::1").unwrap()],
        128,
        true,
    );
    assert_eq!(output.networks, [net("::1"), net("2001:db8::/32")]);
    assert_eq!(output.removed, 1);
}

#[test]
fn comments_survive_normalization_dedup_skip_and_merge() {
    let mut rules = Vec::new();
    for value in [
        "",
        "  # heading",
        "not-an-ip # ignored",
        "1.1.1.1/99 # ignored",
        "223.5.5.5/8   # primary",
        "223.0.0.0/8 # secondary",
        "223.0.0.0/8 # primary",
        "::1 # IPv6",
    ] {
        push_rule(&mut rules, value).unwrap();
    }
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].label, "223.5.5.5/8");
    assert_eq!(rules[0].comment, "primary | secondary");
    let result = apply(vec![net("223.0.0.0/8")], vec![], &rules, &[], 32, true);
    assert_eq!(result.added, 0);
    assert_eq!(result.comments, ["primary | secondary"]);
    let white = [
        parse_rule("1.0.0.0/25 # first").unwrap(),
        parse_rule("1.0.0.128/25 # second").unwrap(),
    ];
    let result = apply(vec![], vec![], &white, &[], 32, true);
    assert_eq!(result.networks, [net("1.0.0.0/24")]);
    assert_eq!(result.comments, ["first | second"]);
}
