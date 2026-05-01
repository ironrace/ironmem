use ironmem::search::tunables;

#[test]
fn rerank_top_k_default_20() {
    std::env::remove_var("IRONMEM_RERANK_TOP_K");
    assert_eq!(tunables::rerank_top_k(), 20);
}
