use bundcore::bundcore::Bund;
use easy_error::{Error, err_msg};
use parking_lot::RwLock;
use std::sync::OnceLock;

pub(crate) static BUND: OnceLock<RwLock<Bund>> = OnceLock::new();

pub mod eval;
pub mod helpers;
pub mod stdlib;
pub mod vm;

pub use vm::init_adam;

/// Initialise the BUND VM (if not already done) and evaluate `code`.
///
/// Calls [`init_adam`] on the first invocation, so callers do not need to
/// initialise the singleton separately.
pub fn bund_eval(code: &str) -> Result<(), Error> {
    init_adam()?;
    let bund = BUND
        .get()
        .ok_or_else(|| err_msg("BUND VM not initialised"))?;
    let mut guard = bund.write();
    helpers::eval::bund_compile_and_eval(&mut guard.vm, code.to_string())?;
    Ok(())
}
