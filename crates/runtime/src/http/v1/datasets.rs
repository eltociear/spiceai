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

use crate::{component::dataset::Dataset, LogErrors, Runtime};
use app::App;
use axum::{
    extract::Path,
    extract::Query,
    http::status,
    response::{IntoResponse, Response},
    Extension, Json,
};
use datafusion::sql::TableReference;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tract_core::tract_data::itertools::Itertools;

use crate::{datafusion::DataFusion, status::ComponentStatus};

use super::{convert_entry_to_csv, dataset_status, Format};

#[derive(Debug, Deserialize)]
pub(crate) struct DatasetFilter {
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DatasetQueryParams {
    #[serde(default)]
    status: bool,

    #[serde(default)]
    format: Format,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) struct DatasetResponseItem {
    pub from: String,
    pub name: String,
    pub replication_enabled: bool,
    pub acceleration_enabled: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ComponentStatus>,
}

pub(crate) async fn get(
    Extension(app): Extension<Arc<RwLock<Option<Arc<App>>>>>,
    Extension(df): Extension<Arc<DataFusion>>,
    Query(filter): Query<DatasetFilter>,
    Query(params): Query<DatasetQueryParams>,
) -> Response {
    let app_lock = app.read().await;
    let Some(readable_app) = app_lock.as_ref() else {
        return (
            status::StatusCode::INTERNAL_SERVER_ERROR,
            Json::<Vec<DatasetResponseItem>>(vec![]),
        )
            .into_response();
    };

    let valid_datasets = Runtime::get_valid_datasets(readable_app, LogErrors(false));
    let datasets: Vec<Arc<Dataset>> = match filter.source {
        Some(source) => valid_datasets
            .into_iter()
            .filter(|d| d.source() == source)
            .collect(),
        None => valid_datasets,
    };

    let resp = datasets
        .iter()
        .map(|d| DatasetResponseItem {
            from: d.from.clone(),
            name: d.name.to_quoted_string(),
            replication_enabled: d.replication.as_ref().is_some_and(|f| f.enabled),
            acceleration_enabled: d.acceleration.as_ref().is_some_and(|f| f.enabled),
            status: if params.status {
                Some(dataset_status(&df, d))
            } else {
                None
            },
        })
        .collect_vec();

    match params.format {
        Format::Json => (status::StatusCode::OK, Json(resp)).into_response(),
        Format::Csv => match convert_entry_to_csv(&resp) {
            Ok(csv) => (status::StatusCode::OK, csv).into_response(),
            Err(e) => {
                tracing::error!("Error converting to CSV: {e}");
                (status::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        },
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) struct MessageResponse {
    pub message: String,
}

#[derive(Deserialize)]
pub struct AccelerationRequest {
    pub refresh_sql: Option<String>,
}

pub(crate) async fn refresh(
    Extension(app): Extension<Arc<RwLock<Option<Arc<App>>>>>,
    Extension(df): Extension<Arc<DataFusion>>,
    Path(dataset_name): Path<String>,
) -> Response {
    let app_lock = app.read().await;
    let Some(readable_app) = &*app_lock else {
        return (status::StatusCode::INTERNAL_SERVER_ERROR).into_response();
    };

    let Some(dataset) = readable_app
        .datasets
        .iter()
        .find(|d| d.name.to_lowercase() == dataset_name.to_lowercase())
    else {
        return (
            status::StatusCode::NOT_FOUND,
            Json(MessageResponse {
                message: format!("Dataset {dataset_name} not found"),
            }),
        )
            .into_response();
    };

    let acceleration_enabled = dataset.acceleration.as_ref().is_some_and(|f| f.enabled);

    if !acceleration_enabled {
        return (
            status::StatusCode::BAD_REQUEST,
            Json(MessageResponse {
                message: format!("Dataset {dataset_name} does not have acceleration enabled"),
            }),
        )
            .into_response();
    };

    match df.refresh_table(&dataset.name).await {
        Ok(()) => (
            status::StatusCode::CREATED,
            Json(MessageResponse {
                message: format!("Dataset refresh triggered for {dataset_name}."),
            }),
        )
            .into_response(),
        Err(err) => (
            status::StatusCode::INTERNAL_SERVER_ERROR,
            Json(MessageResponse {
                message: format!("Failed to trigger refresh for {dataset_name}: {err}."),
            }),
        )
            .into_response(),
    }
}

pub(crate) async fn acceleration(
    Extension(app): Extension<Arc<RwLock<Option<Arc<App>>>>>,
    Extension(df): Extension<Arc<DataFusion>>,
    Path(dataset_name): Path<String>,
    Json(payload): Json<AccelerationRequest>,
) -> Response {
    let app_lock = app.read().await;
    let Some(readable_app) = &*app_lock else {
        return (status::StatusCode::INTERNAL_SERVER_ERROR).into_response();
    };

    let Some(dataset) = readable_app
        .datasets
        .iter()
        .find(|d| d.name.to_lowercase() == dataset_name.to_lowercase())
    else {
        return (
            status::StatusCode::NOT_FOUND,
            Json(MessageResponse {
                message: format!("Dataset {dataset_name} not found"),
            }),
        )
            .into_response();
    };

    if payload.refresh_sql.is_none() {
        return (status::StatusCode::OK).into_response();
    }

    match df
        .update_refresh_sql(
            TableReference::parse_str(&dataset.name),
            payload.refresh_sql,
        )
        .await
    {
        Ok(()) => (status::StatusCode::OK).into_response(),
        Err(e) => (
            status::StatusCode::INTERNAL_SERVER_ERROR,
            Json(MessageResponse {
                message: format!("Request failed. {e}"),
            }),
        )
            .into_response(),
    }
}
