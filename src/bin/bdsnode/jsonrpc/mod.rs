mod add;
mod add_batch;
mod eval;
mod count;
mod duplicates;
mod fulltext;
mod fulltext_get;
mod fulltext_recent;
mod keys;
mod params;
mod primaries;
mod primary;
mod search;
mod search_get;
mod secondaries;
mod secondary;
mod shards;
mod timeline;

use jsonrpsee::RpcModule;

pub fn build_module() -> RpcModule<()> {
    let mut module = RpcModule::new(());
    add::register(&mut module);
    add_batch::register(&mut module);
    eval::register(&mut module);
    timeline::register(&mut module);
    count::register(&mut module);
    keys::register(&mut module);
    duplicates::register(&mut module);
    shards::register(&mut module);
    primaries::register(&mut module);
    primary::register(&mut module);
    secondaries::register(&mut module);
    secondary::register(&mut module);
    fulltext::register(&mut module);
    fulltext_get::register(&mut module);
    fulltext_recent::register(&mut module);
    search::register(&mut module);
    search_get::register(&mut module);
    module
}
