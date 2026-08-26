//! 插件系统：Rust 版 cordis 语义（loader / group / 事件总线）。

pub mod registry;

pub use registry::{
    Entry, EventBus, FiberState, Loader, Plugin, PluginContext, PluginEvent, PluginGroup,
};
