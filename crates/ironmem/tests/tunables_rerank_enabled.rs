use ironmem::search::tunables;

#[test]
fn rerank_enabled_with_llm_haiku() {
    std::env::set_var("IRONMEM_RERANK", "llm_haiku");
    assert!(
        tunables::rerank_enabled(),
        "IRONMEM_RERANK=llm_haiku should enable the rerank stage"
    );
}
