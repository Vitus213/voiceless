//! Guards of the model store that hold across processes: the CLI, the
//! daemon and a standalone console all reach the same files.
//!
//! Two advisory locks per model, as files under `<data>/models/.locks/`:
//!
//! - `<id>.in-use`: shared by whoever runs a model (the daemon's warm
//!   engine, a session, a console job) for as long as its files are open,
//!   and taken exclusively by `ModelManager::remove`. A removal that finds
//!   a lease answers `ModelError::InUse`, from any process, and the
//!   running engine keeps its files. The kernel drops a lease with the
//!   process that holds it, so a runtime that crashed never pins a model.
//! - `<id>.mutation`: exclusive, held by the one install, import or
//!   removal that changes a model. A second one answers
//!   `ModelError::Busy` instead of racing the first over the same payload
//!   and pointer.
//!
//! Leases are best effort for their holder and strict for a remover: a
//! session that cannot create its lock file (a store it cannot write) still
//! runs, because a remover needs that same file to delete anything. The
//! lock files are never deleted, so a lease is always taken on the file a
//! remover looks at.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::PathBuf;

use super::ModelError;
use super::ModelManager;

const LOCKS: &str = ".locks";
const IN_USE: &str = ".in-use";
const MUTATION: &str = ".mutation";
/// Longest id that still leaves room for a lock file name.
const MAX_ID_BYTES: usize = 200;

/// The shared leases of the models a running engine or session uses.
/// Dropping it releases them all.
#[derive(Debug, Default)]
pub struct ModelLeases {
    leases: Vec<(String, File)>,
}

impl ModelLeases {
    /// Whether `model_id` is among the leased models.
    pub fn holds(&self, model_id: &str) -> bool {
        self.leases.iter().any(|(id, _)| id == model_id)
    }
}

impl Drop for ModelLeases {
    fn drop(&mut self) {
        for (_, file) in &self.leases {
            let _ = file.unlock();
        }
    }
}

/// An exclusive lock held while one operation changes a model. Dropping it
/// releases the lock.
#[derive(Debug)]
pub(crate) struct MutationLock {
    file: File,
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        // A fork in another thread can keep the file description alive
        // until exec, even after this process closes its descriptor.
        let _ = self.file.unlock();
    }
}

/// A model id is also the name of its directory and of its lock files: it
/// must be one plain path component.
pub(super) fn validate_id(model_id: &str) -> Result<(), ModelError> {
    let plain = !model_id.is_empty()
        && !model_id.starts_with('.')
        && model_id
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/' && byte != b'\\');
    if plain && model_id.len() <= MAX_ID_BYTES {
        Ok(())
    } else {
        Err(ModelError::UnknownId(model_id.to_owned()))
    }
}

impl ModelManager {
    /// The open lock file of `model_id` and `kind`, created on first use.
    fn lock_file(&self, model_id: &str, kind: &str) -> Result<File, ModelError> {
        validate_id(model_id)?;
        let directory = self.models_root.join(LOCKS);
        fs::create_dir_all(&directory)?;
        let path: PathBuf = directory.join(format!("{model_id}{kind}"));
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?)
    }

    /// Take the right to install, import or remove `model_id`.
    /// `Busy` while another operation holds it.
    pub(crate) fn lock_mutation(&self, model_id: &str) -> Result<MutationLock, ModelError> {
        let file = self.lock_file(model_id, MUTATION)?;
        match file.try_lock() {
            Ok(()) => Ok(MutationLock { file }),
            Err(TryLockError::WouldBlock) => Err(ModelError::Busy(model_id.to_owned())),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// The exclusive in-use lock of `model_id`, as a removal needs it.
    /// `InUse` while an engine or session holds a lease.
    pub(super) fn lock_in_use(&self, model_id: &str) -> Result<MutationLock, ModelError> {
        let file = self.lock_file(model_id, IN_USE)?;
        match file.try_lock() {
            Ok(()) => Ok(MutationLock { file }),
            Err(TryLockError::WouldBlock) => Err(ModelError::InUse(model_id.to_owned())),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// Mark models as in use for as long as the returned leases live, so
    /// that no process removes their files meanwhile (`remove` answers
    /// `InUse`). A model that cannot be leased is skipped with a line on
    /// stderr (see the module doc); it never fails the caller.
    pub fn lease(&self, ids: impl IntoIterator<Item = impl AsRef<str>>) -> ModelLeases {
        let mut leases = Vec::new();
        for model_id in ids {
            let model_id = model_id.as_ref();
            let leased = self.lock_file(model_id, IN_USE).and_then(|file| {
                file.lock_shared()?;
                Ok(file)
            });
            match leased {
                Ok(file) => leases.push((model_id.to_owned(), file)),
                Err(error) => {
                    eprintln!("Voiceless: cannot mark model {model_id} as in use: {error}")
                }
            }
        }
        ModelLeases { leases }
    }

    /// Whether an engine or session holds `model_id`, or a removal of it
    /// is running.
    pub fn in_use(&self, model_id: &str) -> bool {
        matches!(self.lock_in_use(model_id), Err(ModelError::InUse(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ModelManager) {
        let root = tempfile::tempdir().expect("temporary root");
        let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
        (root, manager)
    }

    fn install_placeholder(manager: &ModelManager, id: &str) -> std::path::PathBuf {
        let pointer_dir = manager.models_root.join(id);
        fs::create_dir_all(pointer_dir.join("current")).expect("placeholder payload");
        pointer_dir
    }

    /// A leased model is refused to `remove` with `InUse` and keeps its
    /// files; once the lease is dropped it can be removed.
    #[test]
    fn a_leased_model_survives_removal_until_the_lease_is_dropped() {
        let (_root, manager) = store();
        let pointer_dir = install_placeholder(&manager, "leased-model");

        let leases = manager.lease(["leased-model"]);
        assert!(leases.holds("leased-model"));
        assert!(manager.in_use("leased-model"));
        let error = manager.remove("leased-model").expect_err("leased");
        assert!(
            matches!(&error, ModelError::InUse(id) if id == "leased-model"),
            "{error:?}"
        );
        assert!(pointer_dir.exists(), "the leased files stay");
        assert!(
            manager.in_use("leased-model"),
            "the refused removal left the lease alone"
        );

        drop(leases);
        assert!(!manager.in_use("leased-model"));
        assert!(manager.remove("leased-model").expect("remove"));
        assert!(!pointer_dir.exists());
    }

    /// Leases are shared: an engine and a session of the same model, in
    /// this or another process, hold one at the same time.
    #[test]
    fn leases_of_the_same_model_do_not_conflict() {
        let (_root, manager) = store();
        let engine = manager.lease(["model-a", "silero-vad"]);
        let session = manager.lease(["model-a", "silero-vad"]);
        assert!(engine.holds("model-a") && session.holds("model-a"));
        assert!(!manager.in_use("another-model"));
    }

    /// A second operation on the same model is `Busy` while the first
    /// holds the mutation lock, in this process as in any other.
    #[test]
    fn a_second_installer_of_a_model_is_busy() {
        let (_root, manager) = store();
        let first = manager.lock_mutation("model-a").expect("first");
        assert!(matches!(
            manager.lock_mutation("model-a").expect_err("second"),
            ModelError::Busy(id) if id == "model-a"
        ));
        drop(first);
        assert!(manager.lock_mutation("model-a").is_ok());
    }

    /// An id is a directory and a file name: anything with a path
    /// separator or a leading dot is unknown, before a file is touched.
    #[test]
    fn ids_must_be_one_plain_path_component() {
        for id in [
            "",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            ".hidden",
            "with space",
            "tab\t",
        ] {
            assert!(
                matches!(validate_id(id), Err(ModelError::UnknownId(unknown)) if unknown == id),
                "{id:?}"
            );
        }
        for id in [
            "qwen3-asr-1.7b-gguf",
            "imported-qwen3-asr-0123abcdef456-gguf",
            "silero_vad",
        ] {
            assert!(validate_id(id).is_ok(), "{id}");
        }
    }

    /// `remove` refuses such an id and leaves everything in place.
    #[test]
    fn remove_refuses_ids_that_are_not_plain_model_names() {
        let (root, manager) = store();
        fs::create_dir_all(root.path().join("data/models/versions/kept")).expect("payload");
        let error = manager
            .remove("../versions/kept")
            .expect_err("not a model id");
        assert!(matches!(error, ModelError::UnknownId(id) if id == "../versions/kept"));
        assert!(root.path().join("data/models/versions/kept").is_dir());
        assert!(manager.remove("../escape").is_err());
    }
}
