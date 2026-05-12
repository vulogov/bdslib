//! End-to-end sandbox test for `bdslib::bund_policy`.
//!
//! Spins up a fresh `Bund` VM with `init_stdlib`, installs a policy
//! that disables one category + one explicit word, and asserts:
//!
//! 1. Disabled words execute to a "disabled by bdsnode policy" error.
//! 2. Sibling words in the SAME module that DON'T belong to the
//!    disabled category remain executable.
//! 3. `Policy::is_empty()` reflects loaded state correctly.
//! 4. `load_from_hjson` parses both list keys.
//!
//! The OnceLock that holds the process-wide policy is set on first
//! access; the test binary is the only consumer of that lock, so a
//! single `init_policy` call at the top of the binary is sufficient
//! for every test inside it.

use bdslib::bund_policy::{self, Category, Policy};
use bundcore::bundcore::Bund;

/// Policy used by every test in this binary.  Installed once via the
/// OnceLock; ordering doesn't matter because the policy is consulted
/// at VM-init time, not at word-execution time.
fn ensure_policy() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let p = Policy::from_wire(
            &["os_shell".into(), "process_control".into()],
            &["fs.cwd".into()],     // explicit per-word denial
        );
        bund_policy::init_policy(p);
    });
}

fn fresh_vm() -> Bund {
    let mut bund = Bund::new();
    bdslib::vm::vm::init_stdlib(&mut bund).expect("init_stdlib");
    bund
}

/// Run a single inline word against a fresh VM.  Returns `Ok(())`
/// on success and `Err(message)` on failure — discards the
/// `&mut VM` return value because it isn't `Debug`-printable.
fn invoke_word(word: &str, seed: Option<&str>) -> Result<(), String> {
    let mut bund = fresh_vm();
    let code = match seed {
        Some(s) => format!("\"{}\" {}", s.replace('"', "\\\""), word),
        None    => word.to_owned(),
    };
    match bdslib::vm::helpers::eval::bund_compile_and_eval(&mut bund.vm, code) {
        Ok(_)  => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn disabled_category_word_returns_policy_error() {
    ensure_policy();
    // `system.shell` is in the disabled `os_shell` category.  It
    // takes a string argument off the stack — seed something safe.
    let err = invoke_word("system.shell", Some("echo hi"))
        .expect_err("disabled word must error");
    assert!(err.contains("disabled by bdsnode policy"),
        "error must explain the policy denial: {err}");
}

#[test]
fn disabled_workbench_variant_also_denied() {
    ensure_policy();
    // The workbench-suffixed variant is a separate registration; it
    // must be gated independently.  The denied_stub bails BEFORE
    // touching the stack/workbench, so we can invoke the `.` form
    // with no workbench content and still observe the policy error.
    let err = invoke_word("system.shell.", None)
        .expect_err("workbench variant must error");
    assert!(err.contains("disabled by bdsnode policy"),
        "workbench variant error must explain policy denial: {err}");
}

#[test]
fn process_control_words_are_denied() {
    ensure_policy();
    // `sleep.seconds` is in `process_control`; calling it would
    // otherwise hang the test.  The stub returns immediately.
    let err = invoke_word("sleep.seconds", Some("1"))
        .expect_err("sleep.seconds must be denied");
    assert!(err.contains("disabled by bdsnode policy"), "got: {err}");

    // `bund.exit` is the scary one — without the stub it would
    // `process::exit(0)` and TAKE DOWN the test runner.  The stub
    // turns that into a recoverable error.
    let err = invoke_word("bund.exit", None)
        .expect_err("bund.exit must be denied (otherwise the test runner would exit)");
    assert!(err.contains("disabled by bdsnode policy"), "got: {err}");
}

#[test]
fn explicit_per_word_denial_overrides_category_default() {
    ensure_policy();
    // `fs.cwd` is in `filesystem_read` which is NOT in our disabled
    // categories — but we listed `fs.cwd` explicitly.
    let err = invoke_word("fs.cwd", None)
        .expect_err("explicit denial must take effect");
    assert!(err.contains("disabled by bdsnode policy"), "got: {err}");
}

#[test]
fn non_disabled_filesystem_words_still_work() {
    ensure_policy();
    // `system.path.split` is a pure string-parsing word (no FS I/O)
    // — NOT in any dangerous category, so policy never touches it.
    if let Err(e) = invoke_word("system.path.split", Some("/tmp/foo.txt")) {
        panic!("non-gated path word should execute: {e}");
    }
}

#[test]
fn safe_math_word_unaffected_by_sandbox() {
    ensure_policy();
    // Sanity — confirm the policy didn't accidentally over-block by
    // running a totally unrelated math word.  `math.exp` takes a
    // float, hence `2.0`.
    let mut bund = fresh_vm();
    let r = bdslib::vm::helpers::eval::bund_compile_and_eval(
        &mut bund.vm, "2.0 math.exp".to_string());
    if let Err(e) = r {
        panic!("math.exp should still execute: {e}");
    }
}

#[test]
fn policy_default_is_empty() {
    let p = Policy::default();
    assert!(p.is_empty());
    assert!(p.disabled_categories.is_empty());
    assert!(p.disabled_words.is_empty());
}

#[test]
fn policy_from_wire_accepts_aliases() {
    let p = Policy::from_wire(
        &["shell".into(), "fs_write".into(), "DB_WRITE".into(), "bogus".into()],
        &[],
    );
    assert_eq!(p.disabled_categories.len(), 3);
    assert!(p.disabled_categories.contains(&Category::OsShell));
    assert!(p.disabled_categories.contains(&Category::FilesystemWrite));
    assert!(p.disabled_categories.contains(&Category::LocalDbWrite));
}

#[test]
fn load_from_hjson_round_trips() {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, r#"{{
        bund: {{
            disabled_categories: ["os_shell", "cluster_admin"]
            disabled_words:      ["cls.script.add", "fs.rm"]
        }}
    }}"#).unwrap();
    let p = Policy::load_from_hjson(f.path().to_str().unwrap());
    assert_eq!(p.disabled_categories.len(), 2);
    assert!(p.disabled_categories.contains(&Category::OsShell));
    assert!(p.disabled_categories.contains(&Category::ClusterAdmin));
    assert_eq!(p.disabled_words.len(), 2);
    assert!(p.disabled_words.contains("cls.script.add"));
    assert!(p.disabled_words.contains("fs.rm"));
}

#[test]
fn load_from_hjson_missing_bund_block_returns_empty() {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, r#"{{ dbpath: "./db" }}"#).unwrap();
    let p = Policy::load_from_hjson(f.path().to_str().unwrap());
    assert!(p.is_empty());
}
