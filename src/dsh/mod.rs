//! DSH 集成聚合模块。

pub mod cli;
pub mod home;
pub mod plugins;
pub mod profile;

pub use cli::{find_dsh, spawn_web, WebHandle};
pub use home::{available_profiles, list_profiles, probe_web, ProfileInfo, WebProbe};
pub use plugins::{
    import_async, import_plugin, list_plugins, remove_plugin, set_plugin_enabled, PluginInfo,
};
