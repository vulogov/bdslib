mod count;
mod timeline;

use jsonrpsee::RpcModule;

pub fn build_module() -> RpcModule<()> {
    let mut module = RpcModule::new(());
    timeline::register(&mut module);
    count::register(&mut module);
    module
}
