use alloc::{boxed::Box, vec::Vec};
use core::{ffi::c_void, ptr, slice};

use crate::{
    ffi,
    publisher::{Event, Publisher},
};

const STACK_LIMIT: usize = 512 * 1024;
const MAX_TRANSACTION_BYTES: usize = 256 * 1024;
const MAX_TRANSACTION_OPERATIONS: u32 = 256;
const STARTUP_DEADLINE_MS: u32 = 100;
const JOB_DEADLINE_MS: u32 = 4;
const MAX_PENDING_JOBS: u32 = 1024;
const MAX_BYTECODE_BYTES: usize = 2 * 1024 * 1024;

pub struct Runtime {
    engine: *mut ffi::LiteJs,
    publisher: Box<Publisher>,
}

impl Runtime {
    pub fn try_new(heap_limit: usize, mut publisher: Box<Publisher>) -> Result<Self, ()> {
        let engine = unsafe {
            ffi::litejs_create(
                heap_limit,
                STACK_LIMIT,
                (&mut *publisher as *mut Publisher).cast(),
                commit,
            )
        };
        (!engine.is_null())
            .then_some(Self { engine, publisher })
            .ok_or(())
    }

    pub fn compile_and_evaluate(
        &mut self,
        source: &[u8],
        filename: *const u8,
    ) -> Result<Vec<u8>, ()> {
        let mut error = [0u8; 1024];
        let mut bytecode = ptr::null_mut();
        let mut bytecode_length = 0;
        let result = unsafe {
            ffi::litejs_compile_module(
                self.engine,
                source.as_ptr(),
                source.len(),
                filename.cast(),
                STARTUP_DEADLINE_MS,
                &mut bytecode,
                &mut bytecode_length,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        report(result, &error)?;
        if bytecode.is_null() || bytecode_length == 0 || bytecode_length > MAX_BYTECODE_BYTES {
            unsafe { ffi::litejs_free_buffer(self.engine, bytecode) };
            return Err(());
        }
        let bytes = unsafe { slice::from_raw_parts(bytecode, bytecode_length) };
        let mut owned = Vec::new();
        let copy = owned
            .try_reserve_exact(bytecode_length)
            .map(|()| owned.extend_from_slice(bytes))
            .map_err(|_| ());
        unsafe { ffi::litejs_free_buffer(self.engine, bytecode) };
        copy.map(|()| owned)
    }

    pub fn evaluate_bytecode(&mut self, bytecode: &[u8]) -> Result<(), ()> {
        let mut error = [0u8; 1024];
        let result = unsafe {
            ffi::litejs_eval_bytecode(
                self.engine,
                bytecode.as_ptr(),
                bytecode.len(),
                STARTUP_DEADLINE_MS,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        report(result, &error)
    }

    pub fn drain_jobs(&mut self) -> Result<(), ()> {
        let mut error = [0u8; 1024];
        let result = unsafe {
            ffi::litejs_execute_jobs(
                self.engine,
                MAX_PENDING_JOBS,
                JOB_DEADLINE_MS,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        report(result, &error)
    }

    pub fn run(&mut self) -> Result<(), ()> {
        loop {
            let event = self.publisher.next_event()?;
            self.dispatch(event)?;
            self.drain_jobs()?;
        }
    }

    fn dispatch(&mut self, event: Event) -> Result<(), ()> {
        let mut error = [0u8; 1024];
        let result = unsafe {
            ffi::litejs_dispatch_click(
                self.engine,
                event.node,
                event.generation,
                JOB_DEADLINE_MS,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        report(result, &error)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe { ffi::litejs_destroy(self.engine) }
    }
}

unsafe extern "C" fn commit(
    opaque: *mut c_void,
    bytes: *const u8,
    length: usize,
    operations: u32,
) -> i32 {
    if opaque.is_null()
        || bytes.is_null()
        || length > MAX_TRANSACTION_BYTES
        || operations > MAX_TRANSACTION_OPERATIONS
    {
        return -1;
    }
    // SAFETY: bridge 只在同步 JS callback lifetime 内借出 TypedArray bytes。
    let transaction = unsafe { slice::from_raw_parts(bytes, length) };
    unsafe { &mut *opaque.cast::<Publisher>() }.queue(transaction, operations)
}

fn report(result: i32, error: &[u8]) -> Result<(), ()> {
    if result == 0 {
        return Ok(());
    }
    let length = error
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(error.len());
    if length != 0 {
        ffi::write_stderr(&error[..length]);
        ffi::write_stderr(b"\n");
    }
    Err(())
}
