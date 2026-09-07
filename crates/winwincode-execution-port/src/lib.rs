// SPDX-License-Identifier: Apache-2.0

//! Shared Rust types for the canonical `WinWinCode` `ExecutionPort`.
//!
//! The wire declarations are generated from
//! `schema/winwincode/v1/execution-port.schema.json`; the schema remains the
//! only source of public message shapes.

pub mod action_enforcement;
pub mod action_gateway;
pub mod action_normalizer;
pub mod capability_adapter;
pub mod change_batch_identity;
pub mod change_batch_progress;
pub mod debug_hypothesis_ledger;
pub mod debug_probe_contract;
pub mod debug_probe_delta_context;
pub mod diagnostic_parser;
pub mod generated;
pub mod observation_contract;
pub mod performance_comparison;
pub mod probe_result_normalizer;
pub mod repair_loop_context;
pub mod replay;
pub mod runtime_replay;
pub mod runtime_trace_outbox;
pub mod transport;
pub mod typed_replay;
pub mod validation_config;
