/*
Copyright 2024 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use super::refresh_task::RefreshTask;
use futures::future::BoxFuture;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    task::JoinHandle,
};

use std::sync::Arc;
use tokio::sync::RwLock;

use datafusion::{datasource::TableProvider, sql::TableReference};

use super::refresh::Refresh;

pub struct RefreshTaskRunner {
    dataset_name: TableReference,
    federated: Arc<dyn TableProvider>,
    refresh: Arc<RwLock<Refresh>>,
    accelerator: Arc<dyn TableProvider>,
    task: Option<JoinHandle<()>>,
}

impl RefreshTaskRunner {
    #[must_use]
    pub fn new(
        dataset_name: TableReference,
        federated: Arc<dyn TableProvider>,
        refresh: Arc<RwLock<Refresh>>,
        accelerator: Arc<dyn TableProvider>,
    ) -> Self {
        Self {
            dataset_name,
            federated,
            refresh,
            accelerator,
            task: None,
        }
    }

    pub fn start(&mut self) -> (Sender<()>, Receiver<super::Result<()>>) {
        assert!(self.task.is_none());

        let (start_refresh, mut on_start_refresh) = mpsc::channel::<()>(1);

        let (notify_refresh_complete, on_refresh_complete) = mpsc::channel::<super::Result<()>>(1);

        let dataset_name = self.dataset_name.clone();
        let notify_refresh_complete = Arc::new(notify_refresh_complete);

        let refresh_task = Arc::new(RefreshTask::new(
            dataset_name.clone(),
            Arc::clone(&self.federated),
            Arc::clone(&self.refresh),
            Arc::clone(&self.accelerator),
        ));

        self.task = Some(tokio::spawn(async move {
            let mut task_completion: Option<BoxFuture<super::Result<()>>> = None;

            loop {
                if let Some(task) = task_completion.take() {
                    select! {
                        res = task => {
                            match res {
                                Ok(()) => {
                                    tracing::debug!("Refresh task successfully completed for dataset {dataset_name}");
                                    if let Err(err) = notify_refresh_complete.send(Ok(())).await {
                                        tracing::debug!("Failed to send refresh task completion for dataset {dataset_name}: {err}");
                                    }
                                },
                                Err(err) => {
                                    tracing::debug!("Refresh task for dataset {dataset_name} failed with error: {err}");
                                    if let Err(err) = notify_refresh_complete.send(Err(err)).await {
                                        tracing::debug!("Failed to send refresh task completion for dataset {dataset_name}: {err}");
                                    }
                                }
                            }
                        },
                        _ = on_start_refresh.recv() => {
                            task_completion = Some(Box::pin(refresh_task.run()));
                        }
                    }
                } else {
                    select! {
                        _ = on_start_refresh.recv() => {
                            task_completion = Some(Box::pin(refresh_task.run()));
                        }
                    }
                }
            }
        }));

        (start_refresh, on_refresh_complete)
    }

    pub fn abort(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
            self.task = None;
        }
    }
}

impl Drop for RefreshTaskRunner {
    fn drop(&mut self) {
        self.abort();
    }
}
