//! Cluster-aware Bund stdlib words.
//!
//! Each `cls.*` word is a thin wrapper around a `vm::api::*` helper:
//! it pulls arguments off the stack (or workbench), calls the helper,
//! and pushes the result.  Cluster mode detection is automatic — a
//! script that uses `cls.add` runs as a local-only DB add when
//! cluster mode is off, and replicates to peers when on.
//!
//! Two families:
//!
//! - **Bare name** (`cls.add`, `cls.search`, …) — pulls/pushes via
//!   the data stack.
//! - **Trailing-dot variant** (`cls.add.`, `cls.search.`, …) —
//!   same arity, but pulls/pushes via the workbench.
//!
//! `?cluster.meta` reads the per-thread cluster meta cell populated
//! by the most-recent `cls.*` call (Map result, or `nodata` if no
//! cluster call ran on this thread yet).

extern crate log;

use bundcore::bundcore::Bund;
use easy_error::Error;

pub mod add;
pub mod analysis;
pub mod docs;
pub mod helpers;
pub mod keys;
pub mod llm;
pub mod meta;
pub mod primaries;
pub mod scripts;
pub mod search;
pub mod signals;
pub mod templates;

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    meta::init_stdlib(vm)?;
    add::init_stdlib(vm)?;
    search::init_stdlib(vm)?;
    analysis::init_stdlib(vm)?;
    signals::init_stdlib(vm)?;
    keys::init_stdlib(vm)?;
    primaries::init_stdlib(vm)?;
    docs::init_stdlib(vm)?;
    templates::init_stdlib(vm)?;
    scripts::init_stdlib(vm)?;
    llm::init_stdlib(vm)?;
    Ok(())
}
