// SPDX-License-Identifier: Apache-2.0

//! Shared Rust types for the canonical `WinWinCode` `ClientControlPort`.
//!
//! This crate is the hand-written contract skeleton for the multi-user
//! shared device client plan (plan sections 3.3, 7, and 9). It declares the
//! ten domain objects, the `Envelope` frame, and the client-to-server and
//! server-to-client message enums with their exact wire `kind` strings.
//!
//! The types are self-contained: they intentionally do not reference schema
//! JSON files and do not depend on other workspace crates, so the device
//! client and the control plane can adopt them independently. The wire
//! encodings (for example the decimal-string occupancy fencing token) live
//! in the [`wire`] module.

// The wire contract names many identifiers and brand words (`WinWinCode`,
// `ClientControlPort`) that the `doc_markdown` lint would demand backticks
// around; the docs stay readable without them.
#![allow(clippy::doc_markdown)]

pub mod domain;
pub mod messages;
pub mod wire;
