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

use std::sync::Arc;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::accelerated_table::refresh_task::RefreshTask;
use crate::component::dataset::acceleration::RefreshMode;
use crate::component::dataset::TimeFormat;
use arrow::datatypes::Schema;
use cache::QueryResultsCacheProvider;
use data_components::cdc::ChangesStream;
use datafusion::common::TableReference;
use datafusion::datasource::TableProvider;
use futures::future::BoxFuture;
use opentelemetry::Key;
use rand::Rng;
use snafu::prelude::*;
use tokio::select;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;
use tokio::sync::RwLock;
use tokio::time::sleep;

use super::metrics;
use super::refresh_task_runner::RefreshTaskRunner;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(r#"time_column '{time_column}' in dataset {table_name} has data type '{actual_time_format}', but time_format is configured as '{expected_time_format}'"#))]
    TimeFormatMismatch {
        table_name: String,
        time_column: String,
        expected_time_format: String,
        actual_time_format: String,
    },

    #[snafu(display(r#"time_column '{time_column}' was not found in dataset {table_name}"#))]
    NoTimeColumnFound {
        table_name: String,
        time_column: String,
    },
}

#[derive(Clone, Debug)]
pub struct Refresh {
    pub(crate) time_column: Option<String>,
    pub(crate) time_format: Option<TimeFormat>,
    pub(crate) check_interval: Option<Duration>,
    pub(crate) max_jitter: Option<Duration>,
    pub(crate) sql: Option<String>,
    pub(crate) mode: RefreshMode,
    pub(crate) period: Option<Duration>,
    pub(crate) append_overlap: Option<Duration>,
    pub(crate) refresh_retry_enabled: bool,
    pub(crate) refresh_retry_max_attempts: Option<usize>,
}

impl Refresh {
    #[allow(clippy::needless_pass_by_value)]
    #[must_use]
    pub fn new(mode: RefreshMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn time_column(mut self, time_column: String) -> Self {
        self.time_column = Some(time_column);
        self
    }

    #[must_use]
    pub fn time_format(mut self, time_format: TimeFormat) -> Self {
        self.time_format = Some(time_format);
        self
    }

    #[must_use]
    pub fn check_interval(mut self, check_interval: Duration) -> Self {
        self.check_interval = Some(check_interval);
        self
    }

    #[must_use]
    pub fn max_jitter(mut self, max_jitter: Duration) -> Self {
        self.max_jitter = Some(max_jitter);
        self
    }

    #[must_use]
    pub fn sql(mut self, sql: String) -> Self {
        self.sql = Some(sql);
        self
    }

    #[must_use]
    pub fn period(mut self, period: Duration) -> Self {
        self.period = Some(period);
        self
    }

    #[must_use]
    pub fn append_overlap(mut self, append_overlap: Duration) -> Self {
        self.append_overlap = Some(append_overlap);
        self
    }

    #[must_use]
    pub fn with_retry(mut self, enabled: bool, max_attempts: Option<usize>) -> Self {
        self.refresh_retry_enabled = enabled;
        self.refresh_retry_max_attempts = max_attempts;
        self
    }

    pub(crate) fn validate_time_format(
        &self,
        dataset_name: String,
        schema: &Arc<Schema>,
    ) -> Result<(), Error> {
        let Some(time_column) = self.time_column.clone() else {
            return Ok(());
        };

        let Some((_, field)) = schema.column_with_name(&time_column) else {
            return Err(Error::NoTimeColumnFound {
                table_name: dataset_name,
                time_column,
            });
        };

        let time_format = self.time_format.unwrap_or(TimeFormat::Timestamp);
        let data_type = field.data_type().clone();

        let mut invalid = false;
        match data_type {
            arrow::datatypes::DataType::Utf8 | arrow::datatypes::DataType::LargeUtf8 => {
                if time_format != TimeFormat::ISO8601 {
                    invalid = true;
                }
            }
            arrow::datatypes::DataType::Int8
            | arrow::datatypes::DataType::Int16
            | arrow::datatypes::DataType::Int32
            | arrow::datatypes::DataType::Int64
            | arrow::datatypes::DataType::UInt8
            | arrow::datatypes::DataType::UInt16
            | arrow::datatypes::DataType::UInt32
            | arrow::datatypes::DataType::UInt64
            | arrow::datatypes::DataType::Float16
            | arrow::datatypes::DataType::Float32
            | arrow::datatypes::DataType::Float64 => {
                if time_format != TimeFormat::UnixSeconds && time_format != TimeFormat::UnixMillis {
                    invalid = true;
                }
            }
            arrow::datatypes::DataType::Timestamp(_, None) => {
                if time_format != TimeFormat::Timestamp {
                    invalid = true;
                }
            }
            arrow::datatypes::DataType::Timestamp(_, Some(_)) => {
                if time_format != TimeFormat::Timestamptz {
                    invalid = true;
                }
            }
            arrow::datatypes::DataType::Null
            | arrow::datatypes::DataType::Boolean
            | arrow::datatypes::DataType::Date32
            | arrow::datatypes::DataType::Date64
            | arrow::datatypes::DataType::Time32(_)
            | arrow::datatypes::DataType::Time64(_)
            | arrow::datatypes::DataType::Duration(_)
            | arrow::datatypes::DataType::Interval(_)
            | arrow::datatypes::DataType::Binary
            | arrow::datatypes::DataType::FixedSizeBinary(_)
            | arrow::datatypes::DataType::LargeBinary
            | arrow::datatypes::DataType::BinaryView
            | arrow::datatypes::DataType::Utf8View
            | arrow::datatypes::DataType::List(_)
            | arrow::datatypes::DataType::ListView(_)
            | arrow::datatypes::DataType::FixedSizeList(_, _)
            | arrow::datatypes::DataType::LargeList(_)
            | arrow::datatypes::DataType::LargeListView(_)
            | arrow::datatypes::DataType::Struct(_)
            | arrow::datatypes::DataType::Union(_, _)
            | arrow::datatypes::DataType::Dictionary(_, _)
            | arrow::datatypes::DataType::Decimal128(_, _)
            | arrow::datatypes::DataType::Decimal256(_, _)
            | arrow::datatypes::DataType::Map(_, _)
            | arrow::datatypes::DataType::RunEndEncoded(_, _) => {
                invalid = true;
            }
        };

        if invalid {
            return Err(Error::TimeFormatMismatch {
                table_name: dataset_name,
                time_column,
                expected_time_format: time_format.to_string(),
                actual_time_format: data_type.to_string(),
            });
        };

        Ok(())
    }
}

impl Default for Refresh {
    fn default() -> Self {
        Self {
            time_column: None,
            time_format: None,
            check_interval: None,
            max_jitter: None,
            sql: None,
            mode: RefreshMode::Full,
            period: None,
            append_overlap: None,
            refresh_retry_enabled: false,
            refresh_retry_max_attempts: None,
        }
    }
}

pub(crate) enum AccelerationRefreshMode {
    Disabled,
    Full(Receiver<()>),
    Append(Option<Receiver<()>>),
    Changes(ChangesStream),
}

pub struct Refresher {
    dataset_name: TableReference,
    federated: Arc<dyn TableProvider>,
    refresh: Arc<RwLock<Refresh>>,
    accelerator: Arc<dyn TableProvider>,
    cache_provider: Option<Arc<QueryResultsCacheProvider>>,
    refresh_task_runner: RefreshTaskRunner,
}

impl Refresher {
    pub(crate) fn new(
        dataset_name: TableReference,
        federated: Arc<dyn TableProvider>,
        refresh: Arc<RwLock<Refresh>>,
        accelerator: Arc<dyn TableProvider>,
    ) -> Self {
        let refresh_task_runner = RefreshTaskRunner::new(
            dataset_name.clone(),
            Arc::clone(&federated),
            Arc::clone(&refresh),
            Arc::clone(&accelerator),
        );

        Self {
            dataset_name,
            federated,
            refresh,
            accelerator,
            cache_provider: None,
            refresh_task_runner,
        }
    }

    pub fn cache_provider(
        &mut self,
        cache_provider: Option<Arc<QueryResultsCacheProvider>>,
    ) -> &mut Self {
        self.cache_provider = cache_provider;
        self
    }

    fn compute_delay(period: Duration, max_jitter: Option<Duration>) -> Duration {
        match max_jitter {
            Some(max_jitter) => {
                let jitter = rand::thread_rng().gen_range(Duration::from_secs(0)..max_jitter);
                period + jitter
            }
            None => period,
        }
    }

    pub(crate) async fn start(
        &mut self,
        acceleration_refresh_mode: AccelerationRefreshMode,
        ready_sender: oneshot::Sender<()>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let time_column = self.refresh.read().await.time_column.clone();

        let mut on_start_refresh_external = match acceleration_refresh_mode {
            AccelerationRefreshMode::Disabled => return None,
            AccelerationRefreshMode::Append(receiver) => {
                if let (Some(receiver), Some(_)) = (receiver, time_column) {
                    receiver
                } else {
                    return Some(self.start_streaming_append(ready_sender));
                }
            }
            AccelerationRefreshMode::Full(receiver) => receiver,
            AccelerationRefreshMode::Changes(stream) => {
                return Some(self.start_changes_stream(stream, ready_sender));
            }
        };

        let (start_refresh, mut on_refresh_complete) = self.refresh_task_runner.start();

        let mut ready_sender = Some(ready_sender);
        let dataset_name = self.dataset_name.clone();
        let refresh = Arc::clone(&self.refresh);

        let cache_provider = self.cache_provider.clone();

        let refresh_check_interval = self.refresh.read().await.check_interval;
        let max_jitter = self.refresh.read().await.max_jitter;

        Some(tokio::spawn(async move {
            // first refresh is on start, thus duration is 0
            let mut next_scheduled_refresh_timer = Some(sleep(Self::compute_delay(
                Duration::from_secs(0),
                max_jitter,
            )));

            loop {
                let scheduled_refresh_future: BoxFuture<()> =
                    match next_scheduled_refresh_timer.take() {
                        Some(timer) => Box::pin(timer),
                        None => Box::pin(std::future::pending()),
                    };

                select! {
                    () = scheduled_refresh_future => {
                        tracing::debug!("Starting scheduled refresh");
                        if let Err(err) = start_refresh.send(()).await {
                            tracing::error!("Failed to execute refresh: {err}");
                        }
                    },
                    _ = on_start_refresh_external.recv() => {
                        tracing::debug!("Received external trigger to start refresh");

                        if let Err(err) = start_refresh.send(()).await {
                            tracing::error!("Failed to execute refresh: {err}");
                        }
                    },
                    Some(res) = on_refresh_complete.recv() => {
                        tracing::debug!("Received refresh task completion callback: {res:?}");

                        if let Ok(()) = res {
                            notify_refresh_done(&dataset_name, &refresh, &mut ready_sender).await;

                            if let Some(cache_provider) = &cache_provider {
                                if let Err(e) = cache_provider
                                    .invalidate_for_table(dataset_name.clone())
                                    .await
                                {
                                    tracing::error!("Failed to invalidate cached results for dataset {}: {e}", &dataset_name.to_string());
                                }
                            }
                        }

                        if let Some(refresh_check_interval) = refresh_check_interval {
                            next_scheduled_refresh_timer = Some(sleep(Self::compute_delay(
                                refresh_check_interval,
                                max_jitter,
                            )));
                        }
                    }
                }
            }
        }))
    }

    fn start_streaming_append(
        &mut self,
        ready_sender: oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let refresh_task = Arc::new(RefreshTask::new(
            self.dataset_name.clone(),
            Arc::clone(&self.federated),
            Arc::clone(&self.refresh),
            Arc::clone(&self.accelerator),
        ));

        let cache_provider = self.cache_provider.clone();

        tokio::spawn(async move {
            if let Err(err) = refresh_task
                .start_streaming_append(cache_provider, Some(ready_sender))
                .await
            {
                tracing::error!("Append refresh failed with error: {err}");
            }
        })
    }

    fn start_changes_stream(
        &mut self,
        changes_stream: ChangesStream,
        ready_sender: oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let refresh_task = Arc::new(RefreshTask::new(
            self.dataset_name.clone(),
            Arc::clone(&self.federated),
            Arc::clone(&self.refresh),
            Arc::clone(&self.accelerator),
        ));

        let cache_provider = self.cache_provider.clone();

        tokio::spawn(async move {
            if let Err(err) = refresh_task
                .start_changes_stream(changes_stream, cache_provider, Some(ready_sender))
                .await
            {
                tracing::error!("Changes stream failed with error: {err}");
            }
        })
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        self.refresh_task_runner.abort();
    }
}

pub(crate) fn get_timestamp(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

async fn notify_refresh_done(
    dataset_name: &TableReference,
    refresh: &Arc<RwLock<Refresh>>,
    ready_sender: &mut Option<oneshot::Sender<()>>,
) {
    if let Some(sender) = ready_sender.take() {
        sender.send(()).ok();
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let mut labels = vec![Key::from_static_str("dataset").string(dataset_name.to_string())];
    if let Some(sql) = &refresh.read().await.sql {
        labels.push(Key::from_static_str("sql").string(sql.to_string()));
    };

    metrics::LAST_REFRESH_TIME.record(now.as_secs_f64(), &labels);
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;

    use arrow::{
        array::{ArrowNativeTypeOp, RecordBatch, StringArray, StructArray, UInt64Array},
        datatypes::{DataType, Field, Fields, Schema},
    };
    use data_components::arrow::write::MemTable;
    use datafusion::{physical_plan::collect, prelude::SessionContext};
    use opentelemetry::global;
    use opentelemetry_sdk::{metrics::SdkMeterProvider, Resource};
    use prometheus::proto::MetricType;
    use tokio::{sync::mpsc, time::timeout};

    use crate::status;

    use super::*;

    async fn setup_and_test(
        source_data: Vec<&str>,
        existing_data: Vec<&str>,
        expected_size: usize,
    ) {
        let schema = Arc::new(Schema::new(vec![arrow::datatypes::Field::new(
            "time_in_string",
            DataType::Utf8,
            false,
        )]));
        let arr = StringArray::from(source_data);

        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(arr)])
            .expect("data should be created");

        let federated = Arc::new(
            MemTable::try_new(Arc::clone(&schema), vec![vec![batch]])
                .expect("mem table should be created"),
        );

        let arr = StringArray::from(existing_data);

        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(arr)])
            .expect("data should be created");

        let accelerator = Arc::new(
            MemTable::try_new(schema, vec![vec![batch]]).expect("mem table should be created"),
        ) as Arc<dyn TableProvider>;

        let refresh = Refresh::new(RefreshMode::Full);

        let mut refresher = Refresher::new(
            TableReference::bare("test"),
            federated,
            Arc::new(RwLock::new(refresh)),
            Arc::clone(&accelerator),
        );

        let (trigger, receiver) = mpsc::channel::<()>(1);
        let (ready_sender, is_ready) = oneshot::channel::<()>();
        let acceleration_refresh_mode = AccelerationRefreshMode::Full(receiver);
        let refresh_handle = refresher
            .start(acceleration_refresh_mode, ready_sender)
            .await;

        trigger
            .send(())
            .await
            .expect("trigger sent correctly to refresh");

        timeout(Duration::from_secs(2), async move {
            is_ready.await.expect("data is received");
        })
        .await
        .expect("finish before the timeout");

        let ctx = SessionContext::new();
        let state = ctx.state();

        let plan = accelerator
            .scan(&state, None, &[], None)
            .await
            .expect("Scan plan can be constructed");

        let result = collect(plan, ctx.task_ctx())
            .await
            .expect("Query successful");

        assert_eq!(expected_size, result.first().expect("result").num_rows());

        drop(refresh_handle);
    }

    #[tokio::test]
    async fn test_refresh_full() {
        setup_and_test(
            vec!["1970-01-01", "2012-12-01T11:11:11Z", "2012-12-01T11:11:12Z"],
            vec![],
            3,
        )
        .await;
        setup_and_test(
            vec!["1970-01-01", "2012-12-01T11:11:11Z", "2012-12-01T11:11:12Z"],
            vec![
                "1970-01-01",
                "2012-12-01T11:11:11Z",
                "2012-12-01T11:11:12Z",
                "2012-12-01T11:11:15Z",
            ],
            3,
        )
        .await;
        setup_and_test(
            vec![],
            vec![
                "1970-01-01",
                "2012-12-01T11:11:11Z",
                "2012-12-01T11:11:12Z",
                "2012-12-01T11:11:15Z",
            ],
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn test_refresh_status_change_to_ready() {
        fn wait_until_ready_status(
            registry: &prometheus::Registry,
            desired: status::ComponentStatus,
        ) -> bool {
            for _i in 1..20 {
                let hashmap = registry.gather();
                let metric = hashmap
                    .iter()
                    .find(|m| m.get_name() == "datasets_status")
                    .expect("datasets_status metric exists");
                match metric.get_field_type() {
                    MetricType::GAUGE => {
                        let value = metric.get_metric()[0].get_gauge().get_value();

                        if value.is_eq(f64::from(desired as i32)) {
                            return true;
                        }
                    }
                    _ => panic!("datasets_status is a gauge"),
                }

                sleep(Duration::from_millis(100));
            }

            false
        }

        let registry = prometheus::Registry::new();

        let resource = Resource::default();

        let prometheus_exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .without_scope_info()
            .without_units()
            .without_counter_suffixes()
            .without_target_info()
            .build()
            .expect("to build prometheus exporter");

        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(prometheus_exporter)
            .build();
        global::set_meter_provider(provider);

        status::update_dataset(
            &TableReference::bare("test"),
            status::ComponentStatus::Refreshing,
        );

        setup_and_test(
            vec!["1970-01-01", "2012-12-01T11:11:11Z", "2012-12-01T11:11:12Z"],
            vec![],
            3,
        )
        .await;

        assert!(wait_until_ready_status(
            &registry,
            status::ComponentStatus::Ready
        ));

        status::update_dataset(
            &TableReference::bare("test"),
            status::ComponentStatus::Refreshing,
        );

        setup_and_test(vec![], vec![], 0).await;

        assert!(wait_until_ready_status(
            &registry,
            status::ComponentStatus::Ready
        ));
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn test_refresh_append_batch_for_iso8601() {
        async fn test(
            source_data: Vec<&str>,
            existing_data: Vec<&str>,
            expected_size: usize,
            message: &str,
        ) {
            let schema = Arc::new(Schema::new(vec![arrow::datatypes::Field::new(
                "time_in_string",
                DataType::Utf8,
                false,
            )]));
            let arr = StringArray::from(source_data);

            let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(arr)])
                .expect("data should be created");

            let federated = Arc::new(
                MemTable::try_new(Arc::clone(&schema), vec![vec![batch]])
                    .expect("mem table should be created"),
            );

            let arr = StringArray::from(existing_data);

            let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(arr)])
                .expect("data should be created");

            let accelerator = Arc::new(
                MemTable::try_new(schema, vec![vec![batch]]).expect("mem table should be created"),
            ) as Arc<dyn TableProvider>;

            let refresh = Refresh::new(RefreshMode::Append)
                .time_column("time_in_string".to_string())
                .time_format(TimeFormat::ISO8601);

            let mut refresher = Refresher::new(
                TableReference::bare("test"),
                federated,
                Arc::new(RwLock::new(refresh)),
                Arc::clone(&accelerator),
            );

            let (trigger, receiver) = mpsc::channel::<()>(1);
            let (ready_sender, is_ready) = oneshot::channel::<()>();
            let acceleration_refresh_mode = AccelerationRefreshMode::Append(Some(receiver));
            let refresh_handle = refresher
                .start(acceleration_refresh_mode, ready_sender)
                .await;

            trigger
                .send(())
                .await
                .expect("trigger sent correctly to refresh");

            timeout(Duration::from_secs(2), async move {
                is_ready.await.expect("data is received");
            })
            .await
            .expect("finish before the timeout");

            let ctx = SessionContext::new();
            let state = ctx.state();

            let plan = accelerator
                .scan(&state, None, &[], None)
                .await
                .expect("Scan plan can be constructed");

            let result = collect(plan, ctx.task_ctx())
                .await
                .expect("Query successful");

            assert_eq!(
                expected_size,
                result.into_iter().map(|f| f.num_rows()).sum::<usize>(),
                "{message}"
            );

            drop(refresh_handle);
        }

        test(
            vec!["1970-01-01", "2012-12-01T11:11:11Z", "2012-12-01T11:11:12Z"],
            vec![],
            3,
            "should insert all data into empty accelerator",
        )
        .await;
        test(
            vec!["1970-01-01", "2012-12-01T11:11:11Z", "2012-12-01T11:11:12Z"],
            vec![
                "1970-01-01",
                "2012-12-01T11:11:11Z",
                "2012-12-01T11:11:12Z",
                "2012-12-01T11:11:15Z",
            ],
            4,
            "should not insert any stale data and keep original size",
        )
        .await;
        test(
            vec![],
            vec![
                "1970-01-01",
                "2012-12-01T11:11:11Z",
                "2012-12-01T11:11:12Z",
                "2012-12-01T11:11:15Z",
            ],
            4,
            "should keep original data of accelerator when no new data is found",
        )
        .await;
        test(
            vec!["2012-12-01T11:11:16Z", "2012-12-01T11:11:17Z"],
            vec![
                "1970-01-01",
                "2012-12-01T11:11:11Z",
                "2012-12-01T11:11:12Z",
                "2012-12-01T11:11:15Z",
            ],
            6,
            "should apply new data onto existing data",
        )
        .await;

        // Known limitation, doesn't dedup
        test(
            vec!["2012-12-01T11:11:15Z", "2012-12-01T11:11:15Z"],
            vec![
                "1970-01-01",
                "2012-12-01T11:11:11Z",
                "2012-12-01T11:11:12Z",
                "2012-12-01T11:11:15Z",
            ],
            4,
            "should not apply same timestamp data",
        )
        .await;
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn test_refresh_append_batch_for_timestamp() {
        async fn test(
            source_data: Vec<u64>,
            existing_data: Vec<u64>,
            expected_size: usize,
            time_format: Option<TimeFormat>,
            append_overlap: Option<Duration>,
            message: &str,
        ) {
            let schema = Arc::new(Schema::new(vec![arrow::datatypes::Field::new(
                "time",
                DataType::UInt64,
                false,
            )]));
            let arr = UInt64Array::from(source_data);

            let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(arr)])
                .expect("data should be created");

            let federated = Arc::new(
                MemTable::try_new(Arc::clone(&schema), vec![vec![batch]])
                    .expect("mem table should be created"),
            );

            let arr = UInt64Array::from(existing_data);

            let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(arr)])
                .expect("data should be created");

            let accelerator = Arc::new(
                MemTable::try_new(schema, vec![vec![batch]]).expect("mem table should be created"),
            ) as Arc<dyn TableProvider>;

            let mut refresh = Refresh::new(RefreshMode::Append).time_column("time".to_string());

            if let Some(time_format) = time_format {
                refresh = refresh.time_format(time_format);
            }

            if let Some(append_overlap) = append_overlap {
                refresh = refresh.append_overlap(append_overlap);
            }

            let mut refresher = Refresher::new(
                TableReference::bare("test"),
                federated,
                Arc::new(RwLock::new(refresh)),
                Arc::clone(&accelerator),
            );

            let (trigger, receiver) = mpsc::channel::<()>(1);
            let (ready_sender, is_ready) = oneshot::channel::<()>();
            let acceleration_refresh_mode = AccelerationRefreshMode::Append(Some(receiver));
            let refresh_handle = refresher
                .start(acceleration_refresh_mode, ready_sender)
                .await;

            trigger
                .send(())
                .await
                .expect("trigger sent correctly to refresh");

            timeout(Duration::from_secs(2), async move {
                is_ready.await.expect("data is received");
            })
            .await
            .expect("finish before the timeout");

            let ctx = SessionContext::new();
            let state = ctx.state();

            let plan = accelerator
                .scan(&state, None, &[], None)
                .await
                .expect("Scan plan can be constructed");

            let result = collect(plan, ctx.task_ctx())
                .await
                .expect("Query successful");

            assert_eq!(
                expected_size,
                result.into_iter().map(|f| f.num_rows()).sum::<usize>(),
                "{message}"
            );

            drop(refresh_handle);
        }

        test(
            vec![1, 2, 3],
            vec![],
            3,
            Some(TimeFormat::UnixSeconds),
            None,
            "should insert all data into empty accelerator",
        )
        .await;
        test(
            vec![1, 2, 3],
            vec![2, 3, 4, 5],
            4,
            Some(TimeFormat::UnixSeconds),
            None,
            "should not insert any stale data and keep original size",
        )
        .await;
        test(
            vec![],
            vec![1, 2, 3, 4],
            4,
            Some(TimeFormat::UnixSeconds),
            None,
            "should keep original data of accelerator when no new data is found",
        )
        .await;
        test(
            vec![5, 6],
            vec![1, 2, 3, 4],
            6,
            Some(TimeFormat::UnixSeconds),
            None,
            "should apply new data onto existing data",
        )
        .await;

        // Known limitation, doesn't dedup
        test(
            vec![4, 4],
            vec![1, 2, 3, 4],
            4,
            Some(TimeFormat::UnixSeconds),
            None,
            "should not apply same timestamp data",
        )
        .await;

        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            10,
            Some(TimeFormat::UnixSeconds),
            Some(Duration::from_secs(10)),
            "should apply late arrival and new data onto existing data",
        )
        .await;

        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            7, // 1, 2, 3, 7, 8, 9, 10
            Some(TimeFormat::UnixSeconds),
            Some(Duration::from_secs(3)),
            "should apply late arrival within the append overlap period and new data onto existing data",
        )
        .await;

        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            10, // all the data
            Some(TimeFormat::UnixMillis),
            Some(Duration::from_secs(3)),
            "should fetch all data as 3 seconds is enough to cover all time span in source with millis",
        )
        .await;
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn test_refresh_append_batch_for_timestamp_with_more_complicated_structs() {
        async fn test(
            source_data: Vec<u64>,
            existing_data: Vec<u64>,
            expected_size: usize,
            time_format: Option<TimeFormat>,
            append_overlap: Option<Duration>,
            duplicated_incoming_data: bool,
            message: &str,
        ) {
            let original_schema = Arc::new(Schema::new(vec![arrow::datatypes::Field::new(
                "time",
                DataType::UInt64,
                false,
            )]));
            let arr = UInt64Array::from(source_data);
            let batch =
                RecordBatch::try_new(Arc::clone(&original_schema), vec![Arc::new(arr.clone())])
                    .expect("data should be created");

            let struct_array = StructArray::from(batch);
            let schema = Arc::new(Schema::new(vec![
                arrow::datatypes::Field::new("time", DataType::UInt64, false),
                arrow::datatypes::Field::new(
                    "struct",
                    DataType::Struct(Fields::from(vec![arrow::datatypes::Field::new(
                        "time",
                        DataType::UInt64,
                        false,
                    )])),
                    false,
                ),
            ]));
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![Arc::new(arr), Arc::new(struct_array)],
            )
            .expect("data should be created");

            let mut data = vec![vec![batch.clone()]];
            if duplicated_incoming_data {
                data = vec![vec![batch.clone()], vec![batch]];
            }

            let federated = Arc::new(
                MemTable::try_new(Arc::clone(&schema), data).expect("mem table should be created"),
            );

            let arr = UInt64Array::from(existing_data);
            let batch =
                RecordBatch::try_new(Arc::clone(&original_schema), vec![Arc::new(arr.clone())])
                    .expect("data should be created");
            let struct_array = StructArray::from(batch);
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![Arc::new(arr), Arc::new(struct_array)],
            )
            .expect("data should be created");

            let accelerator = Arc::new(
                MemTable::try_new(schema, vec![vec![batch]]).expect("mem table should be created"),
            ) as Arc<dyn TableProvider>;

            let mut refresh = Refresh::new(RefreshMode::Append).time_column("time".to_string());

            if let Some(time_format) = time_format {
                refresh = refresh.time_format(time_format);
            }

            if let Some(append_overlap) = append_overlap {
                refresh = refresh.append_overlap(append_overlap);
            }

            let mut refresher = Refresher::new(
                TableReference::bare("test"),
                federated,
                Arc::new(RwLock::new(refresh)),
                Arc::clone(&accelerator),
            );

            let (trigger, receiver) = mpsc::channel::<()>(1);
            let (ready_sender, is_ready) = oneshot::channel::<()>();
            let acceleration_refresh_mode = AccelerationRefreshMode::Append(Some(receiver));
            let refresh_handle = refresher
                .start(acceleration_refresh_mode, ready_sender)
                .await;
            trigger
                .send(())
                .await
                .expect("trigger sent correctly to refresh");

            timeout(Duration::from_secs(2), async move {
                is_ready.await.expect("data is received");
            })
            .await
            .expect("finish before the timeout");

            let ctx = SessionContext::new();
            let state = ctx.state();

            let plan = accelerator
                .scan(&state, None, &[], None)
                .await
                .expect("Scan plan can be constructed");

            let result = collect(plan, ctx.task_ctx())
                .await
                .expect("Query successful");

            assert_eq!(
                expected_size,
                result.into_iter().map(|f| f.num_rows()).sum::<usize>(),
                "{message}"
            );

            drop(refresh_handle);
        }

        test(
            vec![1, 2, 3],
            vec![],
            3,
            Some(TimeFormat::UnixSeconds),
            None,
            false,
            "should insert all data into empty accelerator",
        )
        .await;
        test(
            vec![1, 2, 3],
            vec![2, 3, 4, 5],
            4,
            Some(TimeFormat::UnixSeconds),
            None,
            false,
            "should not insert any stale data and keep original size",
        )
        .await;
        test(
            vec![],
            vec![1, 2, 3, 4],
            4,
            Some(TimeFormat::UnixSeconds),
            None,
            false,
            "should keep original data of accelerator when no new data is found",
        )
        .await;
        test(
            vec![5, 6],
            vec![1, 2, 3, 4],
            6,
            Some(TimeFormat::UnixSeconds),
            None,
            false,
            "should apply new data onto existing data",
        )
        .await;

        // Known limitation, doesn't dedup
        test(
            vec![4, 4],
            vec![1, 2, 3, 4],
            4,
            Some(TimeFormat::UnixSeconds),
            None,
            false,
            "should not apply same timestamp data",
        )
        .await;

        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            10,
            Some(TimeFormat::UnixSeconds),
            Some(Duration::from_secs(10)),
            false,
            "should apply late arrival and new data onto existing data",
        )
        .await;

        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            7, // 1, 2, 3, 7, 8, 9, 10
            Some(TimeFormat::UnixSeconds),
            Some(Duration::from_secs(3)),
            false,
            "should apply late arrival within the append overlap period and new data onto existing data",
        )
        .await;

        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            10, // all the data
            Some(TimeFormat::UnixMillis),
            Some(Duration::from_secs(3)),
            false,
            "should fetch all data as 3 seconds is enough to cover all time span in source with millis",
        )
        .await;
        test(
            vec![4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 9],
            16, // all the data
            Some(TimeFormat::UnixMillis),
            Some(Duration::from_secs(3)),
            true,
            "should fetch all data from all fetched record batches as 3 seconds is enough to cover all time span in source with millis",
        )
        .await;
    }

    #[test]
    fn test_validate_time_column_when_no_time_column() {
        let refresh = Refresh::new(RefreshMode::Full);
        let schema = Arc::new(Schema::empty());
        assert!(refresh
            .validate_time_format("dataset_name".to_string(), &schema)
            .is_ok());
    }

    #[test]
    fn test_validate_time_column_when_time_column_not_found() {
        let refresh = Refresh::new(RefreshMode::Append).time_column("time".to_string());

        let schema = Arc::new(Schema::empty());
        assert!(matches!(
            refresh.validate_time_format("test_dataset".to_string(), &schema),
            Err(Error::NoTimeColumnFound { .. })
        ));
    }

    #[test]
    fn test_validate_time_column_when_iso8601_mismatch() {
        for format in [
            TimeFormat::UnixSeconds,
            TimeFormat::UnixMillis,
            TimeFormat::Timestamp,
            TimeFormat::Timestamptz,
        ] {
            let refresh = Refresh::new(RefreshMode::Full)
                .time_column("time".to_string())
                .time_format(format);
            let schema = Arc::new(Schema::new(vec![Field::new("time", DataType::Utf8, false)]));
            assert!(matches!(
                refresh.validate_time_format("test_dataset".to_string(), &schema),
                Err(Error::TimeFormatMismatch { .. })
            ));
        }
    }

    #[test]
    fn test_validate_time_column_when_unix_timestamp_mismatch() {
        for format in [
            TimeFormat::Timestamp,
            TimeFormat::Timestamptz,
            TimeFormat::ISO8601,
        ] {
            let refresh = Refresh::new(RefreshMode::Full)
                .time_column("time".to_string())
                .time_format(format);

            let schema = Arc::new(Schema::new(vec![Field::new(
                "time",
                DataType::Int64,
                false,
            )]));
            assert!(matches!(
                refresh.validate_time_format("test_dataset".to_string(), &schema),
                Err(Error::TimeFormatMismatch { .. })
            ));
        }
    }

    #[test]
    fn test_validate_time_column_when_timestamp_mismatch() {
        for format in [
            TimeFormat::UnixMillis,
            TimeFormat::UnixSeconds,
            TimeFormat::Timestamptz,
            TimeFormat::ISO8601,
        ] {
            let refresh = Refresh::new(RefreshMode::Full)
                .time_column("time".to_string())
                .time_format(format);

            let schema = Arc::new(Schema::new(vec![Field::new(
                "time",
                DataType::Timestamp(arrow::datatypes::TimeUnit::Second, None),
                false,
            )]));
            assert!(matches!(
                refresh.validate_time_format("test_dataset".to_string(), &schema),
                Err(Error::TimeFormatMismatch { .. })
            ));
        }
    }

    #[test]
    fn test_validate_time_column_when_timestamptz_mismatch() {
        for format in [
            TimeFormat::UnixMillis,
            TimeFormat::UnixSeconds,
            TimeFormat::Timestamp,
            TimeFormat::ISO8601,
        ] {
            let refresh = Refresh::new(RefreshMode::Full)
                .time_column("time".to_string())
                .time_format(format);

            let schema = Arc::new(Schema::new(vec![Field::new(
                "time",
                DataType::Timestamp(arrow::datatypes::TimeUnit::Second, Some("+00:00".into())),
                false,
            )]));
            assert!(matches!(
                refresh.validate_time_format("test_dataset".to_string(), &schema),
                Err(Error::TimeFormatMismatch { .. })
            ));
        }
    }

    #[test]
    fn test_validate_time_column_when_iso8601_match() {
        let refresh = Refresh::new(RefreshMode::Full)
            .time_column("time".to_string())
            .time_format(TimeFormat::ISO8601);

        let schema = Arc::new(Schema::new(vec![Field::new("time", DataType::Utf8, false)]));
        assert!(refresh
            .validate_time_format("dataset_name".to_string(), &schema)
            .is_ok());
    }

    #[test]
    fn test_validate_time_column_when_unix_timestamp_match() {
        for format in [TimeFormat::UnixMillis, TimeFormat::UnixSeconds] {
            let refresh = Refresh::new(RefreshMode::Full)
                .time_column("time".to_string())
                .time_format(format);

            let schema = Arc::new(Schema::new(vec![Field::new(
                "time",
                DataType::Int64,
                false,
            )]));
            assert!(refresh
                .validate_time_format("dataset_name".to_string(), &schema)
                .is_ok());
        }
    }

    #[test]
    fn test_validate_time_column_when_timestamp_match() {
        let refresh = Refresh::new(RefreshMode::Full)
            .time_column("time".to_string())
            .time_format(TimeFormat::Timestamp);

        let schema = Arc::new(Schema::new(vec![Field::new(
            "time",
            DataType::Timestamp(arrow::datatypes::TimeUnit::Second, None),
            false,
        )]));
        assert!(refresh
            .validate_time_format("dataset_name".to_string(), &schema)
            .is_ok());
    }

    #[test]
    fn test_validate_time_column_when_timestamptz_match() {
        let refresh = Refresh::new(RefreshMode::Full)
            .time_column("time".to_string())
            .time_format(TimeFormat::Timestamptz);

        let schema = Arc::new(Schema::new(vec![Field::new(
            "time",
            DataType::Timestamp(arrow::datatypes::TimeUnit::Second, Some("+00:00".into())),
            false,
        )]));
        assert!(refresh
            .validate_time_format("dataset_name".to_string(), &schema)
            .is_ok());
    }
}
