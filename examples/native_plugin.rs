//! Build as a separate cdylib with the command in docs/SDK.md.
use cog::sdk::abi::{self, FunctionV3, Handle, HostV3, PluginV3};
use cog::sdk::{Effects, Type};
use std::ffi::c_void;

unsafe extern "C" fn double(
    _state: *mut c_void,
    host: *const HostV3,
    context: *mut c_void,
    arguments: *const Handle,
    count: usize,
    output: *mut Handle,
) -> abi::Status {
    unsafe {
        abi::dispatch(host, context, arguments, count, output, |call| {
            let value = call
                .int(call.argument(0)?)?
                .checked_mul(2)
                .ok_or_else(|| cog::sdk::Error::new("integer overflow"))?;
            call.make_int(value)
        })
    }
}

unsafe extern "C" fn fail(
    _state: *mut c_void,
    host: *const HostV3,
    context: *mut c_void,
    arguments: *const Handle,
    count: usize,
    output: *mut Handle,
) -> abi::Status {
    unsafe {
        abi::dispatch(host, context, arguments, count, output, |_| {
            panic!("example plugin panic");
        })
    }
}

static FUNCTIONS: [FunctionV3; 2] = [
    FunctionV3::new(
        "host::double",
        &[Type::Int],
        Type::Int,
        Effects::PURE,
        0,
        double,
    ),
    FunctionV3::new("host::fail", &[], Type::Nil, Effects::PURE, 0, fail),
];
static PLUGIN: PluginV3 = PluginV3::new(&FUNCTIONS);

#[unsafe(no_mangle)]
pub extern "C" fn cog_plugin_v3() -> *const PluginV3 {
    &PLUGIN
}
