use abeval::arms::{assign_arms, parse_arms_selector, Arm};
use abeval::client::arm_command;
use abeval::corpus::Task;

fn task(prompt: &str) -> Task {
    Task {
        id: "t1".to_string(),
        title: "T".to_string(),
        source: "issue:#1".to_string(),
        repo_scope: vec!["crates/ironmem/src/lib.rs".to_string()],
        prompt: prompt.to_string(),
        acceptance: vec!["a".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: "abcdef1234567890abcdef1234567890abcdef12".to_string(),
    }
}

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
    for arm in [Arm::Ironmem, Arm::Superpowers] {
        let json = serde_json::to_string(&arm).unwrap();
        assert_eq!(
            json,
            format!("\"{}\"", arm.label()),
            "serde form must equal label()"
        );
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

#[test]
fn ironmem_arm_exact_argv() {
    let (program, argv) = arm_command(&task("solve X"), Arm::Ironmem);
    assert_eq!(program, "claude");
    assert_eq!(
        argv,
        vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
            "-p".to_string(),
            "/collab start solve X".to_string(),
        ]
    );
}

#[test]
fn superpowers_arm_exact_argv() {
    let (program, argv) = arm_command(&task("solve X"), Arm::Superpowers);
    assert_eq!(program, "claude");
    assert_eq!(argv[0], "--output-format");
    assert_eq!(argv[1], "json");
    assert_eq!(argv[2], "--permission-mode");
    assert_eq!(argv[3], "bypassPermissions");
    assert_eq!(argv[4], "-p");
    assert!(argv[5].ends_with("solve X"), "got: {}", argv[5]);
    assert!(argv[5].starts_with("Run this task with superpowers skills only."));
    // The prefix mentions "/collab" in a prohibition ("Do not use /collab"),
    // so a substring check would be wrong. Assert on the command token instead:
    // no argv element should BE the "/collab" slash-command token itself.
    assert!(!argv.iter().any(|a| a == "/collab"), "superpowers must never invoke /collab as a command token");
}

#[test]
fn both_arms_carry_print_and_permission_tokens() {
    for arm in [Arm::Ironmem, Arm::Superpowers] {
        let (_p, argv) = arm_command(&task("p"), arm);
        assert!(argv.contains(&"-p".to_string()), "arm {arm:?} missing -p");
        assert!(argv.contains(&"--permission-mode".to_string()));
        assert!(argv.contains(&"bypassPermissions".to_string()));
    }
}

#[test]
fn ironmem_prompt_contains_collab_start_and_superpowers_does_not() {
    let (_p, ir) = arm_command(&task("p"), Arm::Ironmem);
    assert!(ir.iter().any(|a| a.contains("/collab start")));
    let (_p, sp) = arm_command(&task("p"), Arm::Superpowers);
    // The skills-only prefix legitimately mentions "/collab" in a prohibition
    // ("Do not use /collab"), so a substring check would give a false positive.
    // Assert on the command token: no element should BE "/collab" itself.
    assert!(!sp.iter().any(|a| a == "/collab"), "superpowers must never invoke /collab as a command token");
}
