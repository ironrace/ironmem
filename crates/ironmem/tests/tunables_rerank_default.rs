use ironmem::search::tunables;

#[test]
fn rerank_disabled_by_default() {
    std::env::remove_var("IRONMEM_RERANK");
    assert!(!tunables::rerank_enabled());
}
