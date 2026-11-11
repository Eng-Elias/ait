//! `ait-app` — AIT's frontend-agnostic application layer (Spec R13,
//! §5.12's shared session model).
//!
//! Everything a frontend needs to host one `ait` session model lives
//! here: [`tabs::Tab`]/[`tabs::TabManager`]/[`tabs::SessionTemplate`],
//! [`blocks::BlockAssembler`], [`remote::RemoteContext`] +
//! [`remote::SshSpawn`], [`shell_bridge::TabBridge`], the [`ctl`]
//! server/codec/dispatch, the [`headless`] driver,
//! [`suggest::InputShadow`] + the suggestion triggers,
//! [`selection`]/[`scroll`] state, [`status::StatusLine`] +
//! [`status::Toast`], [`store::StoreHandle`],
//! [`transcript::Transcript`], [`config::LiveSettings`],
//! [`skills::LiveSkills`], the [`profile`]/[`conversations`] picker
//! state, [`inband::InBandExec`], [`integration::IntegrationState`],
//! [`agent_panel::AgentPanel`]'s state half, and the [`keymap`]
//! `BINDINGS` + [`keymap::resolve`] over [`ait_term::KeyEvent`] with a
//! per-frontend [`keymap::CapabilityTier`].
//!
//! A frontend contributes only its native→`ait_term` event
//! translation, its layout math (injected into the resize paths) and
//! its paint pass — the TUI in `ait` and the GUI in `ait-gui` render
//! the same model, so a session behaves identically whichever hosts
//! it (CODING_STANDARDS.md §9: business logic never lives in the
//! frontend).

pub mod agent_panel;
pub mod blocks;
pub mod config;
pub mod conversations;
pub mod ctl;
pub mod headless;
pub mod inband;
pub mod integration;
pub mod keymap;
pub mod profile;
pub mod remote;
pub mod scroll;
pub mod selection;
pub mod shell_bridge;
pub mod skills;
pub mod status;
pub mod store;
pub mod suggest;
pub mod tabs;
pub mod transcript;
