// SPDX-License-Identifier: Apache-2.0

//! Shared Rust types for the canonical `WinWinCode` `ExecutionPort`.
//!
//! The wire declarations are generated from
//! `schema/winwincode/v1/execution-port.schema.json`; the schema remains the
//! only source of public message shapes.

pub mod generated;
pub mod replay;
pub mod runtime_replay;
pub mod transport;
pub mod typed_replay;
