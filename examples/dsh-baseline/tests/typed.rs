//! Published dsh plugins used from rutis through generated, typed bindings.
#![cfg(all(unix, dsh_baseline))]

use dsh_baseline::{commands, credentials, fs, invariants, jobs, workspace};
use rutis::{Ctx, FiberView, Plugin};
use rutis_interop::Error;

async fn mount(ctx: &Ctx, plugin: impl Plugin + 'static) -> FiberView {
    let view = ctx.plugin(plugin);
    (&view).await.unwrap();
    view
}

#[tokio::test(flavor = "multi_thread")]
async fn published_plugins_through_typed_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let (home, work) = (dir.path().join("home"), dir.path().join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.txt"), "hello").unwrap();
    let (home, work) = (home.to_str().unwrap(), work.to_str().unwrap());
    let ctx = Ctx::root().unwrap();

    // L0: every plugin mounts; services appear as ordinary rutis services.
    mount(&ctx, invariants::Plugin::new(Default::default())).await;
    mount(
        &ctx,
        credentials::Plugin::new(credentials::Config {
            dsh_home: Some(home.into()),
            watch: Some(false),
            ..Default::default()
        }),
    )
    .await;
    mount(
        &ctx,
        fs::Plugin::new(fs::Config {
            cwd: Some(work.into()),
            ..Default::default()
        }),
    )
    .await;
    mount(&ctx, jobs::Plugin::new(Default::default())).await;
    mount(&ctx, commands::Plugin::new(Default::default())).await;
    // dsh-workspace mounted alone cannot resolve its storage dependencies.
    // Mounted as a group with its providers, they resolve inside Cordis.
    mount(
        &ctx,
        workspace::Plugin::new(workspace::Config {
            storage_json: workspace::StorageJsonConfig {
                root: format!("{home}/kv"),
            },
            storage_domain: workspace::StorageDomainConfig {
                backend: "json".into(),
                routes: None,
            },
            sessions: workspace::SessionsConfig {
                root: format!("{home}/sessions"),
                compression: None,
            },
            storage: Default::default(),
            workspace: Default::default(),
        }),
    )
    .await;
    assert!(ctx.get::<invariants::InvariantRegistry>().is_some());
    assert!(ctx.get::<commands::CommandRuntime>().is_some());
    let registry = ctx.get::<workspace::WorkspaceRegistry>().unwrap();
    let sessions = ctx.get::<workspace::SessionPersistence>().unwrap();
    assert!(sessions.list(None).await.unwrap().is_empty());
    assert!(!registry
        .delete(&workspace::WorkspaceId::from("missing"))
        .await
        .unwrap());

    // Workspaces are live objects: returned by reference, read live, and the
    // same object whichever call returns it.
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let created = registry
        .create(project.to_str().unwrap(), Some("demo"))
        .await
        .unwrap();
    let id = created.id().unwrap();
    assert_eq!(created.title().unwrap(), "demo");
    assert_eq!(
        created.path().unwrap(),
        project.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(created.session_ids().unwrap().is_empty());
    assert_eq!(registry.get(&id).unwrap().as_ref(), Some(&created));
    assert_eq!(registry.list().unwrap(), vec![created.clone()]);
    assert_eq!(
        registry
            .resolve_by_path(project.to_str().unwrap())
            .await
            .unwrap()
            .as_ref(),
        Some(&created)
    );
    assert!(registry.delete(&id).await.unwrap());
    assert!(registry.get(&id).unwrap().is_none());
    drop((registry, sessions, created));

    // L1: typed calls.
    let credentials = ctx.get::<credentials::CredentialProvider>().unwrap();
    let key = credentials::CredentialRef::from("test/api-key");
    assert!(!credentials.describe(&key).await.unwrap().configured);
    credentials.set(&key, "s3cret").await.unwrap();
    let info = credentials.describe(&key).await.unwrap();
    assert!(info.configured && info.writable);
    assert_eq!(info.source.as_deref(), Some("file"));
    let resolved = credentials.resolve(&key).await.unwrap().unwrap();
    assert_eq!(
        (resolved.value.as_str(), resolved.source.as_str()),
        ("s3cret", "file")
    );
    assert!(credentials.list_records().await.unwrap().is_empty());
    credentials.unset(&key).await.unwrap();
    assert!(credentials.resolve(&key).await.unwrap().is_none());

    let files = ctx.get::<fs::FileSystem>().unwrap();
    let file = files.resolve("a.txt", None).await.unwrap();
    let root = files.resolve(".", None).await.unwrap();
    assert_eq!(files.read_text(&file).await.unwrap(), "hello");
    let entries = files.list_dir(&root).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a.txt");
    assert_eq!(entries[0].r#type, fs::FsInfoType::File);
    assert_eq!(entries[0].target, file);
    assert!(files.contains(&root, &file).unwrap());
    let written = files.write_text(&file, "world", None, None).await.unwrap();
    assert_eq!(written.operation, fs::FsWriteOutcomeOperation::Update);
    assert_eq!(files.read_text(&file).await.unwrap(), "world");
    let missing = files.resolve("missing.txt", None).await.unwrap();
    assert!(matches!(
        files.read_text(&missing).await,
        Err(Error::Remote { ref name, .. }) if name == "FsError"
    ));

    let registry = ctx.get::<jobs::JobRegistry>().unwrap();
    assert!(registry.list(None).unwrap().is_empty());

    drop((credentials, files, registry));
    ctx.shutdown().await.unwrap();
}
