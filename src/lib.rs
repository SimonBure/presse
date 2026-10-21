#[macro_use]
mod macros;

pub mod cli;
pub mod format;
mod img;
pub mod pdf;
pub mod press;
pub mod transcode;

pub use cli::args::{Cli, resolve_merge_path_output, resolve_press_path_output};
pub use format::{Format, detect};
pub use img::convert::convert_image;
pub use press::{PdfOptions, PressOptions, Pressed, Presser};
