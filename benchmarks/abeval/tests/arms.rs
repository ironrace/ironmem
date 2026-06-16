use abeval::arms::{assign_arms, parse_arms_selector, Arm};

#[test]
fn labels_are_exact() {
    assert_eq!(Arm::Ironmem.label(), "ironmem");
    assert_eq!(Arm::Superpowers.label(), "superpowers");
}

#[test]
fn both_selector_yields_both_arms() {
    assert_eq!(
        parse_arms_selector("both").unwrap(),
        vec![Arm::Ironmem, Arm::Superpowers]
    );
    assert_eq!(parse_arms_selector("ironmem").unwrap(), vec![Arm::Ironmem]);
    assert_eq!(
        parse_arms_selector("superpowers").unwrap(),
        vec![Arm::Superpowers]
    );
    assert!(parse_arms_selector("bogus").is_err());
}

#[test]
fn serde_form_matches_label_for_every_variant() {
    // `Arm` has two sources of truth for its strings: the serde rename and
    // `label()`. Pin that they agree so neither can drift silently.
    for arm in [Arm::Ironmem, Arm::Superpowers] {
        let json = serde_json::to_string(&arm).unwrap();
        assert_eq!(json, format!("\"{}\"", arm.label()), "serde form must equal label()");
        let back: Arm = serde_json::from_str(&json).unwrap();
        assert_eq!(back, arm, "round-trip must preserve the arm");
    }
}

#[test]
fn assignment_is_deterministic() {
    let a = assign_arms("abeval-01-x", "both").unwrap();
    let b = assign_arms("abeval-01-x", "both").unwrap();
    assert_eq!(a, b);
    assert_eq!(a, vec![Arm::Ironmem, Arm::Superpowers]);
}
