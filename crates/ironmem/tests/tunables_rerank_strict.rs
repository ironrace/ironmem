use ironmem::search::tunables;

#[test]
fn rerank_strict_string_enum_rejects_one() {
    std::env::set_var("IRONMEM_RERANK", "1");
    assert!(
        !tunables::rerank_enabled(),
        "IRONMEM_RERANK=1 must NOT enable"
    );
}
