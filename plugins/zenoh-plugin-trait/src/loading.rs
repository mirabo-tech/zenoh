use crate::vtable::*;
use crate::*;
use libloading::Library;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;
use zenoh_util::core::Result as ZResult;
use zenoh_util::{bail, zerror, LibLoader};

pub struct PluginsManager<StartArgs, RunningPlugin> {
    loader: LibLoader,
    plugin_starters: Vec<Box<dyn PluginStarter<StartArgs, RunningPlugin> + Send + Sync>>,
    running_plugins: HashMap<String, (String, RunningPlugin)>,
}

impl<StartArgs: 'static, RunningPlugin: 'static> PluginsManager<StartArgs, RunningPlugin> {
    /// Constructs a new plugin manager.
    pub fn new(loader: LibLoader) -> Self {
        PluginsManager {
            loader,
            plugin_starters: Vec::new(),
            running_plugins: HashMap::new(),
        }
    }

    /// Adds a statically linked plugin to the manager.
    pub fn add_static<
        P: Plugin<StartArgs = StartArgs, RunningPlugin = RunningPlugin> + Send + Sync,
    >(
        mut self,
    ) -> Self {
        let plugin_starter: StaticPlugin<P> = StaticPlugin::new();
        self.plugin_starters.push(Box::new(plugin_starter));
        self
    }

    /// Starts `plugin`.
    ///
    /// `Ok(true)` => plugin was successfully started  
    /// `Ok(false)` => plugin was running already, nothing happened  
    /// `Err(e)` => starting the plugin failed due to `e`
    pub fn start(
        &mut self,
        plugin: &str,
        args: &StartArgs,
    ) -> ZResult<Option<(&str, &RunningPlugin)>> {
        match self.running_plugins.entry(plugin.into()) {
            Entry::Occupied(_) => Ok(None),
            Entry::Vacant(e) => {
                match self.plugin_starters.iter().find(|p| p.name() == plugin) {
                    Some(s) => {
                        let path = s.path();
                        let (_, plugin) = e.insert((path.into(), s.start(args).map_err(|e| zerror!(e => "Failed to load plugin {} (from {})", plugin, path))?));
                        Ok(Some((path, &*plugin)))
                    }
                    None => bail!("Plugin starter for `{}` not found", plugin),
                }
            }
        }
    }

    /// Lazily starts all plugins.
    ///
    /// `Ok(Ok(name))` => plugin `name` was successfully started  
    /// `Ok(Err(name))` => plugin `name` wasn't started because it was already running  
    /// `Err(e)` => Error `e` occured when trying to start plugin `name`
    pub fn start_all<'l>(
        &'l mut self,
        args: &'l StartArgs,
    ) -> impl Iterator<Item = (&str, &str, ZResult<Option<&RunningPlugin>>)> + 'l {
        let PluginsManager {
            plugin_starters,
            running_plugins,
            ..
        } = self;
        plugin_starters.iter().map(move |p| {
            let name = p.name();
            let path = p.path();
            (
                name,
                path,
                match running_plugins.entry(name.into()) {
                    std::collections::hash_map::Entry::Occupied(_) => Ok(None),
                    std::collections::hash_map::Entry::Vacant(e) => match p.start(args) {
                        Ok(p) => Ok(Some(unsafe {
                            std::mem::transmute(&e.insert((path.into(), p)).1)
                        })),
                        Err(e) => Err(e),
                    },
                },
            )
        })
    }

    /// Stops `plugin`, returning `true` if it was indeed running.
    pub fn stop(&mut self, plugin: &str) -> bool {
        let result = self.running_plugins.remove(plugin).is_some();
        self.plugin_starters
            .retain(|p| p.name() == plugin || !p.deletable());
        result
    }

    pub fn available_plugins(&self) -> impl Iterator<Item = &str> {
        self.plugin_starters.iter().map(|p| p.name())
    }
    pub fn running_plugins_info(&self) -> HashMap<&str, &str> {
        let mut result = HashMap::with_capacity(self.running_plugins.len());
        for p in self.plugin_starters.iter() {
            let name = p.name();
            if self.running_plugins.contains_key(name) && !result.contains_key(name) {
                result.insert(name, p.path());
            }
        }
        result
    }
    pub fn running_plugins(&self) -> impl Iterator<Item = (&str, (&str, &RunningPlugin))> {
        self.running_plugins
            .iter()
            .map(|(s, (path, p))| (s.as_str(), (path.as_str(), p)))
    }
    pub fn plugin(&self, name: &str) -> Option<&RunningPlugin> {
        self.running_plugins.get(name).map(|p| &p.1)
    }

    fn load_plugin(
        name: &str,
        lib: Library,
        path: PathBuf,
    ) -> ZResult<DynamicPlugin<StartArgs, RunningPlugin>> {
        DynamicPlugin::new(name.into(), lib, path).map_err(|e|
            zerror!("Wrong PluginVTable version, your {} doesn't appear to be compatible with this version of Zenoh (vtable versions: plugin v{}, zenoh v{})",
                name,
                e.map_or_else(|| "UNKNWON".to_string(), |e| e.to_string()),
                PLUGIN_VTABLE_VERSION).into()
        )
    }

    pub fn load_plugin_by_name(&mut self, name: String) -> ZResult<String> {
        let (lib, p) = unsafe { self.loader.search_and_load(&format!("zplugin_{}", &name))? };
        let plugin = Self::load_plugin(&name, lib, p)?;
        let path = plugin.path().into();
        self.plugin_starters.push(Box::new(plugin));
        Ok(path)
    }
    pub fn load_plugin_by_paths<P: AsRef<str> + std::fmt::Debug>(
        &mut self,
        name: String,
        paths: &[P],
    ) -> ZResult<String> {
        for path in paths {
            let path = path.as_ref();
            match unsafe { LibLoader::load_file(path) } {
                Ok((lib, p)) => {
                    let plugin = Self::load_plugin(&name, lib, p)?;
                    let path = plugin.path().into();
                    self.plugin_starters.push(Box::new(plugin));
                    return Ok(path);
                }
                Err(e) => log::warn!("Plugin '{}' load fail at {}: {}", &name, path, e),
            }
        }
        bail!("Plugin '{}' not found in {:?}", name, &paths)
    }
}

trait PluginStarter<StartArgs, RunningPlugin> {
    fn name(&self) -> &str;
    fn path(&self) -> &str;
    fn start(&self, args: &StartArgs) -> ZResult<RunningPlugin>;
    fn deletable(&self) -> bool;
}

struct StaticPlugin<P> {
    inner: std::marker::PhantomData<P>,
}

impl<P> StaticPlugin<P> {
    fn new() -> Self {
        StaticPlugin {
            inner: std::marker::PhantomData,
        }
    }
}

impl<StartArgs, RunningPlugin, P> PluginStarter<StartArgs, RunningPlugin> for StaticPlugin<P>
where
    P: Plugin<StartArgs = StartArgs, RunningPlugin = RunningPlugin>,
{
    fn name(&self) -> &str {
        P::STATIC_NAME
    }
    fn path(&self) -> &str {
        "<statically_linked>"
    }
    fn start(&self, args: &StartArgs) -> ZResult<RunningPlugin> {
        P::start(P::STATIC_NAME, args)
    }
    fn deletable(&self) -> bool {
        false
    }
}

impl<StartArgs, RunningPlugin> PluginStarter<StartArgs, RunningPlugin>
    for DynamicPlugin<StartArgs, RunningPlugin>
{
    fn name(&self) -> &str {
        &self.name
    }
    fn path(&self) -> &str {
        self.path.to_str().unwrap()
    }
    fn start(&self, args: &StartArgs) -> ZResult<RunningPlugin> {
        self.vtable.start(self.name(), args)
    }
    fn deletable(&self) -> bool {
        true
    }
}

pub struct DynamicPlugin<StartArgs, RunningPlugin> {
    _lib: Library,
    vtable: PluginVTable<StartArgs, RunningPlugin>,
    pub name: String,
    pub path: PathBuf,
}

impl<StartArgs, RunningPlugin> DynamicPlugin<StartArgs, RunningPlugin> {
    fn new(name: String, lib: Library, path: PathBuf) -> Result<Self, Option<PluginVTableVersion>> {
        let load_plugin = unsafe {
            lib.get::<fn(PluginVTableVersion) -> LoadPluginResult<StartArgs, RunningPlugin>>(
                b"load_plugin",
            )
            .map_err(|_| None)?
        };
        match load_plugin(PLUGIN_VTABLE_VERSION) {
            Ok(vtable) => Ok(DynamicPlugin {
                _lib: lib,
                vtable,
                name,
                path,
            }),
            Err(plugin_version) => Err(Some(plugin_version)),
        }
    }
}
