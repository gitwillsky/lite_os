#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(allocator_api)]

extern crate alloc;

mod allocator;
mod cache;
mod ffi;
mod manifest;
mod publisher;
mod runtime;
mod sha256;
mod source;

use core::{ffi::c_int, panic::PanicInfo};

const DEFAULT_APPLICATION: &[u8] = b"/usr/lib/liteui/apps/system-shell\0";

#[unsafe(no_mangle)]
pub extern "C" fn main(argument_count: c_int, arguments: *const *const u8) -> c_int {
    match run(argument_count, arguments) {
        Ok(()) => 0,
        Err(()) => {
            ffi::write_stderr(b"liteui-host: application failed\n");
            1
        }
    }
}

fn run(argument_count: c_int, arguments: *const *const u8) -> Result<(), ()> {
    let directory = if argument_count == 2 && !arguments.is_null() {
        let candidate = unsafe { *arguments.add(1) };
        (!candidate.is_null()).then_some(candidate).ok_or(())?
    } else if argument_count == 1 {
        DEFAULT_APPLICATION.as_ptr()
    } else {
        return Err(());
    };
    let application = source::read_application(directory)?;
    let manifest = manifest::parse(&application.manifest)?;
    let program = application.program.strip_suffix(&[0]).ok_or(())?;
    let mut digest_input = [0u8; 64];
    digest_input[..32].copy_from_slice(&sha256::digest(program));
    digest_input[32..].copy_from_slice(&sha256::digest(&application.styles));
    if sha256::digest(&digest_input) != manifest.bundle_sha256 {
        return Err(());
    }
    let key = cache::Key::try_new(manifest.bundle_sha256)?;
    let publisher = publisher::Publisher::try_connect()?;
    let mut runtime = runtime::Runtime::try_new(manifest.heap_limit, publisher)?;
    if let Some(hit) = key.load()? {
        if runtime.evaluate_bytecode(hit.bytecode()).is_ok() {
            runtime.drain_jobs()?;
            return runtime.run();
        }
        key.remove();
        // The compositor admits one connection per identity. Close the failed engine's
        // publisher before reconnecting so cache recovery cannot look like a duplicate app.
        core::mem::drop(runtime);
        let publisher = publisher::Publisher::try_connect()?;
        runtime = runtime::Runtime::try_new(manifest.heap_limit, publisher)?;
    }
    let bytecode = runtime.compile_and_evaluate(program, application.filename.as_ptr())?;
    let _ = key.store(&bytecode);
    runtime.drain_jobs()?;
    runtime.run()
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    ffi::write_stderr(b"liteui-host: invariant failure\n");
    unsafe { ffi::_exit(125) }
}
