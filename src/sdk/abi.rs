//! Version 3 C plugin interface. See `include/cog_plugin.h` and `docs/SDK.md`.
//! Only C layouts, opaque call handles and borrowed views cross this boundary.
use super::{Error, Type};
use std::ffi::c_void;

pub type Handle = u64;
pub type Status = u32;
pub const OK: Status = 0;
pub const FAILURE: Status = 1;
pub const INVALID: Status = 2;
pub const TYPE_MISMATCH: Status = 3;
pub const LIMIT: Status = 4;
pub const PANIC: Status = 5;
pub const PENDING: Status = 6;
pub const VERSION: u32 = 3;
pub const ERROR_VALUE: u32 = 12;
pub const OPAQUE_VALUE: u32 = 255;
/// Longest error text the host accepts from `set_error`.
pub(super) const MAX_ERROR_BYTES: usize = 64 * 1024;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Bytes {
    pub data: *const u8,
    pub len: usize,
}
impl Bytes {
    pub const fn new(bytes: &[u8]) -> Self {
        Self {
            data: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
}
// SAFETY: `Bytes` is a copyable descriptor only; it never dereferences its raw
// pointer in safe code. A caller that reads it must honor the ABI's immutable
// storage and lifetime contract. Sharing the descriptor itself grants no access
// to mutate the pointed-to bytes.
unsafe impl Sync for Bytes {}

/// Output from inspect, or input to make. Strings are UTF-8. For arrays make
/// reads `items`; inspect returns the length only (use array_get for elements).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ValueView {
    pub kind: u32,
    pub integer: u64,
    pub number: f64,
    pub bytes: Bytes,
    pub items: *const Handle,
    pub len: usize,
}
impl Default for ValueView {
    fn default() -> Self {
        Self {
            kind: Type::Nil as u32,
            integer: 0,
            number: 0.0,
            bytes: Bytes::new(&[]),
            items: std::ptr::null(),
            len: 0,
        }
    }
}

#[repr(C)]
pub struct HostV3 {
    pub size: u32,
    pub version: u32,
    pub inspect: unsafe extern "C" fn(*mut c_void, Handle, *mut ValueView) -> Status,
    pub make: unsafe extern "C" fn(*mut c_void, *const ValueView, *mut Handle) -> Status,
    pub array_get: unsafe extern "C" fn(*mut c_void, Handle, usize, *mut Handle) -> Status,
    pub array_set: unsafe extern "C" fn(*mut c_void, Handle, usize, Handle) -> Status,
    pub write_output: unsafe extern "C" fn(*mut c_void, Bytes) -> Status,
    pub set_error: unsafe extern "C" fn(*mut c_void, Bytes) -> Status,
    pub wake: unsafe extern "C" fn(*mut c_void) -> Status,
}

pub type CallbackV3 = unsafe extern "C" fn(
    *mut c_void,
    *const HostV3,
    *mut c_void,
    *const Handle,
    usize,
    *mut Handle,
) -> Status;

pub type AsyncStartV3 = unsafe extern "C" fn(
    *mut c_void,
    *const HostV3,
    *mut c_void,
    *mut c_void,
    *const Handle,
    usize,
    *mut *mut c_void,
) -> Status;
pub type AsyncPollV3 = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    *const HostV3,
    *mut c_void,
    *mut Handle,
) -> Status;
pub type AsyncCancelV3 = unsafe extern "C" fn(*mut c_void, *mut c_void);
pub type AsyncDestroyV3 = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[repr(C)]
pub struct FunctionV3 {
    pub size: u32,
    pub name: Bytes,
    pub parameters: Bytes,
    pub result: u8,
    pub asynchronous: u8,
    pub effects: u16,
    pub mutations: u64,
    pub invoke: Option<CallbackV3>,
    pub start: Option<AsyncStartV3>,
    pub poll: Option<AsyncPollV3>,
    pub cancel: Option<AsyncCancelV3>,
    pub destroy_operation: Option<AsyncDestroyV3>,
}
impl FunctionV3 {
    pub const fn new(
        name: &'static str,
        parameters: &'static [Type],
        result: Type,
        effects: super::Effects,
        mutations: u64,
        invoke: CallbackV3,
    ) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            name: Bytes::new(name.as_bytes()),
            parameters: Bytes {
                data: parameters.as_ptr().cast(),
                len: parameters.len(),
            },
            result: result as u8,
            asynchronous: 0,
            effects: effects.bits(),
            mutations,
            invoke: Some(invoke),
            start: None,
            poll: None,
            cancel: None,
            destroy_operation: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub const fn new_async(
        name: &'static str,
        parameters: &'static [Type],
        result: Type,
        effects: super::Effects,
        mutations: u64,
        start: AsyncStartV3,
        poll: AsyncPollV3,
        cancel: AsyncCancelV3,
        destroy_operation: AsyncDestroyV3,
    ) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            name: Bytes::new(name.as_bytes()),
            parameters: Bytes {
                data: parameters.as_ptr().cast(),
                len: parameters.len(),
            },
            result: result as u8,
            asynchronous: 1,
            effects: effects.bits(),
            mutations,
            invoke: None,
            start: Some(start),
            poll: Some(poll),
            cancel: Some(cancel),
            destroy_operation: Some(destroy_operation),
        }
    }
}

#[repr(C)]
pub struct PluginV3 {
    pub size: u32,
    pub version: u32,
    pub function_count: usize,
    pub functions: *const FunctionV3,
    pub create: Option<unsafe extern "C" fn(*mut *mut c_void) -> Status>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}
// SAFETY: plugin authors must keep the descriptor and function table immutable
// and readable until unload; the host retains the library for that full period.
// Safe Rust code treats the raw pointers as opaque and does not dereference them.
unsafe impl Sync for PluginV3 {}
impl PluginV3 {
    pub const fn new(functions: &'static [FunctionV3]) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            version: VERSION,
            function_count: functions.len(),
            functions: functions.as_ptr(),
            create: None,
            destroy: None,
        }
    }
}

/// A borrowed call into the host. Handles and inspected views expire when the
/// callback returns. This wrapper never permits VM reentry or retaining a view.
pub struct PluginCall<'a> {
    host: &'a HostV3,
    context: *mut c_void,
    arguments: &'a [Handle],
}
/// A value view borrowed from one plugin callback. Its raw ABI pointers stay
/// private; byte and string slices borrow this wrapper and cannot escape it.
///
/// ```compile_fail
/// use cog::sdk::abi::{Handle, InspectedValue, PluginCall};
///
/// fn retain_view(call: &PluginCall<'_>, handle: Handle) -> InspectedValue<'static> {
///     call.inspect(handle).unwrap()
/// }
/// ```
pub struct InspectedValue<'call> {
    view: ValueView,
    _call: std::marker::PhantomData<&'call PluginCall<'call>>,
}

impl InspectedValue<'_> {
    pub fn kind(&self) -> u32 {
        self.view.kind
    }

    pub fn integer(&self) -> i64 {
        self.view.integer as i64
    }
    pub fn unsigned_integer(&self) -> u64 {
        self.view.integer
    }

    pub fn character(&self) -> Result<char, Error> {
        if self.view.kind != Type::Char as u32 {
            return Err(Error::new("expected char"));
        }
        let scalar = u32::try_from(self.view.integer).map_err(|_| Error::new("invalid char"))?;
        char::from_u32(scalar).ok_or_else(|| Error::new("invalid char"))
    }

    pub fn number(&self) -> f64 {
        self.view.number
    }

    pub fn len(&self) -> usize {
        if self.view.kind == Type::Str as u32 || self.view.kind == Type::Bytes as u32 {
            self.view.bytes.len
        } else {
            self.view.len
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> Result<&[u8], Error> {
        if self.view.kind != Type::Str as u32 && self.view.kind != Type::Bytes as u32 {
            return Err(Error::new("expected string or bytes"));
        }
        if self.view.bytes.len == 0 {
            return Ok(&[]);
        }
        if self.view.bytes.data.is_null() || self.view.bytes.len > isize::MAX as usize {
            return Err(Error::new("invalid byte view"));
        }
        // SAFETY: the host roots inspected values until the callback returns;
        // this slice borrow is limited by `InspectedValue`'s callback lifetime.
        Ok(unsafe { std::slice::from_raw_parts(self.view.bytes.data, self.view.bytes.len) })
    }

    pub fn string(&self) -> Result<&str, Error> {
        if self.view.kind != Type::Str as u32 {
            return Err(Error::new("expected str"));
        }
        std::str::from_utf8(self.bytes()?).map_err(|_| Error::new("host returned invalid UTF-8"))
    }
}

impl PluginCall<'_> {
    pub fn argument(&self, index: usize) -> Result<Handle, Error> {
        self.arguments
            .get(index)
            .copied()
            .ok_or_else(|| Error::new("missing native argument"))
    }
    fn status(status: Status) -> Result<(), Error> {
        if status == OK {
            Ok(())
        } else {
            Err(Error::new(format!("host operation failed ({status})")))
        }
    }
    fn inspect_raw(&self, value: Handle) -> Result<ValueView, Error> {
        let mut result = ValueView::default();
        // SAFETY: `PluginCall` is created only by `dispatch` from the validated,
        // live host table and callback-scoped context; the host validates handles.
        Self::status(unsafe { (self.host.inspect)(self.context, value, &mut result) })?;
        Ok(result)
    }

    pub fn inspect(&self, value: Handle) -> Result<InspectedValue<'_>, Error> {
        Ok(InspectedValue {
            view: self.inspect_raw(value)?,
            _call: std::marker::PhantomData,
        })
    }

    pub fn int(&self, value: Handle) -> Result<i64, Error> {
        let view = self.inspect(value)?;
        if view.kind() == Type::Int as u32 {
            Ok(view.integer())
        } else {
            Err(Error::new("expected int"))
        }
    }
    pub fn uint(&self, value: Handle) -> Result<u64, Error> {
        let view = self.inspect(value)?;
        if view.kind() == Type::UInt as u32 {
            Ok(view.unsigned_integer())
        } else {
            Err(Error::new("expected uint"))
        }
    }
    pub fn character(&self, value: Handle) -> Result<char, Error> {
        self.inspect(value)?.character()
    }
    pub fn string(&self, value: Handle) -> Result<&str, Error> {
        let view = self.inspect_raw(value)?;
        if view.kind != Type::Str as u32 {
            return Err(Error::new("expected str"));
        }
        if view.bytes.len == 0 {
            return Ok("");
        }
        if view.bytes.data.is_null() || view.bytes.len > isize::MAX as usize {
            return Err(Error::new("invalid byte view"));
        }
        // SAFETY: successful host inspection roots this string through the
        // callback, and the null/length checks above establish a valid byte range.
        let bytes = unsafe { std::slice::from_raw_parts(view.bytes.data, view.bytes.len) };
        std::str::from_utf8(bytes).map_err(|_| Error::new("host returned invalid UTF-8"))
    }
    fn make(&self, value: ValueView) -> Result<Handle, Error> {
        let mut result = 0;
        // SAFETY: the live host table came from `dispatch`; it reads `value`
        // synchronously and writes only to this local output slot.
        Self::status(unsafe { (self.host.make)(self.context, &value, &mut result) })?;
        Ok(result)
    }
    pub fn make_int(&self, value: i64) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Int as u32,
            integer: value as u64,
            ..ValueView::default()
        })
    }
    pub fn make_character(&self, value: char) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Char as u32,
            integer: value as u32 as u64,
            ..ValueView::default()
        })
    }
    pub fn make_nil(&self) -> Result<Handle, Error> {
        self.make(ValueView::default())
    }
    pub fn make_string(&self, value: &str) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Str as u32,
            bytes: Bytes::new(value.as_bytes()),
            ..ValueView::default()
        })
    }
    pub fn make_array(&self, values: &[Handle]) -> Result<Handle, Error> {
        self.make(ValueView {
            kind: Type::Array as u32,
            items: values.as_ptr(),
            len: values.len(),
            ..ValueView::default()
        })
    }
    pub fn array_get(&self, array: Handle, index: usize) -> Result<Handle, Error> {
        let mut result = 0;
        // SAFETY: the live host table came from `dispatch` and the output pointer
        // remains writable for the synchronous callback.
        Self::status(unsafe { (self.host.array_get)(self.context, array, index, &mut result) })?;
        Ok(result)
    }
    pub fn array_set(&self, array: Handle, index: usize, value: Handle) -> Result<(), Error> {
        // SAFETY: the live host table and context are callback-scoped; the host
        // checks both handles and the array mutation contract.
        Self::status(unsafe { (self.host.array_set)(self.context, array, index, value) })
    }
    pub fn write_output(&self, value: &str) -> Result<(), Error> {
        // SAFETY: `value` remains borrowed for the synchronous call and `Bytes`
        // describes its exact UTF-8 byte range.
        Self::status(unsafe {
            (self.host.write_output)(self.context, Bytes::new(value.as_bytes()))
        })
    }
}

/// Invoke a safe Rust plugin callback and catch unwinding panics before the C
/// boundary. The host's normal panic hook may still run. Error text beyond the
/// host's 64 KiB limit is truncated at a character boundary.
///
/// # Safety
/// The host table, context, argument array and output pointer must obey ABI v3
/// and remain valid for this synchronous call. This does not catch aborts/OOM.
pub unsafe fn dispatch(
    host: *const HostV3,
    context: *mut c_void,
    arguments: *const Handle,
    count: usize,
    output: *mut Handle,
    callback: impl FnOnce(&PluginCall<'_>) -> Result<Handle, Error>,
) -> Status {
    if host.is_null()
        || context.is_null()
        || output.is_null()
        || count > 64
        || (count != 0 && arguments.is_null())
    {
        return INVALID;
    }
    // SAFETY: the caller guarantees at least the readable ABI header at `host`;
    // this reads only its first `u32` before checking the declared full-table size.
    let size = unsafe { std::ptr::read(host.cast::<u32>()) };
    if size != size_of::<HostV3>() as u32 {
        return INVALID;
    }
    // SAFETY: the caller guarantees a complete aligned table, and the size check
    // above matches this Rust ABI v3 layout before any field is accessed.
    let host = unsafe { &*host };
    if host.version != VERSION {
        return INVALID;
    }
    let arguments = if count == 0 {
        &[]
    } else {
        // SAFETY: the caller guarantees `count` readable handles; count is capped
        // above and a nonempty slice requires a non-null pointer.
        unsafe { std::slice::from_raw_parts(arguments, count) }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback(&PluginCall {
            host,
            context,
            arguments,
        })
    }));
    match result {
        Ok(Ok(value)) => {
            // SAFETY: the caller guarantees `output` is writable for one handle.
            unsafe {
                output.write(value);
            }
            OK
        }
        Ok(Err(error)) => {
            let message = error.message();
            let message = &message[..message.floor_char_boundary(MAX_ERROR_BYTES)];
            // SAFETY: the validated live host table provides a call-scoped
            // `set_error`; the borrowed message lives through the synchronous call.
            unsafe {
                (host.set_error)(context, Bytes::new(message.as_bytes()));
            }
            FAILURE
        }
        Err(payload) => {
            super::discard_panic(payload);
            // SAFETY: same validated host/context contract as the error path.
            unsafe {
                (host.set_error)(context, Bytes::new(b"Rust plugin callback panicked"));
            }
            PANIC
        }
    }
}

/// Create plugin-owned Rust state without transferring its allocator to the host.
/// Unwinding panics become PANIC; allocation aborts cannot be contained.
///
/// # Safety
/// Output must be writable. Pair success with destroy_state of the same T in the
/// same library; no host code may dereference or free this pointer.
pub unsafe fn create_state<T: Default>(output: *mut *mut c_void) -> Status {
    if output.is_null() {
        return INVALID;
    }
    // SAFETY: the caller's contract guarantees one writable output pointer.
    unsafe {
        output.write(std::ptr::null_mut());
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Box::new(T::default()))) {
        Ok(state) => {
            // SAFETY: the same writable output slot remains valid; ownership of
            // the Box is intentionally transferred to the plugin, not the host.
            unsafe {
                output.write(Box::into_raw(state).cast());
            }
            OK
        }
        Err(payload) => {
            super::discard_panic(payload);
            PANIC
        }
    }
}

/// Destroy plugin state while containing unwinding Drop panics.
///
/// # Safety
/// State must be null or a live pointer returned by `create_state::<T>` in this
/// library. It must be destroyed once, after all calls have finished.
pub unsafe fn destroy_state<T>(state: *mut c_void) {
    if !state.is_null() {
        // SAFETY: the caller guarantees this is the live pointer returned by
        // `create_state::<T>` and that it is destroyed exactly once after use.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(state.cast::<T>()));
        }));
        if let Err(payload) = result {
            super::discard_panic(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspected_empty_strings_accept_null_byte_views() {
        unsafe extern "C" fn inspect(
            _: *mut c_void,
            handle: Handle,
            output: *mut ValueView,
        ) -> Status {
            let bytes = if handle == 0 {
                Bytes {
                    data: std::ptr::null(),
                    len: 0,
                }
            } else {
                Bytes::new("Cog λ".as_bytes())
            };
            unsafe {
                output.write(ValueView {
                    kind: Type::Str as u32,
                    bytes,
                    ..ValueView::default()
                });
            }
            OK
        }
        unsafe extern "C" fn make(_: *mut c_void, _: *const ValueView, _: *mut Handle) -> Status {
            INVALID
        }
        unsafe extern "C" fn get(_: *mut c_void, _: Handle, _: usize, _: *mut Handle) -> Status {
            INVALID
        }
        unsafe extern "C" fn set(_: *mut c_void, _: Handle, _: usize, _: Handle) -> Status {
            INVALID
        }
        unsafe extern "C" fn bytes(_: *mut c_void, _: Bytes) -> Status {
            INVALID
        }
        unsafe extern "C" fn wake(_: *mut c_void) -> Status {
            INVALID
        }
        let host = HostV3 {
            size: size_of::<HostV3>() as u32,
            version: VERSION,
            inspect,
            make,
            array_get: get,
            array_set: set,
            write_output: bytes,
            set_error: bytes,
            wake,
        };
        let mut context = 0u8;
        let call = PluginCall {
            host: &host,
            context: (&mut context as *mut u8).cast(),
            arguments: &[],
        };
        let empty = call.inspect(0).unwrap();
        assert_eq!(empty.string().unwrap(), "");
        assert!(empty.is_empty());
        let text = call.inspect(1).unwrap();
        assert_eq!(text.kind(), Type::Str as u32);
        assert_eq!(text.len(), "Cog λ".len());
        assert_eq!(text.bytes().unwrap(), "Cog λ".as_bytes());
        assert_eq!(text.string().unwrap(), "Cog λ");
        assert_eq!(call.string(1).unwrap(), "Cog λ");
    }

    /// A host table that only records the error text a plugin reports, using
    /// the same 64 KiB bound as the real host.
    fn dispatch_with_recording_host(
        callback: impl FnOnce(&PluginCall<'_>) -> Result<Handle, Error>,
    ) -> (Status, Option<String>) {
        unsafe extern "C" fn inspect(_: *mut c_void, _: Handle, _: *mut ValueView) -> Status {
            INVALID
        }
        unsafe extern "C" fn make(_: *mut c_void, _: *const ValueView, _: *mut Handle) -> Status {
            INVALID
        }
        unsafe extern "C" fn get(_: *mut c_void, _: Handle, _: usize, _: *mut Handle) -> Status {
            INVALID
        }
        unsafe extern "C" fn set(_: *mut c_void, _: Handle, _: usize, _: Handle) -> Status {
            INVALID
        }
        unsafe extern "C" fn output(_: *mut c_void, _: Bytes) -> Status {
            INVALID
        }
        unsafe extern "C" fn set_error(context: *mut c_void, bytes: Bytes) -> Status {
            if bytes.len > 64 * 1024 {
                return LIMIT;
            }
            // SAFETY: `dispatch` passes a live message and the test's own slot.
            let bytes = unsafe { std::slice::from_raw_parts(bytes.data, bytes.len) };
            let message = std::str::from_utf8(bytes).unwrap().to_owned();
            unsafe { *context.cast::<Option<String>>() = Some(message) };
            OK
        }
        unsafe extern "C" fn wake(_: *mut c_void) -> Status {
            INVALID
        }
        let host = HostV3 {
            size: size_of::<HostV3>() as u32,
            version: VERSION,
            inspect,
            make,
            array_get: get,
            array_set: set,
            write_output: output,
            set_error,
            wake,
        };
        let mut message = None::<String>;
        let mut result = 0;
        // SAFETY: the table, context and output slot outlive this call.
        let status = unsafe {
            dispatch(
                &host,
                (&mut message as *mut Option<String>).cast(),
                std::ptr::null(),
                0,
                &mut result,
                callback,
            )
        };
        (status, message)
    }

    struct PanickingPayload;
    impl Drop for PanickingPayload {
        fn drop(&mut self) {
            panic!("payload destructor");
        }
    }

    #[test]
    fn dispatch_contains_panic_payloads_with_panicking_destructors() {
        let (status, message) =
            dispatch_with_recording_host(|_| std::panic::panic_any(PanickingPayload));
        assert_eq!(status, PANIC);
        assert_eq!(message.as_deref(), Some("Rust plugin callback panicked"));
    }

    #[test]
    fn dispatch_truncates_oversized_error_text_on_a_character_boundary() {
        // One ASCII byte shifts every two-byte character across the bound.
        let text = format!("x{}", "\u{3bb}".repeat(64 * 1024));
        let (status, message) = dispatch_with_recording_host(|_| Err(Error::new(text.clone())));
        assert_eq!(status, FAILURE);
        let message = message.expect("a truncated message reaches the host");
        assert_eq!(message.len(), 64 * 1024 - 1);
        assert!(text.starts_with(&message));
    }

    #[test]
    fn rust_state_wrappers_contain_panic_payload_destructors() {
        struct InitBomb;
        impl Default for InitBomb {
            fn default() -> Self {
                std::panic::panic_any(PanickingPayload)
            }
        }
        #[derive(Default)]
        struct DropBomb;
        impl Drop for DropBomb {
            fn drop(&mut self) {
                std::panic::panic_any(PanickingPayload)
            }
        }
        let mut state = std::ptr::null_mut();
        assert_eq!(unsafe { create_state::<InitBomb>(&mut state) }, PANIC);
        assert_eq!(unsafe { create_state::<DropBomb>(&mut state) }, OK);
        unsafe { destroy_state::<DropBomb>(state) };
    }

    #[test]
    fn rust_state_wrappers_contain_initialization_and_destruction_panics() {
        struct InitPanic;
        impl Default for InitPanic {
            fn default() -> Self {
                panic!("initialization test");
            }
        }
        #[derive(Default)]
        struct DropPanic;
        impl Drop for DropPanic {
            fn drop(&mut self) {
                panic!("destruction test");
            }
        }
        let mut state = std::ptr::null_mut();
        assert_eq!(unsafe { create_state::<InitPanic>(&mut state) }, PANIC);
        assert!(state.is_null());
        assert_eq!(unsafe { create_state::<DropPanic>(&mut state) }, OK);
        unsafe {
            destroy_state::<DropPanic>(state);
        }
        assert_eq!(
            unsafe { create_state::<DropPanic>(std::ptr::null_mut()) },
            INVALID
        );
    }
}
