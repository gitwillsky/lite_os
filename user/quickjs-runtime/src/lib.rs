//! Safe, single-threaded owner for the vendored QuickJS runtime.
//!
//! This crate is the only userspace module allowed to touch the QuickJS C ABI. Callers receive no
//! raw context, runtime or value and cannot extend native capabilities behind LiteUI's back.

mod raw;

use std::{
    cell::{Cell, RefCell},
    error::Error,
    ffi::{CStr, CString, c_int, c_void},
    fmt,
    marker::PhantomData,
    ptr::NonNull,
    rc::Rc,
};

const APP_HEAP_BYTES: usize = 16 * 1024 * 1024;
const DESKTOP_HEAP_BYTES: usize = 32 * 1024 * 1024;
const STACK_BYTES: usize = 512 * 1024;
const INTERRUPT_CHECKS_PER_TURN: usize = 10_000;

/// Immutable VM role selecting the confirmed fixed heap limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Privileged graphical-session desktop.
    Desktop,
    /// One ordinary windowed application.
    App,
}

/// Fatal JavaScript runtime error. LiteUI must log it and terminate the corresponding process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineError {
    message: String,
}

impl EngineError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Creates a fatal error reported by the installed safe native host.
    ///
    /// # Parameters
    ///
    /// - `message`: Stable user-facing diagnostic without a JavaScript stack.
    ///
    /// # Returns
    ///
    /// An error that becomes the current JavaScript exception.
    pub fn from_host(message: impl Into<String>) -> Self {
        Self::new(message)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EngineError {}

struct RuntimeState {
    remaining: Cell<usize>,
    rejections: RefCell<Vec<(usize, String)>>,
    host: RefCell<Option<Box<dyn NativeHost>>>,
    response: RefCell<Vec<u8>>,
}

/// Fixed safe target for the single `globalThis.__liteNative` bridge.
pub trait NativeHost {
    /// Handles one host operation without retaining JavaScript values.
    ///
    /// # Parameters
    ///
    /// - `operation`: Stable LiteUI operation name.
    /// - `payload`: UTF-8 operation payload.
    ///
    /// # Returns
    ///
    /// UTF-8 response returned synchronously to JavaScript.
    ///
    /// # Errors
    ///
    /// Returns a fatal operation error. The JavaScript turn receives an exception.
    fn invoke(&mut self, operation: &str, payload: &str) -> Result<String, EngineError>;
}

/// Single-threaded QuickJS Runtime/Context pair with fixed resource and execution limits.
pub struct Engine {
    runtime: NonNull<raw::JSRuntime>,
    context: NonNull<raw::JSContext>,
    state: Box<RuntimeState>,
    // QuickJS Runtime/Context may only be entered from their owner thread. Rc makes that
    // invariant visible to Rust; without it Engine could be moved to a renderer worker.
    _single_threaded: PhantomData<Rc<()>>,
}

impl Engine {
    /// Creates one Runtime and one Context with fixed role limits.
    ///
    /// # Parameters
    ///
    /// - `role`: Desktop or ordinary app; selects only the fixed JS heap cap.
    ///
    /// # Returns
    ///
    /// A unique engine owner.
    ///
    /// # Errors
    ///
    /// Returns an error when QuickJS cannot allocate the Runtime or Context.
    pub fn open(role: Role) -> Result<Self, EngineError> {
        // SAFETY: JS_NewRuntime has no preconditions and returns a unique owner or null.
        let runtime = NonNull::new(unsafe { raw::JS_NewRuntime() })
            .ok_or_else(|| EngineError::new("QuickJS runtime allocation failed"))?;
        let heap_bytes = match role {
            Role::Desktop => DESKTOP_HEAP_BYTES,
            Role::App => APP_HEAP_BYTES,
        };
        let state = Box::new(RuntimeState {
            remaining: Cell::new(INTERRUPT_CHECKS_PER_TURN),
            rejections: RefCell::new(Vec::new()),
            host: RefCell::new(None),
            response: RefCell::new(Vec::new()),
        });
        // SAFETY: runtime is uniquely owned and the boxed callback opaque address remains stable
        // until Engine::drop clears the runtime after the context is gone.
        unsafe {
            raw::JS_SetMemoryLimit(runtime.as_ptr(), heap_bytes);
            raw::JS_SetMaxStackSize(runtime.as_ptr(), STACK_BYTES);
            raw::JS_SetInterruptHandler(
                runtime.as_ptr(),
                Some(raw::interrupt),
                (&*state as *const RuntimeState).cast_mut().cast(),
            );
            raw::JS_SetHostPromiseRejectionTracker(
                runtime.as_ptr(),
                Some(raw::track_promise_rejection),
                (&*state as *const RuntimeState).cast_mut().cast(),
            );
        }
        // SAFETY: runtime remains live and accepts one new default context.
        let Some(context) = NonNull::new(unsafe { raw::JS_NewContext(runtime.as_ptr()) }) else {
            // SAFETY: context publication failed, so runtime remains the only live C owner.
            unsafe { raw::JS_FreeRuntime(runtime.as_ptr()) };
            return Err(EngineError::new("QuickJS context allocation failed"));
        };
        // SAFETY: the context is live and the RuntimeState address remains stable until after the
        // context is freed. The C bridge stores no Rust reference and resolves it per call.
        if unsafe {
            raw::lite_qjs_install_bridge(
                context.as_ptr(),
                (&*state as *const RuntimeState).cast_mut().cast(),
            )
        } < 0
        {
            // SAFETY: bridge publication failed before Engine was exposed. Context precedes Runtime.
            unsafe {
                raw::JS_FreeContext(context.as_ptr());
                raw::JS_FreeRuntime(runtime.as_ptr());
            }
            return Err(EngineError::new(
                "QuickJS native bridge installation failed",
            ));
        }
        Ok(Self {
            runtime,
            context,
            state,
            _single_threaded: PhantomData,
        })
    }

    /// Installs the one safe native host consumed by the self-contained UI bundle.
    ///
    /// # Parameters
    ///
    /// - `host`: Unique synchronous host operation owner.
    ///
    /// # Returns
    ///
    /// Nothing. A later installation atomically replaces the previous host.
    pub fn install_host(&mut self, host: impl NativeHost + 'static) {
        self.state.host.replace(Some(Box::new(host)));
    }

    /// Evaluates one strict ESM source as a single bounded JavaScript turn.
    ///
    /// # Parameters
    ///
    /// - `name`: Diagnostic module name; interior NUL is rejected.
    /// - `source`: UTF-8 JavaScript source. QuickJS reports invalid source as an exception.
    ///
    /// # Returns
    ///
    /// `()` after the module evaluation value is released.
    ///
    /// # Errors
    ///
    /// Returns a fatal syntax/runtime/interrupt/OOM exception or an invalid module name.
    pub fn evaluate(&mut self, name: &str, source: &[u8]) -> Result<(), EngineError> {
        let name =
            CString::new(name).map_err(|_| EngineError::new("QuickJS module name contains NUL"))?;
        let source = CString::new(source)
            .map_err(|_| EngineError::new("QuickJS module source contains NUL"))?;
        self.begin_turn();
        // SAFETY: context is live; source/name pointers remain valid for the synchronous call.
        let value = unsafe {
            raw::JS_Eval(
                self.context.as_ptr(),
                source.as_ptr(),
                source.as_bytes().len(),
                name.as_ptr(),
                raw::JS_EVAL_TYPE_MODULE | raw::JS_EVAL_FLAG_STRICT,
            )
        };
        self.finish_value(value)?;
        self.take_tracked_rejection()
    }

    /// Drains all currently pending Promise jobs under one shared execution budget.
    ///
    /// # Returns
    ///
    /// The number of completed jobs.
    ///
    /// # Errors
    ///
    /// Returns the fatal exception from the first failed or interrupted job.
    pub fn run_jobs(&mut self) -> Result<usize, EngineError> {
        self.begin_turn();
        let mut completed = 0usize;
        loop {
            let mut context = self.context.as_ptr();
            // SAFETY: runtime is live, pctx is valid output storage, and only this thread enters it.
            let result = unsafe { raw::JS_ExecutePendingJob(self.runtime.as_ptr(), &mut context) };
            match result.cmp(&0) {
                std::cmp::Ordering::Greater => {
                    completed += 1;
                    self.take_tracked_rejection()?;
                }
                std::cmp::Ordering::Equal => {
                    self.take_tracked_rejection()?;
                    return Ok(completed);
                }
                std::cmp::Ordering::Less => return Err(self.take_exception(context)),
            }
        }
    }

    fn begin_turn(&self) {
        self.state.remaining.set(INTERRUPT_CHECKS_PER_TURN);
    }

    fn finish_value(&self, value: raw::JSValue) -> Result<(), EngineError> {
        if value.tag == raw::JS_TAG_EXCEPTION {
            return Err(self.take_exception(self.context.as_ptr()));
        }
        if value.tag < 0 {
            // SAFETY: negative tags are reference-counted QuickJS values produced by this context;
            // this branch consumes the one owned return value exactly once.
            unsafe { raw::lite_qjs_free_value(self.context.as_ptr(), value) };
        }
        Ok(())
    }

    fn take_exception(&self, context: *mut raw::JSContext) -> EngineError {
        // SAFETY: a failed QuickJS operation leaves exactly one exception owned by the context.
        let exception = unsafe { raw::JS_GetException(context) };
        let mut length = 0usize;
        // SAFETY: exception belongs to context and remains live until the final free below.
        let text = unsafe { raw::JS_ToCStringLen2(context, &mut length, exception, 0) };
        let message = if text.is_null() {
            "QuickJS exception could not be formatted".to_owned()
        } else {
            // SAFETY: QuickJS returns a NUL-terminated string valid until JS_FreeCString.
            let message = unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: text was allocated by this context's JS_ToCStringLen2 call.
            unsafe { raw::JS_FreeCString(context, text) };
            message
        };
        if exception.tag < 0 {
            // SAFETY: the exception value was removed from context and is consumed exactly once.
            unsafe { raw::lite_qjs_free_value(context, exception) };
        }
        EngineError::new(message)
    }

    fn take_tracked_rejection(&self) -> Result<(), EngineError> {
        let mut rejections = self.state.rejections.borrow_mut();
        if rejections.is_empty() {
            return Ok(());
        }
        let (_, message) = rejections.remove(0);
        rejections.clear();
        Err(EngineError::new(message))
    }
}

unsafe fn host_call(
    opaque: *mut c_void,
    operation: *const u8,
    operation_len: usize,
    payload: *const u8,
    payload_len: usize,
    response: *mut *const u8,
    response_len: *mut usize,
) -> c_int {
    if opaque.is_null() || response.is_null() || response_len.is_null() {
        return 1;
    }
    // SAFETY: the C bridge forwards the stable RuntimeState opaque installed by Engine::open and
    // valid byte ranges returned by QuickJS string conversion for this synchronous callback.
    let state = unsafe { &*opaque.cast::<RuntimeState>() };
    let operation = unsafe { std::slice::from_raw_parts(operation, operation_len) };
    let payload = unsafe { std::slice::from_raw_parts(payload, payload_len) };
    let operation = std::str::from_utf8(operation);
    let payload = std::str::from_utf8(payload);
    let result = match (operation, payload) {
        (Ok(operation), Ok(payload)) => state
            .host
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| EngineError::new("native host is not installed"))
            .and_then(|host| host.invoke(operation, payload)),
        _ => Err(EngineError::new("native host operation is not UTF-8")),
    };
    let (status, bytes) = match result {
        Ok(value) => (0, value.into_bytes()),
        Err(error) => (1, error.to_string().into_bytes()),
    };
    let mut storage = state.response.borrow_mut();
    *storage = bytes;
    // SAFETY: output pointers were checked above. The response Vec remains stable until the next
    // bridge call, while C copies it into a JS string before returning to JavaScript.
    unsafe {
        *response = storage.as_ptr();
        *response_len = storage.len();
    }
    status
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: Engine is the unique owner. Context must be freed before its parent Runtime;
        // dropping in the opposite order would leave context allocations pointing into freed state.
        unsafe {
            raw::JS_FreeContext(self.context.as_ptr());
            raw::JS_FreeRuntime(self.runtime.as_ptr());
        }
    }
}

unsafe fn interrupt(opaque: *mut c_void) -> c_int {
    // SAFETY: Engine::open registers the stable address of its boxed Budget and keeps it alive until
    // after JS_FreeRuntime, which is the last operation that can invoke this callback.
    let state = unsafe { &*opaque.cast::<RuntimeState>() };
    let remaining = state.remaining.get();
    if remaining == 0 {
        return 1;
    }
    state.remaining.set(remaining - 1);
    0
}

unsafe fn track_promise_rejection(
    context: *mut raw::JSContext,
    promise: raw::JSValue,
    reason: raw::JSValue,
    is_handled: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: both pointers are supplied by the live runtime registered in Engine::open. The
    // callback only snapshots the borrowed reason string and never retains a JSValue.
    let state = unsafe { &*opaque.cast::<RuntimeState>() };
    // SAFETY: promise rejection callbacks always carry an object value; QuickJS's fixed 64-bit
    // JSValue layout stores its stable object identity in the union pointer field.
    let promise_key = unsafe { promise.value.ptr } as usize;
    if is_handled != 0 {
        state
            .rejections
            .borrow_mut()
            .retain(|(key, _)| *key != promise_key);
        return;
    }
    let mut length = 0usize;
    // SAFETY: reason is a callback-borrowed value valid for this synchronous conversion.
    let text = unsafe { raw::JS_ToCStringLen2(context, &mut length, reason, 0) };
    let message = if text.is_null() {
        "unhandled Promise rejection".to_owned()
    } else {
        // SAFETY: QuickJS returns a NUL-terminated string until JS_FreeCString below.
        let message = unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: text came from this context and is consumed once.
        unsafe { raw::JS_FreeCString(context, text) };
        message
    };
    let mut rejections = state.rejections.borrow_mut();
    if let Some((_, existing)) = rejections.iter_mut().find(|(key, _)| *key == promise_key) {
        *existing = message;
    } else {
        rejections.push((promise_key, message));
    }
}

#[cfg(test)]
mod tests {
    use super::{Engine, EngineError, NativeHost, Role};

    struct Echo;

    impl NativeHost for Echo {
        fn invoke(&mut self, operation: &str, payload: &str) -> Result<String, EngineError> {
            (operation == "echo")
                .then(|| payload.to_owned())
                .ok_or_else(|| EngineError::new("unsupported test operation"))
        }
    }

    #[test]
    fn evaluates_module_and_drains_promise_jobs() {
        let mut engine = Engine::open(Role::App).expect("engine must open");
        engine
            .evaluate("app.js", b"Promise.resolve().then(() => 42);")
            .expect("valid module must evaluate");
        assert_eq!(engine.run_jobs().expect("jobs must drain"), 1);
    }

    #[test]
    fn reports_javascript_exception() {
        let mut engine = Engine::open(Role::Desktop).expect("engine must open");
        let error = engine
            .evaluate("desktop.js", b"throw new Error('fatal desktop');")
            .expect_err("throw must fail the turn");
        assert!(error.to_string().contains("fatal desktop"));
    }

    #[test]
    fn interrupts_unbounded_javascript_turn() {
        let mut engine = Engine::open(Role::App).expect("engine must open");
        let error = engine
            .evaluate("loop.js", b"while (true) {}")
            .expect_err("fixed interrupt budget must terminate the turn");
        assert!(error.to_string().contains("interrupted"));
    }

    #[test]
    fn exposes_only_the_fixed_native_bridge() {
        let mut engine = Engine::open(Role::App).expect("engine must open");
        engine.install_host(Echo);
        engine
            .evaluate(
                "bridge.js",
                b"if (__liteNative('echo', 'ready') !== 'ready') throw new Error('bridge');",
            )
            .expect("installed bridge must call its safe host");
    }
}
