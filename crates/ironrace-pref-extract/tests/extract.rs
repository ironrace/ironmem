use ironrace_pref_extract::{
    looks_conversational, synthesize_doc, PreferenceExtractor, RegexPreferenceExtractor,
};

fn extract(text: &str) -> Vec<String> {
    RegexPreferenceExtractor.extract(text)
}

#[test]
fn extracts_struggling_with_pattern() {
    let text = "I've been having trouble with the battery life on my phone lately.";
    let out = extract(text);
    assert_eq!(out.len(), 1, "got {:?}", out);
    assert!(out[0].contains("battery life"), "got {:?}", out);
}

#[test]
fn extracts_i_prefer_pattern() {
    let out = extract("Honestly, I prefer black coffee in the morning.");
    assert!(
        out.iter().any(|p| p.contains("black coffee")),
        "got {:?}",
        out,
    );
}

#[test]
fn extracts_lately_pattern() {
    let out = extract("Lately, I've been thinking about switching to a standing desk.");
    assert!(
        out.iter().any(|p| p.contains("standing desk")),
        "got {:?}",
        out,
    );
}

#[test]
fn extracts_used_to_pattern() {
    let out = extract("I used to play guitar in a college band.");
    assert!(
        out.iter().any(|p| p.contains("play guitar")),
        "got {:?}",
        out,
    );
}

#[test]
fn extracts_high_school_memory_pattern() {
    let out = extract(
        "When I was in high school, I really enjoyed running cross country and \
         did pretty well at state.",
    );
    assert!(
        !out.is_empty(),
        "expected memory pattern to fire on high-school clause"
    );
}

#[test]
fn dedupes_and_caps_at_twelve() {
    let mut text = String::new();
    for i in 0..20 {
        text.push_str(&format!("I prefer flavor number {i} above all others. "));
    }
    let out = extract(&text);
    assert!(out.len() <= 12, "expected ≤12 phrases, got {}", out.len());
}

#[test]
fn returns_empty_on_non_conversational_input() {
    let rust = r#"
        fn main() {
            let x = 42;
            println!("{}", x);
        }
    "#;
    assert!(extract(rust).is_empty());
}

#[test]
fn looks_conversational_true_for_first_person_lead() {
    assert!(looks_conversational(
        "I've been thinking about switching jobs."
    ));
    assert!(looks_conversational(
        "So my plan is to take a sabbatical next quarter."
    ));
}

#[test]
fn looks_conversational_false_for_code_or_third_person() {
    assert!(!looks_conversational(
        "fn main() { let x = 42; println!(\"{}\", x); }"
    ));
    assert!(!looks_conversational(
        "The user opened the file and wrote a function."
    ));
}

#[test]
fn synthesize_doc_returns_none_for_empty() {
    assert_eq!(synthesize_doc(&[]), None);
}

#[test]
fn synthesize_doc_joins_phrases() {
    let phrases = vec!["black coffee".to_string(), "standing desks".to_string()];
    assert_eq!(
        synthesize_doc(&phrases),
        Some("black coffee. standing desks".to_string()),
    );
}
