//! Shared core for the shell DLL, the decoder worker and `twctl`: product identity, the
//! format matrix, resource budgets, the decoding pipeline, the worker wire protocol and
//! process isolation, settings, logging, and shell registration.

pub mod config;
pub mod decode;
pub mod formats;
pub mod ids;
pub mod limits;
pub mod log;
pub mod menu;
#[cfg(windows)]
pub mod pipeline;
pub mod protocol;
#[cfg(windows)]
pub mod registration;
#[cfg(windows)]
pub mod spawn;
#[cfg(windows)]
pub mod worker;

#[cfg(test)]
mod testalloc;

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: testalloc::Tracking = testalloc::Tracking;
