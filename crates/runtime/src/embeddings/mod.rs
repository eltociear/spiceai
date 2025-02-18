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
#![allow(unused_attributes)] // This is for the `f16_and_f128` feature.
#![feature(f16_and_f128)]
pub mod array_distance;
pub mod connector;
pub mod execution_plan;
pub mod table;
pub mod task;
pub mod vector_search;
