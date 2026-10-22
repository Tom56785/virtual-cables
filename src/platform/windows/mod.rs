//! Windows integration.

mod attach;
mod broker;
mod control;
mod download;
mod gui;
mod priority;
mod remote;
mod service;
mod win;

pub use broker::run_attach_broker;
pub use gui::{run_gui, show_fatal_dialog};
pub use priority::{enter_audio_thread, raise_process_priority};
pub use service::{install_service, run_service, uninstall_service};
