mod count;
mod params;
mod primaries;
mod primary;
mod timeline;

use jsonrpsee::RpcModule;

pub fn build_module() -> RpcModule<()> {
    let mut module = RpcModule::new(());
    timeline::register(&mut module);
    count::register(&mut module);
    primaries::register(&mut module);
    primary::register(&mut module);
    module
}
