use bundcore::bundcore::Bund;
use parking_lot::RwLock;
use std::sync::OnceLock;

pub(crate) static BUND: OnceLock<RwLock<Bund>> = OnceLock::new();

pub mod vm;
pub use vm::init_adam;

pub mod eval;
pub mod stdlib;

pub mod helpers;
