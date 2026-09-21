//! `acp-recorder` — records the ordered ACP frames exchanged with an ACP agent.
//!
//! A binary + library crate. [`record`](recorder::record) spawns any ACP agent
//! command over stdio, drives a [`Script`](script::Script) of ACP calls, and
//! returns the ordered JSON-RPC frames in both directions.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod frames;
pub mod recorder;
pub mod script;

pub use frames::{Direction, Frame};
pub use recorder::{record, Error};
pub use script::{PermissionPolicy, Script, Step};
