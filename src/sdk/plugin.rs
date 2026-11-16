use super::abi::{self, Bytes, Handle, HostV3, Status, ValueView};
use super::entry_allowed;
use super::{Effects, Engine, Error, ErrorKind, Permissions, Signature, Type};
use super::{NativeContext, Value};
use crate::error::RuntimeError;
use crate::runtime::{AsyncWakeHandle, NativeAsyncFactory, NativeAsyncOperation, RuntimeContext};
use crate::value::Value as VmValue;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const HANDLE_INDEX_BITS: u32 = 12;
const MAX_HANDLES: usize = 1 << HANDLE_INDEX_BITS;
const MAX_PLUGIN_ARGUMENTS: usize = 64;
const MAX_CALL_ID: u64 = u64::MAX >> HANDLE_INDEX_BITS;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
fn failure(message: impl Into<String>) -> Error {
    Error::at(ErrorKind::Native, message)
}

impl Type {
    fn from_tag(tag: u8) -> Result<Self, Error> {
        Ok(match tag {
            0 => Self::Any,
            1 => Self::Int,
            2 => Self::Str,
            3 => Self::Bytes,
            4 => Self::Handle,
            5 => Self::Nil,
            6 => Self::Number,
            7 => Self::Array,
            8 => Self::Record,
            9 => Self::Float,
            10 => Self::Bool,
            11 => Self::StrBuf,
            13 => Self::Char,
            14 => Self::UInt,
            _ => return Err(failure("unknown native type tag")),
        })
    }
    fn from_name(name: &str) -> Result<Self, Error> {
        Ok(match name {
            "any" => Self::Any,
            "int" => Self::Int,
            "str" => Self::Str,
            "bytes" => Self::Bytes,
            "handle" => Self::Handle,
            "nil" => Self::Nil,
            "number" => Self::Number,
            "array" => Self::Array,
            "record" => Self::Record,
            "float" => Self::Float,
            "bool" => Self::Bool,
            "strbuf" => Self::StrBuf,
            "char" => Self::Char,
            _ => return Err(failure("unknown native type name")),
        })
    }
}

/// C ABI v3 native plugin metadata. Parsing and static checking never load
/// native code; the CLI resolves the library relative to its package or manifest.
#[derive(Clone, Debug)]
pub struct PluginManifest {
    /// Legacy single-library path used by external `--plugin` manifests and
    /// older source package manifests.
    pub library: PathBuf,
    /// Exact target selections for portable package plugins.
    pub target_libraries: BTreeMap<String, PathBuf>,
    functions: Vec<(String, Signature)>,
}
impl PluginManifest {
    /// Parse an embedded package plugin declaration with a normalized,
    /// package-relative library path suitable for a future archive.
    pub(crate) fn parse_package_node(node: &kdl::KdlNode) -> Result<Self, Error> {
        let mut property_names = std::collections::BTreeSet::new();
        if node.name().value() != "plugin"
            || node.ty().is_some()
            || node.entries().iter().any(|entry| entry.ty().is_some())
            || node.entries().iter().any(|entry| {
                entry
                    .name()
                    .is_none_or(|name| !matches!(name.value(), "version" | "library"))
                    || !property_names.insert(entry.name().unwrap().value().to_owned())
            })
            || !matches!(node.entries().len(), 1 | 2)
            || node.get("version").and_then(|value| value.as_integer()) != Some(1)
        {
            return Err(failure(
                "package plugin requires plugin version=1 library=\"relative/path\"",
            ));
        }
        let legacy_library = node
            .get("library")
            .and_then(|value| value.as_string())
            .map(PathBuf::from);
        if legacy_library
            .as_ref()
            .is_some_and(|path| !package_library_path(path))
        {
            return Err(failure(
                "package plugin library path must be a normalized path inside the package",
            ));
        }
        let children = node
            .children()
            .ok_or_else(|| failure("package plugin requires function declarations"))?;
        let mut functions = Vec::new();
        let mut target_libraries = BTreeMap::new();
        for child in children.nodes() {
            match child.name().value() {
                "function" if functions.len() < 4096 => {
                    let function = parse_function(child)?;
                    if functions
                        .iter()
                        .any(|(existing, _)| existing == &function.0)
                    {
                        return Err(failure("duplicate native function"));
                    }
                    functions.push(function);
                }
                "library" => {
                    let mut properties = std::collections::BTreeSet::new();
                    let target = child
                        .get("target")
                        .and_then(|value| value.as_string())
                        .ok_or_else(|| failure("target library requires target= and path="))?;
                    let path = child
                        .get("path")
                        .and_then(|value| value.as_string())
                        .filter(|path| !path.is_empty() && !path.contains('\0'))
                        .map(PathBuf::from)
                        .ok_or_else(|| failure("target library requires target= and path="))?;
                    if child.ty().is_some()
                        || child.children().is_some()
                        || child.entries().len() != 2
                        || child.entries().iter().any(|entry| {
                            entry.ty().is_some()
                                || entry.name().is_none_or(|name| {
                                    !matches!(name.value(), "target" | "path")
                                        || !properties.insert(name.value().to_owned())
                                })
                        })
                        || !SUPPORTED_PLUGIN_TARGETS.contains(&target)
                        || !package_library_path(&path)
                    {
                        return Err(failure("invalid target library declaration"));
                    }
                    if target_libraries.insert(target.to_owned(), path).is_some() {
                        return Err(failure("duplicate plugin library target"));
                    }
                }
                _ => return Err(failure("unknown or excessive package plugin metadata")),
            }
        }
        if functions.is_empty() {
            return Err(failure("package plugin requires at least one function"));
        }
        if !target_libraries.is_empty() && legacy_library.is_some() {
            return Err(failure(
                "package plugin cannot combine `library` and target library declarations",
            ));
        }
        if legacy_library.is_none() && target_libraries.is_empty() {
            return Err(failure(
                "package plugin requires a library or target library declarations",
            ));
        }
        Ok(Self {
            library: legacy_library.unwrap_or_default(),
            target_libraries,
            functions,
        })
    }

    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.len() > crate::config::MAX_CONFIGURATION_BYTES {
            return Err(failure("plugin manifest exceeds 256 KiB"));
        }
        crate::limits::check_kdl_depth(text).map_err(failure)?;
        let document =
            kdl::KdlDocument::parse_v2(text).map_err(|error| failure(error.to_string()))?;
        let mut library = None;
        let mut functions = Vec::new();
        for node in document.nodes() {
            if node.ty().is_some() || node.entries().iter().any(|entry| entry.ty().is_some()) {
                return Err(failure("plugin manifest does not accept type annotations"));
            }
            match node.name().value() {
                "plugin" if library.is_none() && node.children().is_none() => {
                    if node.entries().len() != 2
                        || node.get("version").and_then(|v| v.as_integer()) != Some(1)
                    {
                        return Err(failure("plugin requires version=1 and library=\"path\""));
                    }
                    let path = node
                        .get("library")
                        .and_then(|v| v.as_string())
                        .filter(|p| !p.is_empty() && !p.contains('\0'))
                        .ok_or_else(|| failure("plugin requires a library path"))?;
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        return Err(failure(
                            "plugin library path must be relative to the manifest",
                        ));
                    }
                    library = Some(path);
                }
                "function" if functions.len() < 4096 => {
                    let (name, signature) = parse_function(node)?;
                    if functions.iter().any(|(existing, _)| existing == &name) {
                        return Err(failure("duplicate native function"));
                    }
                    functions.push((name, signature));
                }
                _ => return Err(failure("unknown or duplicate plugin manifest node")),
            }
        }
        Ok(Self {
            library: library.ok_or_else(|| failure("missing plugin node"))?,
            target_libraries: BTreeMap::new(),
            functions,
        })
    }
    pub fn read(path: &Path) -> Result<Self, Error> {
        let bytes = crate::limits::read_at_most(
            std::fs::File::open(path).map_err(|e| failure(e.to_string()))?,
            crate::config::MAX_CONFIGURATION_BYTES + 1,
        )
        .map_err(|e| failure(e.to_string()))?;
        Self::parse(std::str::from_utf8(&bytes).map_err(|e| failure(e.to_string()))?)
    }
    pub fn functions(&self) -> &[(String, Signature)] {
        &self.functions
    }
    pub fn library_paths(&self) -> impl Iterator<Item = &Path> {
        if self.target_libraries.is_empty() {
            std::iter::once(self.library.as_path())
                .collect::<Vec<_>>()
                .into_iter()
        } else {
            self.target_libraries
                .values()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>()
                .into_iter()
        }
    }
    pub fn register_metadata(&self, engine: &mut Engine) -> Result<(), Error> {
        let mut next = engine.clone();
        for (name, signature) in &self.functions {
            next.register_metadata(name, signature.clone())?;
        }
        *engine = next;
        Ok(())
    }
    /// Load the library, then check its complete descriptor against this manifest
    /// before registering callbacks. OS library initializers run during loading,
    /// before the descriptor can be checked. Only load trusted native code.
    ///
    /// # Safety
    /// This executes trusted native code with process authority. The library and
    /// all transitive dependencies must obey C ABI v3, including pointer validity,
    /// no unwind across C, thread ownership, async wakeups and operation cleanup.
    pub unsafe fn load(&self, engine: &mut Engine, manifest_directory: &Path) -> Result<(), Error> {
        if self.library.is_absolute() {
            return Err(failure(
                "plugin library path must be relative to the manifest",
            ));
        }
        unsafe {
            load(
                engine,
                &manifest_directory.join(&self.library),
                Some(&self.functions),
            )
        }
    }

    /// Load a package plugin for an exact compilation target. A target bundle
    /// never falls back to another platform's library.
    ///
    /// # Safety
    /// The selected library and its transitive dependencies must be trusted and
    /// obey C ABI v3, including pointer validity, no unwind across C, thread
    /// ownership, async wakeups and operation cleanup.
    pub unsafe fn load_for_target(
        &self,
        engine: &mut Engine,
        package_root: &Path,
        target: &str,
    ) -> Result<(), Error> {
        let library = if self.target_libraries.is_empty() {
            &self.library
        } else {
            self.target_libraries
                .get(target)
                .ok_or_else(|| failure(format!("plugin has no library for target `{target}`")))?
        };
        if !package_library_path(library) {
            return Err(failure(
                "plugin library path must be a normalized path inside the package",
            ));
        }
        unsafe { load(engine, &package_root.join(library), Some(&self.functions)) }
    }
}

pub(crate) const SUPPORTED_PLUGIN_TARGETS: [&str; 3] = [
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
];

fn parse_function(node: &kdl::KdlNode) -> Result<(String, Signature), Error> {
    if node.ty().is_some()
        || node.entries().iter().any(|entry| entry.ty().is_some())
        || node.entries().len() != 2
        || node.entries()[0].name().is_some()
    {
        return Err(failure(
            "function requires a positional name and result=\"type\"",
        ));
    }
    let name = node
        .get(0)
        .and_then(|value| value.as_string())
        .ok_or_else(|| failure("function requires a name"))?;
    let result = node
        .get("result")
        .and_then(|value| value.as_string())
        .ok_or_else(|| failure("function requires a result type"))?;
    let mut signature = Signature::new(Vec::new(), Type::from_name(result)?);
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().value() == "async" {
                if signature.asynchronous
                    || child.ty().is_some()
                    || child.children().is_some()
                    || !child.entries().is_empty()
                {
                    return Err(failure("invalid or duplicate async function marker"));
                }
                signature = signature.with_async();
                continue;
            }
            if child.ty().is_some()
                || child.children().is_some()
                || child.entries().len() != 1
                || child.entries()[0].name().is_some()
                || child.entries()[0].ty().is_some()
            {
                return Err(failure("invalid function metadata"));
            }
            match child.name().value() {
                "param" if signature.parameters.len() < MAX_PLUGIN_ARGUMENTS => {
                    signature.parameters.push(Type::from_name(
                        child
                            .get(0)
                            .and_then(|value| value.as_string())
                            .ok_or_else(|| failure("param requires a type"))?,
                    )?);
                }
                "effect" => {
                    let effect = match child.get(0).and_then(|value| value.as_string()) {
                        Some("output") => Effects::OUTPUT,
                        Some("input") => Effects::INPUT,
                        Some("fs_read") => Effects::FS_READ,
                        Some("fs_write") => Effects::FS_WRITE,
                        Some("process") => Effects::PROCESS,
                        Some("process_info") => Effects::PROCESS_INFO,
                        Some("clock") => Effects::CLOCK,
                        Some("sleep") => Effects::SLEEP,
                        Some("entropy") => Effects::ENTROPY,
                        Some("network") => Effects::NETWORK,
                        _ => return Err(failure("unknown native effect")),
                    };
                    if signature.effects.contains(effect) {
                        return Err(failure("duplicate native effect"));
                    }
                    signature.effects = signature.effects | effect;
                }
                "mutates" => signature.mutating_parameters.push(
                    usize::try_from(
                        child
                            .get(0)
                            .and_then(|value| value.as_integer())
                            .ok_or_else(|| failure("mutates requires an index"))?,
                    )
                    .map_err(|_| failure("invalid mutation index"))?,
                ),
                _ => return Err(failure("unknown or excessive function metadata")),
            }
        }
    }
    signature.identity(name)?;
    Ok((name.to_owned(), signature))
}

fn package_library_path(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        && !path.is_absolute()
        && text
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

struct LibraryState {
    state: *mut c_void,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    _library: libloading::Library,
}

struct PluginAsyncFactory {
    state: Rc<LibraryState>,
    start: abi::AsyncStartV3,
    poll: abi::AsyncPollV3,
    cancel: abi::AsyncCancelV3,
    destroy: abi::AsyncDestroyV3,
    effects: Effects,
}

struct WakeRegistration {
    wake: AsyncWakeHandle,
    active: std::sync::atomic::AtomicBool,
}

struct PluginAsyncOperation {
    state: Rc<LibraryState>,
    operation: *mut c_void,
    poll: abi::AsyncPollV3,
    cancel: abi::AsyncCancelV3,
    destroy: abi::AsyncDestroyV3,
    wake: Box<WakeRegistration>,
    cancelled: bool,
}

fn runtime_plugin_failure(error: Error) -> RuntimeError {
    RuntimeError::NativeFailure {
        message: error.message().to_owned(),
    }
}

impl NativeAsyncFactory for PluginAsyncFactory {
    fn start(
        &self,
        context: &mut RuntimeContext<'_>,
        arguments: &[crate::value::Value],
        wake: AsyncWakeHandle,
    ) -> Result<Box<dyn NativeAsyncOperation>, RuntimeError> {
        if !Permissions::from_runtime(context.capabilities)
            .allowed
            .contains(self.effects)
        {
            return Err(RuntimeError::PermissionDenied {
                op: "native extension",
            });
        }
        check_argument_count(arguments.len()).map_err(runtime_plugin_failure)?;
        let id = next_call_id().map_err(runtime_plugin_failure)?;
        let mut native_context = NativeContext { inner: context };
        let remaining_bytes = native_context.inner.limits.buffer_limit();
        let mut frame = Frame {
            context: &mut native_context,
            id,
            values: Vec::new(),
            error: None,
            remaining_bytes,
        };
        let mut handles = [0; MAX_PLUGIN_ARGUMENTS];
        for (index, value) in arguments.iter().enumerate() {
            handles[index] =
                frame
                    .push(Value(value.clone()))
                    .map_err(|_| RuntimeError::ResourceLimit {
                        resource: "native handle budget",
                    })?;
        }
        let wake = Box::new(WakeRegistration {
            wake,
            active: std::sync::atomic::AtomicBool::new(true),
        });
        let wake_pointer = (&*wake as *const WakeRegistration).cast_mut().cast();
        let mut operation = std::ptr::null_mut();
        let status = unsafe {
            (self.start)(
                self.state.state,
                &HOST,
                (&mut frame as *mut Frame<'_, '_, '_>).cast(),
                wake_pointer,
                handles.as_ptr(),
                arguments.len(),
                &mut operation,
            )
        };
        if status != abi::OK || frame.error.is_some() {
            return Err(runtime_plugin_failure(failure(frame.error.unwrap_or_else(
                || format!("async plugin start failed ({status})"),
            ))));
        }
        Ok(Box::new(PluginAsyncOperation {
            state: self.state.clone(),
            operation,
            poll: self.poll,
            cancel: self.cancel,
            destroy: self.destroy,
            wake,
            cancelled: false,
        }))
    }
}

impl NativeAsyncOperation for PluginAsyncOperation {
    fn poll(
        &mut self,
        context: &mut RuntimeContext<'_>,
    ) -> Result<Option<crate::value::Value>, RuntimeError> {
        let id = next_call_id().map_err(runtime_plugin_failure)?;
        let mut native_context = NativeContext { inner: context };
        let remaining_bytes = native_context.inner.limits.buffer_limit();
        let mut frame = Frame {
            context: &mut native_context,
            id,
            values: Vec::new(),
            error: None,
            remaining_bytes,
        };
        let mut result = 0;
        let status = unsafe {
            (self.poll)(
                self.state.state,
                self.operation,
                &HOST,
                (&mut frame as *mut Frame<'_, '_, '_>).cast(),
                &mut result,
            )
        };
        if status == abi::PENDING && frame.error.is_none() {
            return Ok(None);
        }
        if status != abi::OK || frame.error.is_some() {
            return Err(runtime_plugin_failure(failure(frame.error.unwrap_or_else(
                || format!("async plugin poll failed ({status})"),
            ))));
        }
        frame
            .get(result)
            .cloned()
            .map(|value| Some(value.0))
            .map_err(|_| {
                runtime_plugin_failure(failure(
                    "async plugin returned an invalid or expired value handle",
                ))
            })
    }

    fn cancel(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            self.wake
                .active
                .store(false, std::sync::atomic::Ordering::Release);
            unsafe { (self.cancel)(self.state.state, self.operation) };
        }
    }
}

impl Drop for PluginAsyncOperation {
    fn drop(&mut self) {
        self.wake
            .active
            .store(false, std::sync::atomic::Ordering::Release);
        unsafe { (self.destroy)(self.state.state, self.operation) };
    }
}
impl Drop for LibraryState {
    fn drop(&mut self) {
        // The library field remains alive until after this destructor returns.
        if let Some(destroy) = self.destroy {
            unsafe {
                destroy(self.state);
            }
        }
    }
}

impl Engine {
    /// Load a trusted C ABI v3 plugin and register its descriptor's functions.
    /// Source check/build tools should use PluginManifest metadata instead.
    ///
    /// # Safety
    /// Same requirements as PluginManifest::load. Size/version checks detect
    /// compatibility mistakes; they cannot validate arbitrary native pointers.
    pub unsafe fn load_plugin(&mut self, path: &Path) -> Result<(), Error> {
        unsafe { load(self, path, None) }
    }
}

enum PluginCallbacks {
    Sync(abi::CallbackV3),
    Async {
        start: abi::AsyncStartV3,
        poll: abi::AsyncPollV3,
        cancel: abi::AsyncCancelV3,
        destroy: abi::AsyncDestroyV3,
    },
}

// The unsafe caller guarantees descriptor storage for the lifetime of library.
unsafe fn view<'a>(bytes: Bytes, limit: usize) -> Result<&'a [u8], Error> {
    if bytes.len > limit || (bytes.len != 0 && bytes.data.is_null()) {
        return Err(failure("invalid plugin byte view"));
    }
    if bytes.len == 0 {
        Ok(&[])
    } else {
        Ok(unsafe { std::slice::from_raw_parts(bytes.data, bytes.len) })
    }
}

unsafe fn load(
    engine: &mut Engine,
    path: &Path,
    expected: Option<&[(String, Signature)]>,
) -> Result<(), Error> {
    entry_allowed()?;
    // Bare names must select the explicit manifest/CWD path, without relying
    // on the platform loader's ambient library search directories.
    let path = path
        .canonicalize()
        .map_err(|error| failure(format!("cannot resolve plugin {}: {error}", path.display())))?;
    let library = unsafe { libloading::Library::new(&path) }.map_err(|e| failure(e.to_string()))?;
    let entry: libloading::Symbol<'_, unsafe extern "C" fn() -> *const abi::PluginV3> =
        unsafe { library.get(b"cog_plugin_v3\0") }.map_err(|e| failure(e.to_string()))?;
    let descriptor = unsafe { entry() };
    if descriptor.is_null()
        || unsafe { descriptor.cast::<u32>().read() } != size_of::<abi::PluginV3>() as u32
    {
        return Err(failure("invalid plugin descriptor size"));
    }
    let descriptor = unsafe { &*descriptor };
    if descriptor.version != abi::VERSION
        || descriptor.function_count > 4096
        || (descriptor.function_count != 0 && descriptor.functions.is_null())
        || descriptor.create.is_some() != descriptor.destroy.is_some()
    {
        return Err(failure("unsupported plugin ABI or lifecycle"));
    }
    let mut functions = Vec::new();
    let mut metadata_bytes = 0usize;
    for index in 0..descriptor.function_count {
        let pointer = unsafe { descriptor.functions.add(index) };
        if unsafe { pointer.cast::<u32>().read() } != size_of::<abi::FunctionV3>() as u32 {
            return Err(failure("invalid plugin function descriptor size"));
        }
        let function = unsafe { &*pointer };
        metadata_bytes = metadata_bytes
            .checked_add(function.name.len)
            .and_then(|v| v.checked_add(function.parameters.len))
            .filter(|v| *v <= MAX_METADATA_BYTES)
            .ok_or_else(|| failure("plugin metadata budget exceeded"))?;
        let name = std::str::from_utf8(unsafe { view(function.name, 256)? })
            .map_err(|e| failure(e.to_string()))?
            .to_owned();
        let parameters = unsafe { view(function.parameters, MAX_PLUGIN_ARGUMENTS)? }
            .iter()
            .map(|tag| Type::from_tag(*tag))
            .collect::<Result<Vec<_>, _>>()?;
        if function.asynchronous > 1 {
            return Err(failure("invalid async function marker"));
        }
        let mut signature = Signature::new(parameters, Type::from_tag(function.result)?);
        if function.asynchronous == 1 {
            signature = signature.with_async();
        }
        let signature = signature
            .with_effects(Effects::from_bits(function.effects)?)
            .with_mutating_parameters(
                (0..MAX_PLUGIN_ARGUMENTS)
                    .filter(|index| function.mutations & (1u64 << index) != 0)
                    .collect(),
            );
        signature.identity(&name)?;
        let callbacks = if function.asynchronous == 0 {
            if function.start.is_some()
                || function.poll.is_some()
                || function.cancel.is_some()
                || function.destroy_operation.is_some()
            {
                return Err(failure("sync function has async callbacks"));
            }
            PluginCallbacks::Sync(
                function
                    .invoke
                    .ok_or_else(|| failure("missing native callback"))?,
            )
        } else {
            if function.invoke.is_some() {
                return Err(failure("async function has a sync callback"));
            }
            PluginCallbacks::Async {
                start: function
                    .start
                    .ok_or_else(|| failure("missing async start callback"))?,
                poll: function
                    .poll
                    .ok_or_else(|| failure("missing async poll callback"))?,
                cancel: function
                    .cancel
                    .ok_or_else(|| failure("missing async cancel callback"))?,
                destroy: function
                    .destroy_operation
                    .ok_or_else(|| failure("missing async destroy callback"))?,
            }
        };
        if functions.iter().any(|(existing, _, _)| existing == &name) {
            return Err(failure("duplicate plugin function"));
        }
        functions.push((name, signature, callbacks));
    }
    if let Some(expected) = expected
        && (expected.len() != functions.len()
            || expected.iter().any(|(name, signature)| {
                !functions.iter().any(|(actual, actual_signature, _)| {
                    actual == name
                        && actual_signature.identity(actual).ok() == signature.identity(name).ok()
                })
            }))
    {
        return Err(failure("plugin descriptor does not match the manifest"));
    }
    // Validate the entire registration before executing create or mutating Engine.
    let mut trial = engine.clone();
    for (name, signature, _) in &functions {
        trial.register_metadata(name, signature.clone())?;
    }
    let state = Rc::new(LibraryState {
        state: std::ptr::null_mut(),
        destroy: descriptor.destroy,
        _library: library,
    });
    let mut state = state;
    if let Some(create) = descriptor.create {
        let status = unsafe { create(&mut Rc::get_mut(&mut state).unwrap().state) };
        if status != abi::OK {
            return Err(failure(format!("plugin initialization failed ({status})")));
        }
    }
    let mut next = engine.clone();
    for (name, signature, callbacks) in functions {
        match callbacks {
            PluginCallbacks::Sync(invoke) => {
                let state = state.clone();
                next.register(&name, signature, move |context, arguments| {
                    call(&state, invoke, context, arguments)
                })?;
            }
            PluginCallbacks::Async {
                start,
                poll,
                cancel,
                destroy,
            } => {
                let factory = Rc::new(PluginAsyncFactory {
                    state: state.clone(),
                    start,
                    poll,
                    cancel,
                    destroy,
                    effects: signature.effects,
                });
                next.register_async_plugin(&name, signature, factory)?;
            }
        }
    }
    *engine = next;
    Ok(())
}

thread_local! { static NEXT_CALL: Cell<u64> = const { Cell::new(1) }; }
fn next_call_id() -> Result<u64, Error> {
    NEXT_CALL.with(|next| {
        let id = next.get();
        if id == 0 {
            return Err(failure("native call generation exhausted"));
        }
        next.set(if id == MAX_CALL_ID { 0 } else { id + 1 });
        Ok(id)
    })
}
fn check_argument_count(count: usize) -> Result<(), Error> {
    if count > MAX_PLUGIN_ARGUMENTS {
        Err(failure("native plugin argument limit exceeded"))
    } else {
        Ok(())
    }
}
struct Frame<'a, 'b, 'c> {
    context: &'a mut NativeContext<'b, 'c>,
    id: u64,
    values: Vec<Value>,
    error: Option<String>,
    remaining_bytes: usize,
}
impl Frame<'_, '_, '_> {
    fn get(&self, handle: Handle) -> Result<&Value, Status> {
        if (handle >> HANDLE_INDEX_BITS) != self.id {
            return Err(abi::INVALID);
        }
        let index = (handle & (MAX_HANDLES as u64 - 1)) as usize;
        self.values.get(index).ok_or(abi::INVALID)
    }
    fn push(&mut self, value: Value) -> Result<Handle, Status> {
        if self.values.len() >= MAX_HANDLES {
            return Err(abi::LIMIT);
        }
        self.values.try_reserve(1).map_err(|_| abi::LIMIT)?;
        self.values.push(value);
        Ok((self.id << HANDLE_INDEX_BITS) | (self.values.len() - 1) as u64)
    }
    fn charge(&mut self, bytes: usize) -> Result<(), Status> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes).ok_or(abi::LIMIT)?;
        Ok(())
    }
}

fn call(
    state: &LibraryState,
    invoke: abi::CallbackV3,
    context: &mut NativeContext<'_, '_>,
    arguments: &[Value],
) -> Result<Value, Error> {
    let id = next_call_id()?;
    let remaining_bytes = context.inner.limits.buffer_limit();
    let mut frame = Frame {
        context,
        id,
        values: Vec::new(),
        error: None,
        remaining_bytes,
    };
    check_argument_count(arguments.len())?;
    let mut handles = [0; MAX_PLUGIN_ARGUMENTS];
    for (index, value) in arguments.iter().enumerate() {
        handles[index] = frame
            .push(value.clone())
            .map_err(|_| failure("native handle budget exceeded"))?;
    }
    let mut result = 0;
    let status = unsafe {
        invoke(
            state.state,
            &HOST,
            (&mut frame as *mut Frame<'_, '_, '_>).cast(),
            handles.as_ptr(),
            arguments.len(),
            &mut result,
        )
    };
    if status != abi::OK || frame.error.is_some() {
        return Err(failure(
            frame
                .error
                .unwrap_or_else(|| format!("native plugin failed ({status})")),
        ));
    }
    frame
        .get(result)
        .cloned()
        .map_err(|_| failure("native plugin returned an invalid or expired value handle"))
}

fn guarded(action: impl FnOnce() -> Result<(), Status>) -> Status {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        Ok(Ok(())) => abi::OK,
        Ok(Err(status)) => status,
        Err(_) => abi::PANIC,
    }
}
// A trusted caller may only use this pointer synchronously on the calling thread.
unsafe fn frame<'a>(context: *mut c_void) -> &'a mut Frame<'a, 'a, 'a> {
    unsafe { &mut *context.cast::<Frame<'a, 'a, 'a>>() }
}
unsafe extern "C" fn inspect(
    context: *mut c_void,
    handle: Handle,
    output: *mut ValueView,
) -> Status {
    guarded(|| {
        if context.is_null() || output.is_null() {
            return Err(abi::INVALID);
        }
        let value = unsafe { frame(context) }.get(handle)?;
        let mut view = ValueView::default();
        view.kind = match &value.0 {
            VmValue::Int(v) => {
                view.integer = *v as u64;
                Type::Int as u32
            }
            VmValue::UInt(v) => {
                view.integer = *v;
                Type::UInt as u32
            }
            VmValue::Float(v) => {
                view.number = *v;
                Type::Float as u32
            }
            VmValue::Bool(v) => {
                view.integer = u64::from(*v);
                Type::Bool as u32
            }
            VmValue::Char(v) => {
                view.integer = *v as u32 as u64;
                Type::Char as u32
            }
            VmValue::Bytes(v) => {
                let bytes = v.try_borrow().map_err(|_| abi::INVALID)?;
                // The frame roots this value until the synchronous native callback returns.
                view.bytes = Bytes::new(bytes.as_slice());
                Type::Bytes as u32
            }
            VmValue::Str(v) => {
                view.bytes = Bytes::new(v.as_bytes());
                Type::Str as u32
            }
            VmValue::Nil => Type::Nil as u32,
            VmValue::Array(v) => {
                view.len = v.try_borrow().map_err(|_| abi::INVALID)?.len();
                Type::Array as u32
            }
            VmValue::Error(_) => abi::ERROR_VALUE,
            _ => abi::OPAQUE_VALUE,
        };
        unsafe {
            output.write(view);
        }
        Ok(())
    })
}
unsafe extern "C" fn make(
    context: *mut c_void,
    input: *const ValueView,
    output: *mut Handle,
) -> Status {
    guarded(|| {
        if context.is_null() || input.is_null() || output.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let input = unsafe { &*input };
        let value = match input.kind {
            1 => Value::int(input.integer as i64),
            14 => Value::uint(input.integer),
            9 => Value::float(input.number),
            10 if input.integer == 0 || input.integer == 1 => Value::boolean(input.integer != 0),
            13 => {
                let scalar = u32::try_from(input.integer).map_err(|_| abi::INVALID)?;
                let character = char::from_u32(scalar).ok_or(abi::INVALID)?;
                Value::character(character)
            }
            5 => Value::nil(),
            2 => {
                frame.charge(input.bytes.len)?;
                let bytes =
                    unsafe { view(input.bytes, isize::MAX as usize) }.map_err(|_| abi::INVALID)?;
                Value::string(std::str::from_utf8(bytes).map_err(|_| abi::INVALID)?)
            }
            7 => {
                if input.len > MAX_HANDLES || (input.len != 0 && input.items.is_null()) {
                    return Err(abi::LIMIT);
                }
                frame.charge(
                    input
                        .len
                        .checked_mul(size_of::<Value>())
                        .ok_or(abi::LIMIT)?,
                )?;
                let items = if input.len == 0 {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(input.items, input.len) }
                };
                let mut values = Vec::new();
                values.try_reserve(items.len()).map_err(|_| abi::LIMIT)?;
                for handle in items {
                    values.push(frame.get(*handle)?.clone());
                }
                Value::array(values).map_err(|_| abi::TYPE_MISMATCH)?
            }
            _ => return Err(abi::TYPE_MISMATCH),
        };
        let handle = frame.push(value)?;
        unsafe {
            output.write(handle);
        }
        Ok(())
    })
}
unsafe extern "C" fn array_get(
    context: *mut c_void,
    array: Handle,
    index: usize,
    output: *mut Handle,
) -> Status {
    guarded(|| {
        if context.is_null() || output.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let value = frame
            .get(array)?
            .array_get(index)
            .map_err(|_| abi::INVALID)?;
        let handle = frame.push(value)?;
        unsafe {
            output.write(handle);
        }
        Ok(())
    })
}
unsafe extern "C" fn array_set(
    context: *mut c_void,
    array: Handle,
    index: usize,
    value: Handle,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        frame
            .get(array)?
            .array_set(index, frame.get(value)?.clone())
            .map_err(|_| abi::TYPE_MISMATCH)
    })
}
unsafe extern "C" fn write_output(context: *mut c_void, bytes: Bytes) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let bytes = unsafe { view(bytes, frame.context.inner.limits.buffer_limit()) }
            .map_err(|_| abi::LIMIT)?;
        frame
            .context
            .write_output(std::str::from_utf8(bytes).map_err(|_| abi::INVALID)?)
            .map_err(|_| abi::FAILURE)
    })
}
unsafe extern "C" fn set_error(context: *mut c_void, bytes: Bytes) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let frame = unsafe { frame(context) };
        let bytes = unsafe { view(bytes, 64 * 1024) }.map_err(|_| abi::LIMIT)?;
        frame.error = Some(
            std::str::from_utf8(bytes)
                .map_err(|_| abi::INVALID)?
                .to_owned(),
        );
        Ok(())
    })
}
unsafe extern "C" fn wake(context: *mut c_void) -> Status {
    guarded(|| {
        if context.is_null() {
            return Err(abi::INVALID);
        }
        let registration = unsafe { &*context.cast::<WakeRegistration>() };
        if !registration
            .active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(abi::INVALID);
        }
        registration.wake.wake().map_err(|error| match error {
            RuntimeError::ResourceLimit { .. } => abi::LIMIT,
            _ => abi::INVALID,
        })
    })
}

static HOST: HostV3 = HostV3 {
    size: size_of::<HostV3>() as u32,
    version: abi::VERSION,
    inspect,
    make,
    array_get,
    array_set,
    write_output,
    set_error,
    wake,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_handles_cover_the_full_budget_and_expire_across_calls() {
        let mut output = Vec::new();
        let mut runtime = crate::runtime::RuntimeContext::new(
            &mut output,
            crate::runtime::VmCapabilities::default(),
        );
        let mut context = NativeContext {
            inner: &mut runtime,
        };
        let mut frame = Frame {
            context: &mut context,
            id: u64::from(u32::MAX),
            values: Vec::new(),
            error: None,
            remaining_bytes: usize::MAX,
        };
        for index in 0..MAX_PLUGIN_ARGUMENTS {
            let handle = frame.push(Value::int(index as i64)).unwrap();
            assert!(
                matches!(frame.get(handle).unwrap().0, VmValue::Int(value) if value == index as i64)
            );
        }
        let result = frame.push(Value::int(MAX_PLUGIN_ARGUMENTS as i64)).unwrap();
        assert!(
            matches!(frame.get(result).unwrap().0, VmValue::Int(value) if value == MAX_PLUGIN_ARGUMENTS as i64)
        );
        frame.values.clear();

        let handles = (0..MAX_HANDLES)
            .map(|index| frame.push(Value::int(index as i64)).unwrap())
            .collect::<Vec<_>>();
        for (index, &handle) in handles.iter().enumerate() {
            assert_ne!(handle, 0);
            assert!(
                matches!(frame.get(handle).unwrap().0, VmValue::Int(value) if value == index as i64)
            );
        }
        assert_eq!(frame.push(Value::int(0)), Err(abi::LIMIT));
        assert!(frame.get(0).is_err());
        frame.id += 1;
        assert!(handles.iter().all(|&handle| frame.get(handle).is_err()));
        let last = (MAX_CALL_ID << HANDLE_INDEX_BITS) | (MAX_HANDLES as u64 - 1);
        frame.id = MAX_CALL_ID;
        assert!(
            matches!(frame.get(last).unwrap().0, VmValue::Int(value) if value == (MAX_HANDLES - 1) as i64)
        );
    }

    #[test]
    fn call_generations_continue_past_the_old_u32_boundary() {
        let previous = NEXT_CALL.with(|next| next.replace(u64::from(u32::MAX)));
        assert_eq!(next_call_id().unwrap(), u64::from(u32::MAX));
        assert_eq!(next_call_id().unwrap(), u64::from(u32::MAX) + 1);
        NEXT_CALL.with(|next| next.set(previous));
    }

    #[test]
    fn plugin_argument_limit_matches_the_fixed_handle_array() {
        assert!(check_argument_count(MAX_PLUGIN_ARGUMENTS).is_ok());
        assert!(check_argument_count(MAX_PLUGIN_ARGUMENTS + 1).is_err());
    }

    #[test]
    fn call_generations_remain_exhausted_without_wrapping() {
        let previous = NEXT_CALL.with(|next| next.replace(MAX_CALL_ID));
        assert_eq!(next_call_id().unwrap(), MAX_CALL_ID);
        assert!(next_call_id().is_err());
        assert!(next_call_id().is_err());
        NEXT_CALL.with(|next| next.set(previous));
    }
}
