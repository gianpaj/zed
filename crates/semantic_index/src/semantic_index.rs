mod chunking;
mod embedding;
mod embedding_index;
mod indexing;
mod project_index;
mod project_index_debug_view;
mod summary_backlog;
mod summary_index;
mod worktree_index;

use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{AppContext, AsyncAppContext, BorrowAppContext, Context, Global, Model, WeakModel};
use project::Project;
use project_index::ProjectIndex;
use std::{path::PathBuf, sync::Arc};
use ui::ViewContext;
use workspace::Workspace;

pub use embedding::*;
pub use project_index_debug_view::ProjectIndexDebugView;
pub use summary_index::FileSummary;

pub struct SemanticDb {
    embedding_provider: Arc<dyn EmbeddingProvider>,
    db_connection: heed::Env,
    project_indices: HashMap<WeakModel<Project>, Model<ProjectIndex>>,
}

impl Global for SemanticDb {}

impl SemanticDb {
    pub async fn new(
        db_path: PathBuf,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        cx: &mut AsyncAppContext,
    ) -> Result<Self> {
        let db_connection = cx
            .background_executor()
            .spawn(async move {
                std::fs::create_dir_all(&db_path)?;
                unsafe {
                    heed::EnvOpenOptions::new()
                        .map_size(1024 * 1024 * 1024)
                        .max_dbs(3000)
                        .open(db_path)
                }
            })
            .await
            .context("opening database connection")?;

        cx.update(|cx| {
            cx.observe_new_views(
                |workspace: &mut Workspace, cx: &mut ViewContext<Workspace>| {
                    let project = workspace.project().clone();

                    if cx.has_global::<SemanticDb>() {
                        cx.update_global::<SemanticDb, _>(|this, cx| {
                            let project_index = cx.new_model(|cx| {
                                ProjectIndex::new(
                                    project.clone(),
                                    this.db_connection.clone(),
                                    this.embedding_provider.clone(),
                                    cx,
                                )
                            });

                            let project_weak = project.downgrade();
                            this.project_indices
                                .insert(project_weak.clone(), project_index);

                            cx.on_release(move |_, _, cx| {
                                if cx.has_global::<SemanticDb>() {
                                    cx.update_global::<SemanticDb, _>(|this, _| {
                                        this.project_indices.remove(&project_weak);
                                    })
                                }
                            })
                            .detach();
                        })
                    } else {
                        log::info!("No SemanticDb, skipping project index")
                    }
                },
            )
            .detach();
        })
        .ok();

        Ok(SemanticDb {
            db_connection,
            embedding_provider,
            project_indices: HashMap::default(),
        })
    }

    pub fn project_index(
        &mut self,
        project: Model<Project>,
        _cx: &mut AppContext,
    ) -> Option<Model<ProjectIndex>> {
        self.project_indices.get(&project.downgrade()).cloned()
    }

    pub fn remaining_summaries(
        &self,
        project: &WeakModel<Project>,
        cx: &mut AppContext,
    ) -> Option<usize> {
        self.project_indices.get(project).map(|project_index| {
            project_index.update(cx, |project_index, cx| {
                project_index.remaining_summaries(cx)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use chunking::Chunk;
    use embedding_index::{ChunkedFile, EmbeddingIndex};
    use futures::{future::BoxFuture, FutureExt};
    use gpui::TestAppContext;
    use indexing::IndexingEntrySet;
    use language::language_settings::AllLanguageSettings;
    use project::{Project, ProjectEntryId};
    use settings::SettingsStore;
    use smol::{channel, stream::StreamExt};
    use std::{future, path::Path, sync::Arc};

    fn init_test(cx: &mut TestAppContext) {
        env_logger::init();

        _ = cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            language::init(cx);
            Project::init_settings(cx);
            SettingsStore::update(cx, |store, cx| {
                store.update_user_settings::<AllLanguageSettings>(cx, |_| {});
            });
        });
    }

    pub struct TestEmbeddingProvider {
        batch_size: usize,
        compute_embedding: Box<dyn Fn(&str) -> Result<Embedding> + Send + Sync>,
    }

    impl TestEmbeddingProvider {
        pub fn new(
            batch_size: usize,
            compute_embedding: impl 'static + Fn(&str) -> Result<Embedding> + Send + Sync,
        ) -> Self {
            return Self {
                batch_size,
                compute_embedding: Box::new(compute_embedding),
            };
        }
    }

    impl EmbeddingProvider for TestEmbeddingProvider {
        fn embed<'a>(
            &'a self,
            texts: &'a [TextToEmbed<'a>],
        ) -> BoxFuture<'a, Result<Vec<Embedding>>> {
            let embeddings = texts
                .iter()
                .map(|to_embed| (self.compute_embedding)(to_embed.text))
                .collect();
            future::ready(embeddings).boxed()
        }

        fn batch_size(&self) -> usize {
            self.batch_size
        }
    }

    #[gpui::test]
    async fn test_search(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        init_test(cx);

        let temp_dir = tempfile::tempdir().unwrap();

        let mut semantic_index = SemanticDb::new(
            temp_dir.path().into(),
            Arc::new(TestEmbeddingProvider::new(16, |text| {
                let mut embedding = vec![0f32; 2];
                // if the text contains garbage, give it a 1 in the first dimension
                if text.contains("garbage in") {
                    embedding[0] = 0.9;
                } else {
                    embedding[0] = -0.9;
                }

                if text.contains("garbage out") {
                    embedding[1] = 0.9;
                } else {
                    embedding[1] = -0.9;
                }

                Ok(Embedding::new(embedding))
            })),
            &mut cx.to_async(),
        )
        .await
        .unwrap();

        let project_path = Path::new("./fixture");

        let project = cx
            .spawn(|mut cx| async move { Project::example([project_path], &mut cx).await })
            .await;

        cx.update(|cx| {
            let language_registry = project.read(cx).languages().clone();
            let node_runtime = project.read(cx).node_runtime().unwrap().clone();
            languages::init(language_registry, node_runtime, cx);

            // Manually create and insert the ProjectIndex
            let project_index = cx.new_model(|cx| {
                ProjectIndex::new(
                    project.clone(),
                    semantic_index.db_connection.clone(),
                    semantic_index.embedding_provider.clone(),
                    cx,
                )
            });
            semantic_index
                .project_indices
                .insert(project.downgrade(), project_index);
        });

        let project_index = cx
            .update(|_cx| {
                semantic_index
                    .project_indices
                    .get(&project.downgrade())
                    .cloned()
            })
            .unwrap();

        while cx
            .update(|cx| semantic_index.remaining_summaries(&project.downgrade(), cx))
            .unwrap()
            > 0
        {
            cx.run_until_parked();
        }

        let results = cx
            .update(|cx| {
                let project_index = project_index.read(cx);
                let query = "garbage in, garbage out";
                project_index.search(query.into(), 4, cx)
            })
            .await
            .unwrap();

        assert!(
            results.len() > 1,
            "should have found some results, but only found {:?}",
            results
        );

        for result in &results {
            println!("result: {:?}", result.path);
            println!("score: {:?}", result.score);
        }

        // Find result that is greater than 0.5
        let search_result = results.iter().find(|result| result.score > 0.9).unwrap();

        assert_eq!(search_result.path.to_string_lossy(), "needle.md");

        let content = cx
            .update(|cx| {
                let worktree = search_result.worktree.read(cx);
                let entry_abs_path = worktree.abs_path().join(&search_result.path);
                let fs = project.read(cx).fs().clone();
                cx.background_executor()
                    .spawn(async move { fs.load(&entry_abs_path).await.unwrap() })
            })
            .await;

        let range = search_result.range.clone();
        let content = content[range.clone()].to_owned();

        assert!(content.contains("garbage in, garbage out"));
    }

    #[gpui::test]
    async fn test_embed_files(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let provider = Arc::new(TestEmbeddingProvider::new(3, |text| {
            if text.contains('g') {
                Err(anyhow!("cannot embed text containing a 'g' character"))
            } else {
                Ok(Embedding::new(
                    ('a'..='z')
                        .map(|char| text.chars().filter(|c| *c == char).count() as f32)
                        .collect(),
                ))
            }
        }));

        let (indexing_progress_tx, _) = channel::unbounded();
        let indexing_entries = Arc::new(IndexingEntrySet::new(indexing_progress_tx));

        let (chunked_files_tx, chunked_files_rx) = channel::unbounded::<ChunkedFile>();
        chunked_files_tx
            .send_blocking(ChunkedFile {
                path: Path::new("test1.md").into(),
                mtime: None,
                handle: indexing_entries.insert(ProjectEntryId::from_proto(0)),
                text: "abcdefghijklmnop".to_string(),
                chunks: [0..4, 4..8, 8..12, 12..16]
                    .into_iter()
                    .map(|range| Chunk {
                        range,
                        digest: Default::default(),
                    })
                    .collect(),
            })
            .unwrap();
        chunked_files_tx
            .send_blocking(ChunkedFile {
                path: Path::new("test2.md").into(),
                mtime: None,
                handle: indexing_entries.insert(ProjectEntryId::from_proto(1)),
                text: "qrstuvwxyz".to_string(),
                chunks: [0..4, 4..8, 8..10]
                    .into_iter()
                    .map(|range| Chunk {
                        range,
                        digest: Default::default(),
                    })
                    .collect(),
            })
            .unwrap();
        chunked_files_tx.close();

        let embed_files_task =
            cx.update(|cx| EmbeddingIndex::embed_files(provider.clone(), chunked_files_rx, cx));
        embed_files_task.task.await.unwrap();

        let mut embedded_files_rx = embed_files_task.files;
        let mut embedded_files = Vec::new();
        while let Some((embedded_file, _)) = embedded_files_rx.next().await {
            embedded_files.push(embedded_file);
        }

        assert_eq!(embedded_files.len(), 1);
        assert_eq!(embedded_files[0].path.as_ref(), Path::new("test2.md"));
        assert_eq!(
            embedded_files[0]
                .chunks
                .iter()
                .map(|embedded_chunk| { embedded_chunk.embedding.clone() })
                .collect::<Vec<Embedding>>(),
            vec![
                (provider.compute_embedding)("qrst").unwrap(),
                (provider.compute_embedding)("uvwx").unwrap(),
                (provider.compute_embedding)("yz").unwrap(),
            ],
        );
    }
}
