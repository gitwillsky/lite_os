//! Exact private ABI for the vendored QuickJS 2026-06-04 source.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub(super) struct JSRuntime {
    _private: [u8; 0],
}

#[repr(C)]
pub(super) struct JSContext {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) union JSValueUnion {
    pub(super) uint64: u64,
    pub(super) float64: f64,
    pub(super) ptr: *mut c_void,
    pub(super) short_big_int: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct JSValue {
    pub(super) value: JSValueUnion,
    pub(super) tag: i64,
}

pub(super) const JS_TAG_EXCEPTION: i64 = 6;
pub(super) const JS_EVAL_TYPE_MODULE: c_int = 1;
pub(super) const JS_EVAL_FLAG_STRICT: c_int = 1 << 3;

pub(super) type InterruptHandler = unsafe extern "C" fn(*mut JSRuntime, *mut c_void) -> c_int;
pub(super) type PromiseRejectionTracker =
    unsafe extern "C" fn(*mut JSContext, JSValue, JSValue, c_int, *mut c_void);

unsafe extern "C" {
    pub(super) fn JS_NewRuntime() -> *mut JSRuntime;
    pub(super) fn JS_FreeRuntime(runtime: *mut JSRuntime);
    pub(super) fn JS_SetMemoryLimit(runtime: *mut JSRuntime, limit: usize);
    pub(super) fn JS_SetMaxStackSize(runtime: *mut JSRuntime, stack_size: usize);
    pub(super) fn JS_SetInterruptHandler(
        runtime: *mut JSRuntime,
        callback: Option<InterruptHandler>,
        opaque: *mut c_void,
    );
    pub(super) fn JS_SetHostPromiseRejectionTracker(
        runtime: *mut JSRuntime,
        callback: Option<PromiseRejectionTracker>,
        opaque: *mut c_void,
    );
    pub(super) fn JS_NewContext(runtime: *mut JSRuntime) -> *mut JSContext;
    pub(super) fn JS_FreeContext(context: *mut JSContext);
    pub(super) fn JS_Eval(
        context: *mut JSContext,
        source: *const c_char,
        source_len: usize,
        filename: *const c_char,
        flags: c_int,
    ) -> JSValue;
    pub(super) fn JS_GetException(context: *mut JSContext) -> JSValue;
    pub(super) fn JS_ToCStringLen2(
        context: *mut JSContext,
        length: *mut usize,
        value: JSValue,
        cesu8: c_int,
    ) -> *const c_char;
    pub(super) fn JS_FreeCString(context: *mut JSContext, string: *const c_char);
    pub(super) fn lite_qjs_install_bridge(context: *mut JSContext, opaque: *mut c_void) -> c_int;
    pub(super) fn lite_qjs_free_value(context: *mut JSContext, value: JSValue);
    pub(super) fn JS_ExecutePendingJob(
        runtime: *mut JSRuntime,
        context: *mut *mut JSContext,
    ) -> c_int;
}

#[unsafe(no_mangle)]
unsafe extern "C" fn lite_qjs_host_call(
    opaque: *mut c_void,
    operation: *const u8,
    operation_len: usize,
    payload: *const u8,
    payload_len: usize,
    response: *mut *const u8,
    response_len: *mut usize,
) -> c_int {
    // SAFETY: the C bridge supplies the exact callback arguments documented by host_call.
    unsafe {
        super::host_call(
            opaque,
            operation,
            operation_len,
            payload,
            payload_len,
            response,
            response_len,
        )
    }
}

pub(super) unsafe extern "C" fn interrupt(_runtime: *mut JSRuntime, opaque: *mut c_void) -> c_int {
    // SAFETY: QuickJS returns the stable RuntimeState opaque installed by Engine::open.
    unsafe { super::interrupt(opaque) }
}

pub(super) unsafe extern "C" fn track_promise_rejection(
    context: *mut JSContext,
    promise: JSValue,
    reason: JSValue,
    is_handled: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: QuickJS owns all callback values for the duration of this synchronous call.
    unsafe { super::track_promise_rejection(context, promise, reason, is_handled, opaque) }
}
