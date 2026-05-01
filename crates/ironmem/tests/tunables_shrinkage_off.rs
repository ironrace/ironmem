use ironmem::search::tunables;

#[test]
fn shrinkage_off_via_env() {
    std::env::set_var("IRONMEM_SHRINKAGE_RERANK", "0");
    assert!(!tunables::shrinkage_rerank_enabled());
}
