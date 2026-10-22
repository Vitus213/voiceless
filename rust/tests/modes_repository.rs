//! Contract of `voiceless::modes`: `ModesRepository` serves the builtin
//! modes from memory until the first mutation creates `modes.json`,
//! resolves modes by id or case-insensitive name with `quick` as the
//! default, persists user modes with unique names and increasing
//! `sort_order`, refuses mutations of builtins, reloads from disk, and
//! `render_template` / `prompt_table` expand prompts the way the pipeline
//! expects. No fakes: every test uses a real `modes.json` in a fresh
//! `tempdir`.

use std::fs;

use tempfile::tempdir;
use voiceless::modes::{builtin_modes, prompt_table, render_template, ModesError, ModesRepository};

/// Opening without a file lists exactly the builtins with `quick` first
/// and writes nothing; the first `add` creates a JSON array on disk.
#[test]
fn open_seeds_builtins_and_keeps_quick_first() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("modes.json");
    let mut repository = ModesRepository::open(path.clone()).expect("open repository");

    let modes = repository.list();
    assert_eq!(modes.len(), builtin_modes().len());
    assert_eq!(modes[0].id, "quick");
    assert!(
        !path.is_file(),
        "opening must not write until the first mutation"
    );
    repository
        .add("触发持久化", "p", "")
        .expect("first mutation");
    assert!(path.is_file(), "first mutation must persist modes.json");
    let raw = fs::read_to_string(&path).expect("modes.json readable");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert!(parsed.is_array());
}

/// `None` and blank resolve to `quick`; an id or a display name resolves
/// the same mode; an unknown identifier is `NotFound`.
#[test]
fn resolve_by_id_and_case_insensitive_name_defaults_to_quick() {
    let root = tempdir().expect("temporary root");
    let repository =
        ModesRepository::open(root.path().join("modes.json")).expect("open repository");

    assert_eq!(repository.resolve(None).expect("default").id, "quick");
    assert_eq!(repository.resolve(Some("  ")).expect("blank").id, "quick");
    assert_eq!(
        repository.resolve(Some("translate-en")).expect("by id").id,
        "translate-en"
    );
    assert_eq!(
        repository.resolve(Some("翻译为英文")).expect("by name").id,
        "translate-en"
    );
    assert!(matches!(
        repository.resolve(Some("不存在")),
        Err(ModesError::NotFound(_))
    ));
}

/// A new mode is non-builtin with `sort_order` 4 (after the four
/// builtins) and survives reopening the repository from the file.
#[test]
fn add_persists_user_mode_with_unique_name_and_next_sort_order() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("modes.json");
    let mut repository = ModesRepository::open(path.clone()).expect("open repository");

    let mode = repository
        .add("自定义", "原文：{text}", "整理中")
        .expect("add mode");
    assert!(!mode.builtin);
    assert_eq!(mode.sort_order, 4, "next after the four builtins");
    assert_eq!(repository.list().len(), builtin_modes().len() + 1);

    // Reopen proves persistence.
    let repository = ModesRepository::open(path).expect("reopen repository");
    assert_eq!(repository.get(&mode.id).expect("stored").name, "自定义");

    assert!(matches!(
        repository.resolve(Some("自定义")).expect("exists"),
        _
    ));
}

/// Duplicate names (even with surrounding whitespace) are `DuplicateName`,
/// updating or removing `quick` is `BuiltinImmutable`, and a blank name is
/// `EmptyName`.
#[test]
fn duplicate_names_and_builtin_mutations_are_rejected() {
    let root = tempdir().expect("temporary root");
    let mut repository =
        ModesRepository::open(root.path().join("modes.json")).expect("open repository");

    repository.add("重复", "p", "").expect("first add");
    assert!(matches!(
        repository.add("重复", "p", ""),
        Err(ModesError::DuplicateName(_))
    ));
    assert!(
        matches!(
            repository.add("  重复  ", "p", ""),
            Err(ModesError::DuplicateName(_))
        ),
        "name comparison is whitespace-normalized"
    );
    assert!(matches!(
        repository.update("quick", Some("改名"), None, None),
        Err(ModesError::BuiltinImmutable(_))
    ));
    assert!(matches!(
        repository.remove("quick"),
        Err(ModesError::BuiltinImmutable(_))
    ));
    assert!(matches!(
        repository.add("   ", "p", ""),
        Err(ModesError::EmptyName)
    ));
}

/// Removing a user mode returns it, leaves only the builtins and makes
/// `get` fail for its id.
#[test]
fn remove_user_mode_and_keep_builtins_intact() {
    let root = tempdir().expect("temporary root");
    let mut repository =
        ModesRepository::open(root.path().join("modes.json")).expect("open repository");

    let mode = repository.add("临时", "p", "").expect("add");
    let removed = repository.remove(&mode.id).expect("remove");
    assert_eq!(removed.id, mode.id);
    assert_eq!(repository.list().len(), builtin_modes().len());
    assert!(repository.get(&mode.id).is_err());
}

/// `reload` fails on an invalid file but keeps the in-memory list, so a
/// following `add` rewrites a valid file that then reloads.
#[test]
fn reload_repairs_missing_file_from_disk() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("modes.json");
    let mut repository = ModesRepository::open(path.clone()).expect("open repository");

    fs::write(&path, "not json").expect("corrupt file");
    assert!(
        repository.reload().is_err(),
        "invalid JSON must fail reload"
    );

    repository
        .add("恢复后", "p", "")
        .expect("add persists fresh file");
    let modes = repository.reload().expect("reload from disk");
    assert!(modes.iter().any(|mode| mode.name == "恢复后"));
}

/// Inserted text containing `{clipboard}` is not re-expanded, unknown
/// placeholders stay literal, and a template without placeholders is
/// returned unchanged.
#[test]
fn template_expansion_is_single_pass_and_preserves_unknown_placeholders() {
    assert_eq!(
        render_template("原文：{text}，选中：{selected}", "你好{clipboard}", "", ""),
        "原文：你好{clipboard}，选中："
    );
    assert_eq!(
        render_template("{unknown} stays", "t", "", ""),
        "{unknown} stays"
    );
    assert_eq!(
        render_template("no placeholders", "t", "s", "c"),
        "no placeholders"
    );
}

/// An empty organize prompt keeps the builtin prompt-optimize template; a
/// non-empty one replaces it and leaves `voice-polish` untouched.
#[test]
fn prompt_table_overrides_only_prompt_optimize() {
    let modes = builtin_modes();

    // Empty override: the builtin prompt-optimize prompt stays untouched.
    let defaults = prompt_table(&modes, "");
    let builtin_po = defaults
        .iter()
        .find(|(id, _)| id == "prompt-optimize")
        .expect("prompt-optimize present");
    assert!(
        builtin_po.1.contains("清晰、可执行"),
        "type4me prompt: {builtin_po:?}"
    );

    // Non-empty override: only prompt-optimize changes, other modes keep theirs.
    let overridden = prompt_table(&modes, "自定义：{text}");
    let po = overridden
        .iter()
        .find(|(id, _)| id == "prompt-optimize")
        .expect("prompt-optimize present");
    assert_eq!(po.1, "自定义：{text}");
    let polish = overridden
        .iter()
        .find(|(id, _)| id == "voice-polish")
        .expect("voice-polish present");
    assert!(
        polish.1.contains("删除口头语"),
        "unrelated mode kept: {polish:?}"
    );
}
