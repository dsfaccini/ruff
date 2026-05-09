use crate::document::DocumentKey;
use crate::server::Result;
use crate::server::api::diagnostics::{
    publish_diagnostics_if_needed, publish_settings_diagnostics,
};
use crate::server::api::traits::{NotificationHandler, SyncNotificationHandler};
use crate::session::Session;
use crate::session::client::Client;
use crate::system::AnySystemPath;
use lsp_types as types;
use lsp_types::{FileChangeType, notification as notif};
use ruff_db::Db as _;
use ruff_db::system::{System, SystemPath};
use ty_project::Db as _;
use ty_project::watch::{ChangeEvent, ChangedKind, CreatedKind, DeletedKind, ExistingPathKind};

pub(crate) struct DidChangeWatchedFiles;

impl NotificationHandler for DidChangeWatchedFiles {
    type NotificationType = notif::DidChangeWatchedFiles;
}

impl SyncNotificationHandler for DidChangeWatchedFiles {
    fn run(
        session: &mut Session,
        client: &Client,
        params: types::DidChangeWatchedFilesParams,
    ) -> Result<()> {
        let mut file_changes = Vec::new();

        for change in params.changes {
            let path = DocumentKey::from_url(&change.uri).into_file_path();

            let system_path = match path {
                AnySystemPath::System(system) => system,
                AnySystemPath::SystemVirtual(path) => {
                    tracing::debug!("Ignoring virtual path from change event: `{path}`");
                    continue;
                }
            };

            match change.typ {
                FileChangeType::CREATED | FileChangeType::CHANGED | FileChangeType::DELETED => {
                    file_changes.push((system_path, change.typ));
                }
                _ => {
                    tracing::debug!(
                        "Ignoring unsupported change event type: `{:?}` for {system_path}",
                        change.typ
                    );
                }
            }
        }

        if file_changes.is_empty() {
            return Ok(());
        }

        let changes_by_root: Vec<_> = session
            .project_dbs()
            .filter_map(|db| {
                let root = db.project().root(db).to_owned();
                let changes = file_changes
                    .iter()
                    .filter_map(|(path, typ)| to_change_event(path, *typ, db.system()))
                    .collect::<Vec<_>>();

                (!changes.is_empty()).then_some((root, changes))
            })
            .collect();

        if changes_by_root.is_empty() {
            return Ok(());
        }

        for (root, changes) in changes_by_root {
            tracing::debug!("Applying changes to `{root}`");

            session.apply_changes(&AnySystemPath::System(root.clone()), &changes);
            publish_settings_diagnostics(session, client, root);
        }

        let client_capabilities = session.client_capabilities();

        if client_capabilities.supports_workspace_diagnostic_refresh() {
            client.send_request::<types::request::WorkspaceDiagnosticRefresh>(
                session,
                (),
                |_, ()| {},
            );
        } else {
            for key in session.text_document_handles() {
                publish_diagnostics_if_needed(&key, session, client);
            }
        }

        if client_capabilities.supports_inlay_hint_refresh() {
            client.send_request::<types::request::InlayHintRefreshRequest>(session, (), |_, ()| {});
        }

        Ok(())
    }
}

fn to_change_event(
    path: &SystemPath,
    typ: FileChangeType,
    system: &dyn System,
) -> Option<ChangeEvent> {
    match typ {
        FileChangeType::CREATED => Some(ChangeEvent::Created {
            path: path.to_path_buf(),
            kind: CreatedKind::from(ExistingPathKind::from_system(system, path)),
        }),
        FileChangeType::CHANGED => {
            ExistingPathKind::from_system(system, path)
                .is_file()
                .then(|| ChangeEvent::Changed {
                    path: path.to_path_buf(),
                    kind: ChangedKind::Any,
                })
        }
        FileChangeType::DELETED => Some(ChangeEvent::Deleted {
            path: path.to_path_buf(),
            kind: DeletedKind::Any,
        }),
        _ => None,
    }
}
