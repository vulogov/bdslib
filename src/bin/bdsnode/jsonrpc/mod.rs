mod count;
mod duplicates;
mod keys;
mod params;
mod primaries;
mod primary;
mod secondaries;
mod secondary;
mod timeline;

use jsonrpsee::RpcModule;

pub fn build_module() -> RpcModule<()> {
    let mut module = RpcModule::new(());
    timeline::register(&mut module);
    count::register(&mut module);
    keys::register(&mut module);
    duplicates::register(&mut module);
    primaries::register(&mut module);
    primary::register(&mut module);
    secondaries::register(&mut module);
    secondary::register(&mut module);
    module
}
