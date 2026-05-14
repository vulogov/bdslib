extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;
use rust_multistackvm::stdlib::execute_types::CF;

pub mod conditional_fmt;

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    // Poison-recover: a panic in another Bund worker while holding
    // `CF` must not permanently brick conditional-format init for
    // every subsequent VM.  The map's contents survive a panic
    // intact, so `into_inner()` is the correct recovery.
    let mut cf = CF.lock().unwrap_or_else(|e| e.into_inner());

    cf.insert("fmt".to_string(), conditional_fmt::conditional_run);

    drop(cf);

    let _ = vm
        .vm
        .register_inline("fmt".to_string(), conditional_fmt::stdlib_conditional_fmt);
    Ok(())
}
