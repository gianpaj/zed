use crate::embedding::EmbeddingProvider;
use crate::embedding_index::EmbeddingIndex;
use crate::indexing::IndexingEntrySet;
use crate::summary_index::SummaryIndex;
use anyhow::Result;
use fs::Fs;
use futures::future::Shared;
use gpui::{
    AppContext, AsyncAppContext, Context, Model, ModelContext, Subscription, Task, WeakModel,
};
use language::LanguageRegistry;
use log;
use project::{UpdatedEntriesSet, Worktree};
use smol::channel;
use std::sync::Arc;
use util::ResultExt;

#[derive(Clone)]
pub enum WorktreeIndexHandle {
    Loading {
        index: Shared<Task<Result<Model<WorktreeIndex>, Arc<anyhow::Error>>>>,
    },
    Loaded {
        index: Model<WorktreeIndex>,
    },
}

pub struct WorktreeIndex {
    worktree: Model<Worktree>,
    db_connection: heed::Env,
    embedding_index: EmbeddingIndex,
    summary_index: SummaryIndex,
    entry_ids_being_indexed: Arc<IndexingEntrySet>,
    _index_entries: Task<Result<()>>,
    _subscription: Subscription,
}

impl WorktreeIndex {
    pub fn load(
        worktree: Model<Worktree>,
        db_connection: heed::Env,
        language_registry: Arc<LanguageRegistry>,
        fs: Arc<dyn Fs>,
        status_tx: channel::Sender<()>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        cx: &mut AppContext,
    ) -> Task<Result<Model<Self>>> {
        let worktree_for_index = worktree.clone();
        let worktree_for_summary = worktree.clone();
        let worktree_abs_path = worktree.read(cx).abs_path();
        let embedding_fs = Arc::clone(&fs);
        let summary_fs = fs;
        cx.spawn(|mut cx| async move {
            let entries_being_indexed = Arc::new(IndexingEntrySet::new(status_tx));
            let (embedding_index, summary_index) = cx
                .background_executor()
                .spawn({
                    let entries_being_indexed = Arc::clone(&entries_being_indexed);
                    let db_connection = db_connection.clone();
                    async move {
                        let mut txn = db_connection.write_txn()?;
                        let embedding_index = {
                            let db_name = worktree_abs_path.to_string_lossy();
                            let db = db_connection.create_database(&mut txn, Some(&db_name))?;

                            EmbeddingIndex::new(
                                worktree_for_index,
                                embedding_fs,
                                db_connection.clone(),
                                db,
                                language_registry,
                                embedding_provider,
                                Arc::clone(&entries_being_indexed),
                            )
                        };
                        let summary_index = {
                            let file_digest_db = {
                                let db_name =
                                // Prepend something that wouldn't be found at the beginning of an
                                // absolute path, so we don't get db key namespace conflicts with
                                // embeddings, which use the abs path as a key.
                                format!("digests-{}", worktree_abs_path.to_string_lossy());
                                db_connection.create_database(&mut txn, Some(&db_name))?
                            };
                            let summary_db = {
                                let db_name =
                                // Prepend something that wouldn't be found at the beginning of an
                                // absolute path, so we don't get db key namespace conflicts with
                                // embeddings, which use the abs path as a key.
                                format!("summaries-{}", worktree_abs_path.to_string_lossy());
                                db_connection.create_database(&mut txn, Some(&db_name))?
                            };
                            SummaryIndex::new(
                                worktree_for_summary,
                                summary_fs,
                                db_connection.clone(),
                                file_digest_db,
                                summary_db,
                                Arc::clone(&entries_being_indexed),
                            )
                        };
                        txn.commit()?;
                        anyhow::Ok((embedding_index, summary_index))
                    }
                })
                .await?;

            cx.new_model(|cx| {
                Self::new(
                    worktree,
                    db_connection,
                    embedding_index,
                    summary_index,
                    entries_being_indexed,
                    cx,
                )
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worktree: Model<Worktree>,
        db_connection: heed::Env,
        embedding_index: EmbeddingIndex,
        summary_index: SummaryIndex,
        entry_ids_being_indexed: Arc<IndexingEntrySet>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        let (updated_entries_tx, updated_entries_rx) = channel::unbounded();
        let _subscription = cx.subscribe(&worktree, move |_this, _worktree, event, _cx| {
            if let worktree::Event::UpdatedEntries(update) = event {
                dbg!(&update);
                log::debug!("Updating entries...");
                _ = updated_entries_tx.try_send(update.clone());
            } else {
                dbg!("non-update event");
            }
        });

        Self {
            db_connection,
            embedding_index,
            summary_index,
            worktree,
            entry_ids_being_indexed,
            _index_entries: cx.spawn(|this, cx| Self::index_entries(this, updated_entries_rx, cx)),
            _subscription,
        }
    }

    pub fn entry_ids_being_indexed(&self) -> &IndexingEntrySet {
        self.entry_ids_being_indexed.as_ref()
    }

    pub fn worktree(&self) -> &Model<Worktree> {
        &self.worktree
    }

    pub fn db_connection(&self) -> &heed::Env {
        &self.db_connection
    }

    pub fn embedding_index(&self) -> &EmbeddingIndex {
        &self.embedding_index
    }

    pub fn summary_index(&self) -> &SummaryIndex {
        &self.summary_index
    }

    async fn index_entries(
        this: WeakModel<Self>,
        updated_entries: channel::Receiver<UpdatedEntriesSet>,
        mut cx: AsyncAppContext,
    ) -> Result<()> {
        let index = this.update(&mut cx, |this, cx| {
            futures::future::try_join(
                this.embedding_index.index_entries_changed_on_disk(cx),
                this.summary_index.index_entries_changed_on_disk(cx),
            )
        })?;
        index.await.log_err();

        while let Ok(updated_entries) = updated_entries.recv().await {
            let index = this.update(&mut cx, |this, cx| {
                futures::future::try_join(
                    this.embedding_index
                        .index_updated_entries(updated_entries.clone(), cx),
                    this.summary_index
                        .index_updated_entries(updated_entries, cx),
                )
            })?;
            index.await.log_err();
        }

        Ok(())
    }

    #[cfg(test)]
    pub fn path_count(&self) -> Result<u64> {
        use anyhow::Context;

        let txn = self
            .db_connection
            .read_txn()
            .context("failed to create read transaction")?;
        Ok(self.embedding_index().db().len(&txn)?)
    }

    // fn index_updated_entries(
    //     &self,
    //     updated_entries: UpdatedEntriesSet,
    //     cx: &AppContext,
    // ) -> impl Future<Output = Result<()>> {
    //     log::debug!("index_updated_entries({:?})", &updated_entries);
    //     let worktree = self.worktree.read(cx).snapshot();
    //     let worktree_abs_path = worktree.abs_path().clone();
    //     let scan = self.scan_updated_entries(worktree, updated_entries.clone(), cx);
    //     let processed = self.process_files(worktree_abs_path, scan.updated_entries, cx);
    //     let embed = Self::embed_files(self.embedding_provider.clone(), processed.chunked_files, cx);
    //     let might_need_summary = self.check_summary_cache(processed.might_need_summary, cx);
    //     let summarized = self.summarize_files(might_need_summary.files, cx);
    //     let persist_summaries = self.persist_summaries(summarized.files, cx);
    //     let persist_embeds = self.persist_embeddings(scan.deleted_entry_ranges, embed.files, cx);
    //     async move {
    //         futures::try_join!(
    //             scan.task,
    //             processed.task,
    //             embed.task,
    //             might_need_summary.task,
    //             summarized.task,
    //             persist_embeds,
    //             persist_summaries
    //         )?;
    //         Ok(())
    //     }
    // }

    // fn scan_updated_entries(
    //     &self,
    //     worktree: Snapshot,
    //     updated_entries: UpdatedEntriesSet,
    //     cx: &AppContext,
    // ) -> ScanEntries {
    //     let (updated_entries_tx, updated_entries_rx) = channel::bounded(512);
    //     let (deleted_entry_ranges_tx, deleted_entry_ranges_rx) = channel::bounded(128);
    //     let entries_being_indexed = self.entry_ids_being_indexed.clone();
    //     let task = cx.background_executor().spawn(async move {
    //         for (path, entry_id, status) in updated_entries.iter() {
    //             match status {
    //                 project::PathChange::Added
    //                 | project::PathChange::Updated
    //                 | project::PathChange::AddedOrUpdated => {
    //                     if let Some(entry) = worktree.entry_for_id(*entry_id) {
    //                         if entry.is_file() {
    //                             let handle = entries_being_indexed.insert(entry.id);
    //                             updated_entries_tx.send((entry.clone(), handle)).await?;
    //                         }
    //                     }
    //                 }
    //                 project::PathChange::Removed => {
    //                     let db_path = db_key_for_path(path);
    //                     deleted_entry_ranges_tx
    //                         .send((Bound::Included(db_path.clone()), Bound::Included(db_path)))
    //                         .await?;
    //                 }
    //                 project::PathChange::Loaded => {
    //                     // Do nothing.
    //                 }
    //             }
    //         }

    //         Ok(())
    //     });

    //     ScanEntries {
    //         updated_entries: updated_entries_rx,
    //         deleted_entry_ranges: deleted_entry_ranges_rx,
    //         task,
    //     }
    // }
}
