//! Library crate so the playback engine can be driven headlessly by test
//! harnesses (see `examples/capture.rs`) as well as by the GTK front-end.

pub mod library;
pub mod mpris;
pub mod player;
pub mod playlist;
pub mod settings;
pub mod silence;
