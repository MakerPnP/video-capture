use cfg_if::cfg_if;

extern crate x_base as base;
pub extern crate x_media as media;
pub extern crate x_variant as variant;

pub mod camera;
pub mod device;
pub mod error;

cfg_if! {
    if #[cfg(any(target_os = "ios", target_os = "macos"))] {
        #[path = "mac/mod.rs"]
        pub mod backend;
    } else if #[cfg(target_os = "windows")] {
        #[path = "windows/mod.rs"]
        pub mod backend;
    } else if #[cfg(target_os = "linux")] {
        #[path = "linux/mod.rs"]
        pub mod backend;
    } else {
        compile_error!("unsupported target");
    }
}
