//! 插件系统：Rust 版 cordis 语义。
//!
//! 对齐 @deepseek-ai/cordis + cordis-plugin-loader 的核心语义：
//! - loader：插件条目（Entry）注册/更新/启停
//! - group：插件组（子 loader）
//! - 事件总线：插件间发布/订阅
//! JS 插件无法加载，故插件 = Rust 实现 Plugin trait 的动态注册。

use std::collections::HashMap;

use log::info;

/// 插件上下文（挂载时获得）。
pub struct PluginContext<'a> {
    pub id: &'a str,
    pub bus: &'a EventBus,
}

/// 插件 trait（等价于 cordis 插件的 apply 函数）。
pub trait Plugin: Send + Sync {
    /// 插件名（loader entry 名）
    fn name(&self) -> &str;
    /// 挂载（等价于 cordis apply）
    fn mount(&self, ctx: &PluginContext) -> Result<(), String>;
    /// 卸载
    fn unmount(&self) {}
}

/// 插件 fiber 状态（对齐 FIBER_STATE）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    Pending,
    Loading,
    Active,
    Failed,
    Disposed,
}

impl FiberState {
    pub fn as_str(&self) -> &'static str {
        match self {
            FiberState::Pending => "pending",
            FiberState::Loading => "loading",
            FiberState::Active => "active",
            FiberState::Failed => "failed",
            FiberState::Disposed => "disposed",
        }
    }
}

/// 插件条目（对齐 loader Entry）。
pub struct Entry {
    pub id: String,
    pub plugin: Box<dyn Plugin>,
    pub enabled: bool,
    pub state: FiberState,
}

/// Loader（对齐 cordis Loader）。
#[derive(Default)]
pub struct Loader {
    entries: HashMap<String, Entry>,
    order: Vec<String>,
}

impl Loader {
    /// 注册并挂载插件。
    pub fn mount(&mut self, plugin: Box<dyn Plugin>, bus: &EventBus) -> Result<(), String> {
        let name = plugin.name().to_string();
        if self.entries.contains_key(&name) {
            return Err(format!("duplicate plugin: {name}"));
        }
        let ctx = PluginContext { id: &name, bus };
        let result = plugin.mount(&ctx);
        let state = match &result {
            Ok(()) => FiberState::Active,
            Err(_) => FiberState::Failed,
        };
        self.entries.insert(
            name.clone(),
            Entry {
                id: name.clone(),
                plugin,
                enabled: true,
                state,
            },
        );
        self.order.push(name);
        result
    }

    /// 卸载并移除插件。
    pub fn unmount(&mut self, name: &str) -> Option<Box<dyn Plugin>> {
        let entry = self.entries.remove(name)?;
        entry.plugin.unmount();
        self.order.retain(|n| n != name);
        Some(entry.plugin)
    }

    pub fn disable(&mut self, name: &str) {
        if let Some(e) = self.entries.get_mut(name) {
            e.enabled = false;
        }
    }

    pub fn enable(&mut self, name: &str) {
        if let Some(e) = self.entries.get_mut(name) {
            e.enabled = true;
        }
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    /// 条目列表（loader 顺序，对齐 PluginInventoryGateway.list）。
    pub fn list(&self) -> Vec<&Entry> {
        self.order
            .iter()
            .filter_map(|n| self.entries.get(n))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 插件组（对齐 cordis EntryGroup）：子 loader。
#[derive(Default)]
pub struct PluginGroup {
    pub name: String,
    loader: Loader,
}

impl PluginGroup {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            loader: Loader::default(),
        }
    }
    pub fn mount(&mut self, plugin: Box<dyn Plugin>, bus: &EventBus) -> Result<(), String> {
        self.loader.mount(plugin, bus)
    }
    pub fn loader(&self) -> &Loader {
        &self.loader
    }
}

/// 插件事件（等价于 cordis 事件）。
#[derive(Debug, Clone)]
pub struct PluginEvent {
    pub name: String,
    pub payload: serde_json::Value,
}

/// 事件总线（发布/订阅）。
#[derive(Default)]
pub struct EventBus {
    subscribers: HashMap<String, Vec<Box<dyn Fn(&PluginEvent) + Send + Sync>>>,
}

impl EventBus {
    pub fn on(&mut self, event: &str, f: Box<dyn Fn(&PluginEvent) + Send + Sync>) {
        self.subscribers
            .entry(event.to_string())
            .or_default()
            .push(f);
    }

    pub fn emit(&self, event: &PluginEvent) {
        if let Some(subs) = self.subscribers.get(&event.name) {
            for s in subs {
                s(event);
            }
        }
        info!("plugin event: {}", event.name);
    }
}
