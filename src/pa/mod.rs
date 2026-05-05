//! Power Automate target.
//!
//! Everything PA-specific lives under this module: the JSON emitter, the
//! Legacy Import Package zip builder, and (in `names`) the action-type and
//! function-library names that PA hardcodes. Modules outside `pa` should
//! be target-agnostic where reasonable; when they need a PA name, they
//! pull it from here rather than inlining the string.

pub mod decoder;
pub mod emitter;
pub mod functions;
pub mod names;
pub mod packager;
pub(crate) mod paexpr;
