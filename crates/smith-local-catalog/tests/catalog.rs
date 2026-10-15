use smith_local_catalog::*;
use std::time::Duration;

#[test]
fn malformed_unknown_and_oversized_storage_never_becomes_empty() {
    for bytes in [b"".as_slice(),b"null",b"{}",b"{\"format_version\":1,\"revision\":0,\"entries\":[],\"selected_for_review\":null,\"load\":true}"] {
        assert!(Catalog::decode(bytes).is_err());
    }
    assert!(matches!(
        Catalog::decode(&vec![b' '; MAX_RECORD_BYTES + 1]),
        Err(Error::LimitExceeded)
    ));
    assert!(matches!(
        Catalog::decode(
            br#"{"format_version":2,"revision":0,"entries":[],"selected_for_review":null}"#
        ),
        Err(Error::UnsupportedVersion)
    ));
    let empty = Catalog::empty();
    assert_eq!(empty.storage_base_sha256(), None);
    let bytes = empty.encode().unwrap();
    let restored = Catalog::decode(&bytes).unwrap();
    assert_eq!(restored.revision(), 0);
    assert_eq!(restored.storage_base_sha256().unwrap().len(), 64);
    assert!(restored.list().is_empty());
}

#[test]
fn cancelled_and_invalid_budgets_are_typed() {
    assert!(matches!(
        ReadControl::new(Duration::ZERO),
        Err(Error::InvalidSelection)
    ));
    assert!(matches!(
        ReadControl::new(Duration::from_secs(301)),
        Err(Error::InvalidSelection)
    ));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
        path::{Path, PathBuf},
    };
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            fs::write(
                root.join("runner"),
                b"synthetic executable; must never execute",
            )
            .unwrap();
            fs::set_permissions(root.join("runner"), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(root.join("model.gguf"), b"GGUF synthetic model bytes only").unwrap();
            Self { _dir: dir, root }
        }
        fn request(&self) -> CaptureRequest {
            let source = Source {
                label: "explicit synthetic selection".into(),
                revision: "fixture-v1".into(),
                origin: Origin::ExternalUserSelected,
            };
            CaptureRequest {
                description: ModelDescription {
                    format: ModelFormat::Gguf,
                    architecture: "synthetic".into(),
                    declared_capabilities: vec![DeclaredCapability::TextChat],
                    declared_context_limit: Some(4096),
                    hints: AdapterHints::LlamaGguf(GgufHints {
                        context_tokens: Some(1024),
                        output_tokens: Some(128),
                        mmap: Some(false),
                        ..Default::default()
                    }),
                },
                runtime: PackageSelection {
                    root: self.root.clone(),
                    source: source.clone(),
                    files: vec![SelectedFile {
                        name: "runner".into(),
                        role: FileRole::Executable,
                    }],
                },
                model: PackageSelection {
                    root: self.root.clone(),
                    source,
                    files: vec![SelectedFile {
                        name: "model.gguf".into(),
                        role: FileRole::Weights,
                    }],
                },
            }
        }
        fn capture(&self) -> CapturedAssets {
            capture(self.request(), &control()).unwrap()
        }
        fn catalog(&self) -> Catalog {
            Catalog::empty()
                .insert(0, "My synthetic GGUF".into(), self.capture())
                .unwrap()
        }
    }
    fn control() -> ReadControl {
        ReadControl::new(Duration::from_secs(10)).unwrap()
    }
    fn context() -> PreparationContext {
        PreparationContext {
            session_id: "fresh-profile-session".into(),
            config_revision: "connection-revision-9".into(),
            inventory_generation: "inventory-generation-1".into(),
        }
    }
    fn selected(f: &Fixture) -> Catalog {
        let c = f.catalog();
        let e = &c.list()[0];
        c.select_for_review(1, &e.id, e.revision).unwrap()
    }
    fn review(c: &Catalog) -> SelectionReview {
        let e = &c.list()[0];
        c.review(&e.id, e.revision, context()).unwrap()
    }

    #[test]
    fn browse_restore_and_select_do_not_touch_missing_files_or_apply_hints() {
        let f = Fixture::new();
        let c = f.catalog();
        let encoded = c.encode().unwrap();
        let original = c.record().entries[0].manifest.clone();
        fs::remove_file(f.root.join("runner")).unwrap();
        fs::remove_file(f.root.join("model.gguf")).unwrap();
        let c = Catalog::decode(&encoded).unwrap();
        let entry = &c.list()[0];
        assert_eq!(entry.origin, Origin::ExternalUserSelected);
        assert_eq!(entry.completeness, Completeness::SelectedFilesOnly);
        assert!(!entry.selected_for_review);
        let c = c.select_for_review(1, &entry.id, entry.revision).unwrap();
        assert!(c.list()[0].selected_for_review);
        assert_eq!(c.details(&entry.id).unwrap().manifest, original);
        assert!(!c.encode().unwrap().windows(6).any(|w| w == b"active"));
        assert!(matches!(
            c.prepare(&review(&c), &context(), &control()),
            Err(Error::UnsafeFile)
        ));
        let renamed = c
            .rename(
                2,
                &entry.id,
                entry.revision,
                "Readable offline label".into(),
            )
            .unwrap();
        assert_eq!(renamed.details(&entry.id).unwrap().manifest, original);
        assert_eq!(renamed.list()[0].revision, 3);
    }

    #[test]
    fn full_prepare_captures_exact_bytes_and_refuses_content_replacement() {
        let f = Fixture::new();
        let c = selected(&f);
        let r = review(&c);
        let first = c.prepare(&r, &context(), &control()).unwrap();
        assert_eq!(first.entry(), c.details(&c.list()[0].id).unwrap());
        let path = f.root.join("model.gguf");
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed[5] ^= 1;
        fs::write(&path, &changed).unwrap();
        assert!(matches!(
            c.prepare(&r, &context(), &control()),
            Err(Error::FileChanged)
        ));
        fs::write(&path, &original).unwrap();
        // Restoring the exact selected bytes is permitted, after hashing again;
        // a saved inode/mtime or historical successful prepare is never reused.
        assert!(c.prepare(&r, &context(), &control()).is_ok());
        fs::write(f.root.join("runner"), b"changed executable").unwrap();
        assert!(matches!(
            c.prepare(&r, &context(), &control()),
            Err(Error::FileChanged)
        ));
    }

    #[test]
    fn review_binds_session_inventory_config_selection_and_catalog_revision() {
        let f = Fixture::new();
        let c = selected(&f);
        let r = review(&c);
        for current in [
            PreparationContext {
                session_id: "other".into(),
                ..context()
            },
            PreparationContext {
                config_revision: "connection-revision-10".into(),
                ..context()
            },
            PreparationContext {
                inventory_generation: "other-device-driver-observation".into(),
                ..context()
            },
        ] {
            assert!(matches!(
                c.prepare(&r, &current, &control()),
                Err(Error::StaleReview)
            ));
        }
        let changed = c
            .insert(c.revision(), "another".into(), f.capture())
            .unwrap();
        assert!(matches!(
            changed.check_review(&r, &context()),
            Err(Error::StaleReview)
        ));
        let e = &c.list()[0];
        let changed = c
            .replace(
                c.revision(),
                &e.id,
                e.revision,
                "renamed".into(),
                f.capture(),
            )
            .unwrap();
        assert!(matches!(
            changed.check_review(&r, &context()),
            Err(Error::StaleReview)
        ));
        let removed = c.trash(c.revision(), &e.id, e.revision).unwrap();
        assert!(removed.record().selected_for_review.is_none());
        assert!(matches!(
            removed.check_review(&r, &context()),
            Err(Error::StaleReview)
        ));
    }

    #[test]
    fn cas_and_trash_restore_preserve_identity_without_implicit_selection() {
        let f = Fixture::new();
        let c = f.catalog();
        let old = c.list()[0].clone();
        assert!(matches!(
            c.insert(0, "stale".into(), f.capture()),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            c.select_for_review(1, &old.id, 0),
            Err(Error::Conflict)
        ));
        let bytes = c.encode().unwrap();
        let restored = Catalog::decode(&bytes).unwrap();
        let base = restored.storage_base_sha256().unwrap().to_owned();
        let deleted = restored.trash(1, &old.id, old.revision).unwrap();
        assert_eq!(deleted.storage_base_sha256(), Some(base.as_str()));
        assert!(deleted.list().is_empty());
        assert_eq!(deleted.list_trash().len(), 1);
        assert!(matches!(
            deleted.select_for_review(2, &old.id, 2),
            Err(Error::EntryTrashed)
        ));
        let reopened = Catalog::decode(&deleted.encode().unwrap()).unwrap();
        let restored = reopened.restore(2, &old.id, 2).unwrap();
        assert_eq!(restored.list()[0].id, old.id);
        assert!(!restored.list()[0].selected_for_review);
        assert!(f.root.join("model.gguf").exists());
        let replacement = deleted.insert(2, "new".into(), f.capture()).unwrap();
        assert_ne!(replacement.list()[0].id, old.id);
        assert_eq!(replacement.revision(), 3);
        assert!(matches!(
            replacement.review(&replacement.list()[0].id, 3, context()),
            Err(Error::WrongSelection)
        ));
    }

    #[test]
    fn trash_counts_toward_capacity_and_revision_exhaustion_never_wraps() {
        let f = Fixture::new();
        let original = f.catalog();
        let mut record = original.record().clone();
        let seed = record.entries[0].clone();
        record.revision = MAX_ENTRIES as u64;
        record.entries = (1..=MAX_ENTRIES as u64)
            .map(|revision| {
                let mut entry = seed.clone();
                entry.id = format!("local-{revision:016x}");
                entry.revision = revision;
                entry.trashed = true;
                entry
            })
            .collect();
        let full = Catalog::decode(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(full.list().is_empty());
        assert_eq!(full.list_trash().len(), MAX_ENTRIES);
        assert!(matches!(
            full.insert(full.revision(), "new".into(), f.capture()),
            Err(Error::LimitExceeded)
        ));
        assert_eq!(full.list_trash().len(), MAX_ENTRIES);
        record.entries.truncate(1);
        record.revision = MAX_REVISION;
        let exhausted = Catalog::decode(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(matches!(
            exhausted.restore(MAX_REVISION, &seed.id, 1),
            Err(Error::LimitExceeded)
        ));
        assert_eq!(exhausted.revision(), MAX_REVISION);
    }

    #[test]
    fn record_digest_and_adapter_metadata_cannot_be_silently_reinterpreted() {
        let f = Fixture::new();
        let c = f.catalog();
        let mut value = serde_json::to_value(c.record()).unwrap();
        value["entries"][0]["manifest"]["model"]["files"][0]["sha256"] =
            serde_json::json!("0".repeat(64));
        assert!(matches!(
            Catalog::decode(&serde_json::to_vec(&value).unwrap()),
            Err(Error::InvalidRecord)
        ));
        let mut request = f.request();
        request.description.format = ModelFormat::Mlx;
        assert!(capture(request, &control()).is_err());
        let mut request = f.request();
        request
            .description
            .declared_capabilities
            .push(DeclaredCapability::TextChat);
        assert!(capture(request, &control()).is_err());
        let mut request = f.request();
        if let AdapterHints::LlamaGguf(h) = &mut request.description.hints {
            h.threads = Some(0);
        }
        assert!(capture(request, &control()).is_err());
    }

    #[test]
    fn mlx_manifest_is_explicit_subset_and_each_selected_sidecar_is_revalidated() {
        let f = Fixture::new();
        let mut request = f.request();
        request.description = ModelDescription {
            format: ModelFormat::Mlx,
            architecture: "qwen3".into(),
            declared_capabilities: vec![
                DeclaredCapability::RawCompletion,
                DeclaredCapability::TextChat,
            ],
            declared_context_limit: Some(4096),
            hints: AdapterHints::MlxSwift(MlxHints {
                context_tokens: Some(1024),
                temperature_milli: Some(0),
                ..Default::default()
            }),
        };
        request.model.files.clear();
        for name in [
            "config.json",
            "tokenizer.json",
            "tokenizer_config.json",
            "model.safetensors",
        ] {
            fs::write(f.root.join(name), format!("synthetic {name}")).unwrap();
            request.model.files.push(SelectedFile {
                name: name.into(),
                role: if name.ends_with(".safetensors") {
                    FileRole::Weights
                } else {
                    FileRole::ModelMetadata
                },
            });
        }
        // This file is unselected and never enumerated or included in the hash.
        fs::write(
            f.root.join("unselected.bin"),
            b"not part of the captured subset",
        )
        .unwrap();
        let assets = capture(request, &control()).unwrap();
        assert_eq!(
            assets.manifest().model.completeness,
            Completeness::SelectedFilesOnly
        );
        let c = Catalog::empty()
            .insert(0, "MLX external selection".into(), assets)
            .unwrap();
        let e = &c.list()[0];
        let c = c.select_for_review(1, &e.id, 1).unwrap();
        let r = review(&c);
        assert!(c.prepare(&r, &context(), &control()).is_ok());
        fs::write(f.root.join("tokenizer.json"), b"changed tokenizer").unwrap();
        assert!(matches!(
            c.prepare(&r, &context(), &control()),
            Err(Error::FileChanged)
        ));
    }

    #[test]
    fn symlink_ancestors_final_links_hardlinks_and_fifo_are_refused() {
        let f = Fixture::new();
        let real = f.root.join("model.gguf");
        fs::rename(&real, f.root.join("real-model")).unwrap();
        symlink("real-model", &real).unwrap();
        assert!(matches!(
            capture(f.request(), &control()),
            Err(Error::UnsafeFile)
        ));
        fs::remove_file(&real).unwrap();
        fs::hard_link(f.root.join("real-model"), &real).unwrap();
        assert!(matches!(
            capture(f.request(), &control()),
            Err(Error::UnsafeFile)
        ));
        fs::remove_file(&real).unwrap();
        let name = std::ffi::CString::new(real.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            capture(f.request(), &control()),
            Err(Error::UnsafeFile)
        ));
        let alias = f.root.join("alias");
        symlink(&f.root, &alias).unwrap();
        let mut request = f.request();
        request.runtime.root = alias;
        assert!(matches!(
            capture(request, &control()),
            Err(Error::UnsafeFile)
        ));
    }

    #[test]
    fn path_aliases_duplicates_and_large_sparse_files_fail_before_hashing() {
        let f = Fixture::new();
        for name in [
            "../model.gguf",
            "./model.gguf",
            "sub//model.gguf",
            "sub/../model.gguf",
            "/model.gguf",
            "C:\\model.gguf",
        ] {
            let mut r = f.request();
            r.model.files[0].name = name.into();
            assert!(capture(r, &control()).is_err(), "{name}");
        }
        for suffix in ["/.", "//", "/child/.."] {
            let mut r = f.request();
            r.runtime.root = PathBuf::from(format!("{}{suffix}", f.root.display()));
            assert!(capture(r, &control()).is_err());
        }
        let mut r = f.request();
        r.runtime.files.push(r.runtime.files[0].clone());
        assert!(capture(r, &control()).is_err());
        fs::OpenOptions::new()
            .write(true)
            .open(f.root.join("model.gguf"))
            .unwrap()
            .set_len(MAX_SELECTED_BYTES + 1)
            .unwrap();
        assert!(matches!(
            capture(f.request(), &control()),
            Err(Error::LimitExceeded)
        ));
    }

    #[test]
    fn cancellation_and_expired_deadline_cannot_publish_a_capture() {
        let f = Fixture::new();
        let stop = control();
        stop.cancel();
        assert!(matches!(capture(f.request(), &stop), Err(Error::Cancelled)));
        let expired = ReadControl::new(Duration::from_nanos(1)).unwrap();
        assert!(matches!(
            capture(f.request(), &expired),
            Err(Error::Deadline)
        ));
    }

    #[test]
    fn serialized_selection_details_are_passive_and_cannot_restore_review_authority() {
        let f = Fixture::new();
        let c = selected(&f);
        let r = review(&c);
        let summary = serde_json::to_value(r.summary()).unwrap();
        assert_eq!(summary["context"]["session_id"], "fresh-profile-session");
        assert!(summary.get("approved").is_none());
        let list = serde_json::to_string(&c.list()).unwrap();
        assert!(!list.contains(f.root.to_str().unwrap()));
        assert!(!list.contains("sha256"));
        let mut stale_context = context();
        stale_context.session_id = "restarted-profile-session".into();
        let restored = Catalog::decode(&c.encode().unwrap()).unwrap();
        assert!(matches!(
            restored.prepare(&r, &stale_context, &control()),
            Err(Error::StaleReview)
        ));
        assert!(Path::new(&f.root).join("runner").exists());
    }
}
