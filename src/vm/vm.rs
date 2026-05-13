extern crate log;

use crate::vm::stdlib::init_bund_stdlib;

use bundcore::bundcore::Bund;
use easy_error::{Error, bail};
use parking_lot::RwLock;
use std::collections::BTreeSet;

pub fn init_adam() -> Result<(), Error> {
    let mut adam = Bund::new();
    init_stdlib(&mut adam)?;
    match crate::vm::BUND.get().is_some() {
        true => log::info!("BUND Adam instance already initialized."),
        false => match crate::vm::BUND.set(RwLock::new(adam)) {
            Ok(_) => {
                log::debug!("BUND Adam instance succesfully initialized.")
            }
            Err(err) => bail!("Error initializing BUND Adam instance: {:?}", err),
        },
    }
    Ok(())
}

pub fn init_stdlib(vm: &mut Bund) -> Result<(), Error> {
    init_bund_stdlib(vm)?;
    // Apply the operator-supplied sandbox AFTER the full stdlib is
    // registered: re-registering a disabled word with a denying stub
    // simply replaces the existing entry, so the order matters.
    crate::vm::policy::apply_to(vm)?;
    Ok(())
}

/// Snapshot the names of every word the Adam VM currently
/// recognises — inline functions, command words, methods, lambdas,
/// classes, and alias mappings.  Returns an empty set when the VM
/// hasn't been initialised yet so the caller can skip introspection
/// gracefully.
///
/// This is the source of truth for "is this a valid Bund word?"
/// checks (e.g. the `v2/to.bund` undefined-word dry-run).  The set
/// includes denied-stub registrations because policy-disabled words
/// still resolve at parse time; the eval step is where the denial
/// fires.
pub fn registered_word_names() -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let bund = match crate::vm::BUND.get() {
        Some(b) => b,
        None    => return out,
    };
    let guard = bund.read();
    let vm    = &guard.vm;
    // `register_inline("foo", …)` stores under key `"foo_inline"` (and
    // `"foo._inline"` for the workbench-suffix variant).  The
    // user-facing word is everything before the trailing `_inline`, so
    // we strip the suffix here to match what Bund scripts actually
    // type.  Command / method registrations key by the bare name, so
    // they pass through verbatim.
    for k in vm.inline_fun.keys() {
        match k.strip_suffix("_inline") {
            Some(name) if !name.is_empty() => { out.insert(name.to_owned()); }
            _ => { out.insert(k.clone()); }
        }
    }
    out.extend(vm.command_fun .keys().cloned());
    out.extend(vm.methods_fun .keys().cloned());
    out.extend(vm.lambdas     .keys().cloned());
    out.extend(vm.classes     .keys().cloned());
    out.extend(vm.name_mapping.keys().cloned());
    out
}
