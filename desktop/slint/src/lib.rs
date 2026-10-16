//! Isolated original-Slint adapter qualification. There is deliberately no
//! constructor accepting an arbitrary backend URL or an existing profile.
pub mod bridge;
pub mod fixture;
mod http;
#[cfg(feature = "native-ui")]
pub mod native;
pub mod navigation;
pub mod owned_backend;

pub mod settings;

#[cfg(feature = "native-ui")]
pub mod picker;

pub mod catalog_bridge;
pub mod catalog_controller;
pub mod conversation_export;
pub mod documents;

pub mod mlx;

pub mod task_bridge;

pub mod profile_bridge;

pub mod agents_bridge;
pub mod transcription;

pub mod messages;

pub mod batch;

pub mod memory_bridge;

pub mod historical_memory_bridge;
#[path = "../../../src/historical_memory_contract.rs"]
pub mod historical_memory_contract;
#[path = "../../../src/historical_message_contract.rs"]
pub mod historical_message_contract;
pub mod historical_presentation;
pub mod historical_source_bridge;

pub mod extensions_bridge;

pub mod historical_message_bridge;

pub mod platform_ui;
