//! Word-level sandbox for the Bund VM.
//!
//! Every Bund word that touches the host (shell, filesystem, process
//! lifecycle, cluster-wide writes, …) is mapped to one of seven
//! [`Category`] values.  Operators turn off categories or individual
//! words via the `bund.disabled_categories` / `bund.disabled_words`
//! keys in `bds.hjson`; bdsnode parses them into a [`Policy`] and
//! calls [`init_policy`] once at startup.
//!
//! After [`crate::vm::stdlib::init_bund_stdlib`] registers every
//! word, [`apply_to`] re-registers each disabled word with
//! [`denied_stub`] — the word still exists in the VM, so Bund scripts
//! can detect-and-handle the denial, but invoking it returns a
//! "disabled by bdsnode policy" error instead of executing.
//!
//! Defaults: **nothing** is disabled.  All seven categories ship
//! enabled so existing deployments (scheduled scripts, dev runs)
//! keep working without config edits.  Operators who expose the VM
//! to less-trusted callers (chat snippets, the Bund playground)
//! should add at least `os_shell` and `process_control` to the
//! disabled set.
//!
//! ## Categories
//!
//! | Key                | Risk profile                                            |
//! |--------------------|---------------------------------------------------------|
//! | `os_shell`         | Arbitrary shell command execution → RCE on bdsnode.     |
//! | `process_control`  | `bund.exit` kills the bdsnode process; `sleep.seconds`  |
//! |                    | blocks a worker for arbitrary duration.                 |
//! | `filesystem_write` | Write/copy/move/remove arbitrary paths.                 |
//! | `filesystem_read`  | Read arbitrary files, fetch arbitrary URLs (SSRF),      |
//! |                    | enumerate directories, eval-of-file.                    |
//! | `code_eval`        | `bund.eval` and friends — recursive code execution and  |
//! |                    | dynamic file loading bypass static review.              |
//! | `cluster_admin`    | Cluster-replicated writes — adds/deletes/updates of     |
//! |                    | telemetry, docs, signals, templates, and (worst)        |
//! |                    | persistent BUND scripts that run as scheduled cron.     |
//! | `local_db_write`   | Local-only DB writes that bypass cluster replication —  |
//! |                    | silent drift between peers if used outside maintenance. |

extern crate log;

use std::collections::BTreeSet;
use std::sync::OnceLock;

use bundcore::bundcore::Bund;
use easy_error::{bail, Error};
use rust_multistackvm::multistackvm::VM;

/// Risk category a Bund word belongs to.  Order is deterministic
/// (used by logging).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Category {
    OsShell,
    ProcessControl,
    FilesystemWrite,
    FilesystemRead,
    CodeEval,
    ClusterAdmin,
    LocalDbWrite,
}

impl Category {
    /// Canonical wire name used in `bds.hjson`.  Aliases accepted on
    /// input (`shell` → `OsShell`, etc.) via [`from_wire`].
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::OsShell         => "os_shell",
            Self::ProcessControl  => "process_control",
            Self::FilesystemWrite => "filesystem_write",
            Self::FilesystemRead  => "filesystem_read",
            Self::CodeEval        => "code_eval",
            Self::ClusterAdmin    => "cluster_admin",
            Self::LocalDbWrite    => "local_db_write",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::OsShell         => "arbitrary shell command execution",
            Self::ProcessControl  => "kills bdsnode (`bund.exit`) or blocks workers (`sleep.seconds`)",
            Self::FilesystemWrite => "writes, copies, moves, or removes arbitrary filesystem paths",
            Self::FilesystemRead  => "reads files, fetches URLs (SSRF), enumerates directories, eval-of-file",
            Self::CodeEval        => "recursive BUND code execution and dynamic file loading",
            Self::ClusterAdmin    => "cluster-replicated writes incl. persistent scheduled BUND scripts",
            Self::LocalDbWrite    => "local-only DB writes that bypass cluster replication",
        }
    }

    /// Parse a wire name (case-insensitive, accepts common short
    /// aliases).  Returns `None` for unknown values so the parser can
    /// log + skip rather than refuse to start.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "os_shell"         | "shell"        => Some(Self::OsShell),
            "process_control"  | "process"      => Some(Self::ProcessControl),
            "filesystem_write" | "fs_write"     => Some(Self::FilesystemWrite),
            "filesystem_read"  | "fs_read"      => Some(Self::FilesystemRead),
            "code_eval"        | "eval"         => Some(Self::CodeEval),
            "cluster_admin"    | "cluster_write" => Some(Self::ClusterAdmin),
            "local_db_write"   | "db_write"     => Some(Self::LocalDbWrite),
            _ => None,
        }
    }

    pub fn all() -> &'static [Category] {
        &[
            Self::OsShell, Self::ProcessControl,
            Self::FilesystemWrite, Self::FilesystemRead,
            Self::CodeEval, Self::ClusterAdmin, Self::LocalDbWrite,
        ]
    }
}

/// Static word → category table.  Workbench-variant words (the
/// trailing `.` form) are listed separately because they're distinct
/// registrations in the VM.  Keep this list aligned with the actual
/// `register_inline` calls under `src/vm/stdlib/` — a CI check could
/// enforce it but for now it's manual.
const WORD_CATEGORY: &[(&str, Category)] = &[
    // ── os_shell ────────────────────────────────────────────────────
    ("system.shell",   Category::OsShell),
    ("system.shell.",  Category::OsShell),

    // ── process_control ────────────────────────────────────────────
    ("bund.exit",     Category::ProcessControl),
    ("sleep.seconds", Category::ProcessControl),

    // ── filesystem_write ───────────────────────────────────────────
    ("file.write",  Category::FilesystemWrite),
    ("file.write.", Category::FilesystemWrite),
    ("fs.cp",       Category::FilesystemWrite),
    ("fs.mv",       Category::FilesystemWrite),
    ("fs.rm",       Category::FilesystemWrite),

    // ── filesystem_read ────────────────────────────────────────────
    ("file",            Category::FilesystemRead),
    ("file.",           Category::FilesystemRead),
    ("url",             Category::FilesystemRead),
    ("url.",            Category::FilesystemRead),
    ("fs.ls",           Category::FilesystemRead),
    ("fs.ls.",          Category::FilesystemRead),
    ("fs.ls.dir",       Category::FilesystemRead),
    ("fs.ls.dir.",      Category::FilesystemRead),
    ("fs.cwd",          Category::FilesystemRead),
    ("fs.is_file",      Category::FilesystemRead),
    ("fs_is_file.",     Category::FilesystemRead),
    ("bund.eval-file",  Category::FilesystemRead),
    ("bund.eval-file.", Category::FilesystemRead),
    // `filename` canonicalises its argument — touches the FS to
    // resolve symlinks and verify existence — so it's a read, not a
    // pure string op.
    ("filename",        Category::FilesystemRead),
    ("filename.",       Category::FilesystemRead),

    // ── code_eval ──────────────────────────────────────────────────
    ("bund.eval",  Category::CodeEval),
    ("bund.eval.", Category::CodeEval),
    ("compile",    Category::CodeEval),
    ("apply",      Category::CodeEval),
    ("use",        Category::CodeEval),
    ("use.",       Category::CodeEval),

    // ── cluster_admin ──────────────────────────────────────────────
    // Adds / updates / deletes across every replicated store.
    ("cls.add",                  Category::ClusterAdmin),
    ("cls.add.",                 Category::ClusterAdmin),
    ("cls.add.batch",            Category::ClusterAdmin),
    ("cls.add.batch.",           Category::ClusterAdmin),
    ("cls.update",               Category::ClusterAdmin),
    ("cls.update.",              Category::ClusterAdmin),
    ("cls.delete",               Category::ClusterAdmin),
    ("cls.delete.",              Category::ClusterAdmin),

    ("cls.doc.add",              Category::ClusterAdmin),
    ("cls.doc.add.",             Category::ClusterAdmin),
    ("cls.doc.add.file",         Category::ClusterAdmin),
    ("cls.doc.add.file.",        Category::ClusterAdmin),
    ("cls.doc.update.content",   Category::ClusterAdmin),
    ("cls.doc.update.content.",  Category::ClusterAdmin),
    ("cls.doc.update.metadata",  Category::ClusterAdmin),
    ("cls.doc.update.metadata.", Category::ClusterAdmin),
    ("cls.doc.delete",           Category::ClusterAdmin),
    ("cls.doc.delete.",          Category::ClusterAdmin),
    ("cls.doc.reindex",          Category::ClusterAdmin),
    ("cls.doc.reindex.",         Category::ClusterAdmin),
    ("cls.doc.sync",             Category::ClusterAdmin),
    ("cls.doc.sync.",            Category::ClusterAdmin),

    ("cls.tpl.add",              Category::ClusterAdmin),
    ("cls.tpl.add.",             Category::ClusterAdmin),
    ("cls.tpl.update.body",      Category::ClusterAdmin),
    ("cls.tpl.update.body.",     Category::ClusterAdmin),
    ("cls.tpl.update.metadata",  Category::ClusterAdmin),
    ("cls.tpl.update.metadata.", Category::ClusterAdmin),
    ("cls.tpl.delete",           Category::ClusterAdmin),
    ("cls.tpl.delete.",          Category::ClusterAdmin),
    ("cls.tpl.reindex",          Category::ClusterAdmin),
    ("cls.tpl.reindex.",         Category::ClusterAdmin),

    ("cls.signal.emit",          Category::ClusterAdmin),
    ("cls.signal.emit.",         Category::ClusterAdmin),
    ("cls.signal.update",        Category::ClusterAdmin),
    ("cls.signal.update.",       Category::ClusterAdmin),

    // Scheduled BUND scripts are the highest-blast-radius admin word
    // here — adding one installs a persistent cron job on every
    // peer.  Operators exposing the VM to chat snippets MUST disable
    // either `cluster_admin` outright or at minimum the script.*
    // words.
    ("cls.script.add",       Category::ClusterAdmin),
    ("cls.script.add.",      Category::ClusterAdmin),
    ("cls.script.update",    Category::ClusterAdmin),
    ("cls.script.update.",   Category::ClusterAdmin),
    ("cls.script.delete",    Category::ClusterAdmin),
    ("cls.script.delete.",   Category::ClusterAdmin),

    // ── local_db_write ─────────────────────────────────────────────
    // Local-only writes bypass cluster replication; useful for
    // maintenance scripts but dangerous if a chat user invokes them.
    ("db.add",                    Category::LocalDbWrite),
    ("db.add.",                   Category::LocalDbWrite),
    ("db.sync",                   Category::LocalDbWrite),
    ("doc.add",                   Category::LocalDbWrite),
    ("doc.add.",                  Category::LocalDbWrite),
    ("doc.add.file",              Category::LocalDbWrite),
    ("doc.add.file.",             Category::LocalDbWrite),
    ("doc.add.vec",               Category::LocalDbWrite),
    ("doc.add.vec.",              Category::LocalDbWrite),
    ("doc.delete",                Category::LocalDbWrite),
    ("doc.delete.",               Category::LocalDbWrite),
    ("doc.update.content",        Category::LocalDbWrite),
    ("doc.update.content.",       Category::LocalDbWrite),
    ("doc.update.metadata",       Category::LocalDbWrite),
    ("doc.update.metadata.",      Category::LocalDbWrite),
    ("doc.store.content.vec",     Category::LocalDbWrite),
    ("doc.store.content.vec.",    Category::LocalDbWrite),
    ("doc.store.meta.vec",        Category::LocalDbWrite),
    ("doc.store.meta.vec.",       Category::LocalDbWrite),
    ("doc.reindex",               Category::LocalDbWrite),
    ("doc.reindex.",              Category::LocalDbWrite),
    ("doc.sync",                  Category::LocalDbWrite),
];

/// Operator-supplied policy.  Empty = nothing disabled (the default).
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub disabled_categories: BTreeSet<Category>,
    /// Explicit per-word denials.  Applied on top of category
    /// denials, so an operator can enable a category broadly but
    /// still block one specific word, or vice versa.
    pub disabled_words:      BTreeSet<String>,
}

impl Policy {
    pub fn is_empty(&self) -> bool {
        self.disabled_categories.is_empty() && self.disabled_words.is_empty()
    }

    /// Build a Policy from raw wire-format inputs.  Logs (warn-level)
    /// any value that doesn't parse rather than erroring — bdsnode
    /// starting with a partially-valid policy is strictly safer than
    /// refusing to start at all.
    pub fn from_wire(categories: &[String], words: &[String]) -> Self {
        let mut p = Policy::default();
        for c in categories {
            match Category::from_wire(c) {
                Some(cat) => { p.disabled_categories.insert(cat); }
                None      => log::warn!(
                    "[bund::policy] unknown disabled category {c:?} — ignoring \
                     (valid: {})", Category::all().iter()
                        .map(|c| c.as_wire()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
        for w in words {
            let w = w.trim().to_owned();
            if w.is_empty() { continue; }
            if !WORD_CATEGORY.iter().any(|(name, _)| *name == w) {
                log::warn!("[bund::policy] disabled word {w:?} is not in the \
                            sandbox table — it will still be registered as a \
                            denied stub but you should double-check the spelling");
            }
            p.disabled_words.insert(w);
        }
        p
    }
}

impl Policy {
    /// Read `bund.disabled_categories` and `bund.disabled_words` from
    /// the `bund:` block of `bds.hjson`.  Both keys accept a list of
    /// strings; either may be absent.  Returns an empty Policy on any
    /// read/parse failure or missing block — bdsnode logs the issue
    /// and starts up unsandboxed rather than refusing to run.
    ///
    /// Example `bds.hjson` fragment:
    ///
    /// ```hjson
    /// bund: {
    ///   disabled_categories: ["os_shell", "process_control"]
    ///   disabled_words:      ["cls.script.add", "cls.script.delete"]
    /// }
    /// ```
    pub fn load_from_hjson(path: &str) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(r)  => r,
            Err(_) => return Self::default(),
        };
        let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
            Ok(v)  => v,
            Err(_) => return Self::default(),
        };
        let bund = match val.as_object()
            .and_then(|o| o.get("bund"))
            .and_then(|v| v.as_object())
        {
            Some(o) => o,
            None    => return Self::default(),
        };
        let cats: Vec<String> = bund.get("disabled_categories")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let words: Vec<String> = bund.get("disabled_words")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        Policy::from_wire(&cats, &words)
    }
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Install the process-wide policy.  Must be called once at bdsnode
/// startup, BEFORE the first VM is initialised.  Idempotent — second
/// and subsequent calls silently no-op (matches the rest of bdslib's
/// `init_*` conventions).
pub fn init_policy(p: Policy) {
    let _ = POLICY.set(p);
}

/// Read the active policy.  Falls back to an empty policy (nothing
/// disabled) when [`init_policy`] was never called — preserves
/// backward-compatible behaviour for library users that bypass
/// bdsnode startup.
pub fn policy() -> &'static Policy {
    POLICY.get_or_init(Policy::default)
}

/// Returns the category a word belongs to, or `None` for words that
/// aren't tracked by the sandbox (the bulk of math/string/console
/// helpers).
pub fn category_of(word: &str) -> Option<Category> {
    WORD_CATEGORY.iter().find(|(n, _)| *n == word).map(|(_, c)| *c)
}

/// List every Bund word that the active policy currently denies
/// (category-disabled words ∪ explicitly-listed words).  Order is
/// alphabetical so the output is stable for diffing / hashing.
///
/// Used by the v2/to.bund translator to splice a "DO NOT use these
/// words" section into the system prompt, and by introspection RPCs
/// that want to show the operator-active blocklist.
pub fn effective_disabled_words() -> Vec<String> {
    let p = policy();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for (word, cat) in WORD_CATEGORY {
        if p.disabled_categories.contains(cat) {
            out.insert((*word).to_owned());
        }
    }
    for w in &p.disabled_words {
        out.insert(w.clone());
    }
    out.into_iter().collect()
}

/// Group the active policy's denied words by category for prompt
/// rendering / log output.  Words listed in `disabled_words` but not
/// present in the static `WORD_CATEGORY` table appear under the
/// synthetic key `"explicit"`.
///
/// Each inner Vec is sorted; the outer Vec is sorted by wire-name of
/// the category (or `"explicit"` last).
pub fn effective_disabled_by_category() -> Vec<(String, Vec<String>)> {
    use std::collections::BTreeMap;
    let p = policy();
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (word, cat) in WORD_CATEGORY {
        if p.disabled_categories.contains(cat) {
            groups.entry(cat.as_wire().to_owned())
                .or_default()
                .insert((*word).to_owned());
        }
    }
    for w in &p.disabled_words {
        let key = category_of(w).map(|c| c.as_wire().to_owned())
            .unwrap_or_else(|| "explicit".to_owned());
        groups.entry(key).or_default().insert(w.clone());
    }
    groups.into_iter()
        .map(|(k, v)| (k, v.into_iter().collect::<Vec<_>>()))
        .collect()
}

/// Stub that all disabled words are re-registered as.  Returns a
/// clear error message pointing the operator at the config knob.
/// The function does not know its own name (`VMInlineFn` is a bare
/// fn pointer, no closure capture), so the error is generic — log
/// lines emitted at startup enumerate exactly which words and
/// categories are disabled.
pub fn denied_stub(_vm: &mut VM) -> Result<&mut VM, Error> {
    bail!(
        "BUND word disabled by bdsnode policy. \
         Edit `bund.disabled_categories` / `bund.disabled_words` in bds.hjson \
         (or check startup logs to see which words are currently denied)."
    );
}

/// Apply the active policy to a freshly-initialised VM.  Walks the
/// word table, and for every word whose category is disabled OR
/// whose name is in `disabled_words`, re-registers the inline
/// function with [`denied_stub`].
///
/// Cheap when nothing is disabled (the common path) — single
/// allocation-free scan of the static table.
pub fn apply_to(vm: &mut Bund) -> Result<(), Error> {
    let p = policy();
    if p.is_empty() { return Ok(()); }
    for (word, cat) in WORD_CATEGORY {
        let denied = p.disabled_categories.contains(cat)
            || p.disabled_words.contains(*word);
        if !denied { continue; }
        // `register_inline` removes the existing entry first; this is
        // a true replacement, not a duplicate.
        let _ = vm.vm.register_inline(word.to_string(), denied_stub)?;
    }
    // Explicit denials for words not in the table — register a stub
    // anyway so subsequent invocations get the policy message rather
    // than "word not registered".
    for w in &p.disabled_words {
        if !WORD_CATEGORY.iter().any(|(n, _)| *n == w.as_str()) {
            let _ = vm.vm.register_inline(w.clone(), denied_stub)?;
        }
    }
    Ok(())
}

/// One-shot startup log summarising which words/categories are
/// disabled.  Call from bdsnode `main` once [`init_policy`] has run
/// so operators see the active sandbox at a glance.
pub fn log_policy_summary() {
    let p = policy();
    if p.is_empty() {
        log::info!(
            "[bund::policy] no Bund words are disabled — every category is enabled \
             (configure via `bund.disabled_categories` / `bund.disabled_words` in \
             bds.hjson if exposing the VM to less-trusted callers)"
        );
        return;
    }
    for c in &p.disabled_categories {
        let words_in_cat: Vec<&str> = WORD_CATEGORY.iter()
            .filter(|(_, x)| x == c)
            .map(|(n, _)| *n)
            .collect();
        log::warn!(
            "[bund::policy] category {} DISABLED ({}): {} word(s) blocked: {}",
            c.as_wire(), c.description(), words_in_cat.len(),
            words_in_cat.join(", ")
        );
    }
    if !p.disabled_words.is_empty() {
        let extra: Vec<&str> = p.disabled_words.iter()
            .filter(|w| {
                let cat = category_of(w);
                match cat {
                    Some(c) => !p.disabled_categories.contains(&c),
                    None    => true,
                }
            })
            .map(|s| s.as_str())
            .collect();
        if !extra.is_empty() {
            log::warn!(
                "[bund::policy] additional per-word denials beyond the category set: {}",
                extra.join(", ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_wire_roundtrip() {
        for c in Category::all() {
            assert_eq!(Category::from_wire(c.as_wire()), Some(*c));
        }
    }

    #[test]
    fn category_aliases_parse() {
        assert_eq!(Category::from_wire("shell"),         Some(Category::OsShell));
        assert_eq!(Category::from_wire("FS_WRITE"),      Some(Category::FilesystemWrite));
        assert_eq!(Category::from_wire("  eval  "),      Some(Category::CodeEval));
        assert_eq!(Category::from_wire("db_write"),      Some(Category::LocalDbWrite));
        assert_eq!(Category::from_wire("cluster_write"), Some(Category::ClusterAdmin));
    }

    #[test]
    fn category_unknown_returns_none() {
        assert_eq!(Category::from_wire("totally_made_up"), None);
        assert_eq!(Category::from_wire(""), None);
    }

    #[test]
    fn from_wire_skips_invalid_categories() {
        let p = Policy::from_wire(&["os_shell".into(), "nonsense".into()], &[]);
        assert_eq!(p.disabled_categories.len(), 1);
        assert!(p.disabled_categories.contains(&Category::OsShell));
    }

    #[test]
    fn category_of_known_words() {
        assert_eq!(category_of("system.shell"),  Some(Category::OsShell));
        assert_eq!(category_of("system.shell."), Some(Category::OsShell));
        assert_eq!(category_of("bund.exit"),     Some(Category::ProcessControl));
        assert_eq!(category_of("sleep.seconds"), Some(Category::ProcessControl));
        assert_eq!(category_of("fs.rm"),         Some(Category::FilesystemWrite));
        assert_eq!(category_of("url"),           Some(Category::FilesystemRead));
        assert_eq!(category_of("bund.eval"),     Some(Category::CodeEval));
        assert_eq!(category_of("cls.script.add"), Some(Category::ClusterAdmin));
        assert_eq!(category_of("doc.delete"),    Some(Category::LocalDbWrite));
    }

    #[test]
    fn category_of_unknown_returns_none() {
        // These are safe pure-string operations — NOT gated.
        assert_eq!(category_of("system.path.split"),    None);
        assert_eq!(category_of("system.path.filename"), None);
        assert_eq!(category_of("math.exp"),             None);
        assert_eq!(category_of("display"),              None);
    }

    #[test]
    fn filename_is_filesystem_read_not_pure_string() {
        // `filename` calls Path::canonicalize() which touches the FS;
        // it must be classified, not whitelisted.
        assert_eq!(category_of("filename"),  Some(Category::FilesystemRead));
        assert_eq!(category_of("filename."), Some(Category::FilesystemRead));
    }

    #[test]
    fn empty_policy_is_default() {
        let p = Policy::default();
        assert!(p.is_empty());
    }
}
