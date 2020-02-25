use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::slice;

use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::function::Function;
use crate::module::{Module, ParsedModule};
use crate::utils::eq_cstr_str;

type PinnedAnyClosure = Pin<Box<dyn core::any::Any + 'static>>;

/// A runtime context for wasm3 modules.
#[derive(Debug)]
pub struct Runtime {
    raw: NonNull<ffi::M3Runtime>,
    environment: Environment,
    // holds all linked closures so that they properly get disposed of when runtime drops
    closure_store: UnsafeCell<Vec<PinnedAnyClosure>>,
}

impl Runtime {
    /// Creates a new runtime with the given stack size in slots.
    ///
    /// # Errors
    ///
    /// This function will error on memory allocation failure.
    pub fn new(environment: &Environment, stack_size: u32) -> Result<Self> {
        unsafe {
            NonNull::new(ffi::m3_NewRuntime(
                environment.as_ptr(),
                stack_size,
                ptr::null_mut(),
            ))
        }
        .ok_or_else(Error::malloc_error)
        .map(|raw| Runtime {
            raw,
            environment: environment.clone(),
            closure_store: UnsafeCell::new(Vec::new()),
        })
    }

    /// Parses and loads a module from bytes.
    pub fn parse_and_load_module<'rt>(&'rt self, bytes: &[u8]) -> Result<Module<'rt>> {
        Module::parse(&self.environment, bytes).and_then(|module| self.load_module(module))
    }

    /// Loads a parsed module returning the module if unsuccessful.
    ///
    /// # Errors
    ///
    /// This function will error if the module's environment differs from the one this runtime uses.
    pub fn load_module<'rt>(&'rt self, module: ParsedModule) -> Result<Module<'rt>> {
        if &self.environment != module.environment() {
            Err(Error::ModuleLoadEnvMismatch)
        } else {
            Error::from_ffi_res(unsafe { ffi::m3_LoadModule(self.raw.as_ptr(), module.as_ptr()) })?;
            let raw = module.as_ptr();
            mem::forget(module);
            Ok(Module::from_raw(self, raw))
        }
    }

    pub(crate) unsafe fn mallocated(&self) -> *mut ffi::M3MemoryHeader {
        self.raw.as_ref().memory.mallocated
    }

    pub(crate) fn rt_error(&self) -> Result<()> {
        unsafe { Error::from_ffi_res(self.raw.as_ref().runtimeError) }
    }

    pub(crate) fn push_closure(&self, closure: PinnedAnyClosure) {
        unsafe { (*self.closure_store.get()).push(closure) };
    }

    /// Looks up a function by the given name in the loaded modules of this runtime.
    /// See [`Module::find_function`] for possible error cases.
    ///
    /// [`Module::find_function`]: ../module/struct.Module.html#method.find_function
    pub fn find_function<'rt, ARGS, RET>(&'rt self, name: &str) -> Result<Function<'rt, ARGS, RET>>
    where
        ARGS: crate::WasmArgs,
        RET: crate::WasmType,
    {
        self.modules()
            .find_map(|module| match module.find_function::<ARGS, RET>(name) {
                res @ Ok(_) | res @ Err(Error::InvalidFunctionSignature) => Some(res),
                _ => None,
            })
            .unwrap_or(Err(Error::FunctionNotFound))
    }

    /// Searches for a module with the given name in the runtime's loaded modules.
    ///
    /// Using this over searching through [`Runtime::modules`] is a bit more efficient as it
    /// works on the underlying CStrings directly and doesn't require an upfront length calculation.
    ///
    /// [`Runtime::modules`]: struct.Runtime.html#method.modules
    pub fn find_module<'rt>(&'rt self, name: &str) -> Result<Module<'rt>> {
        unsafe {
            let mut module = ptr::NonNull::new(self.raw.as_ref().modules);
            while let Some(raw_mod) = module {
                if eq_cstr_str(raw_mod.as_ref().name, name) {
                    return Ok(Module::from_raw(self, raw_mod.as_ptr()));
                }
                module = ptr::NonNull::new(raw_mod.as_ref().next);
            }
            Err(Error::ModuleNotFound)
        }
    }

    /// Returns an iterator over the runtime's loaded modules.
    pub fn modules<'rt>(&'rt self) -> impl Iterator<Item = Module<'rt>> + 'rt {
        // pointer could get invalidated if modules can become unloaded
        // pushing new modules into the runtime while this iterator exists is fine as its backed by a linked list meaning it wont get invalidated.
        let mut module = unsafe { ptr::NonNull::new(self.raw.as_ref().modules) };
        core::iter::from_fn(move || {
            let next = unsafe { module.and_then(|module| ptr::NonNull::new(module.as_ref().next)) };
            mem::replace(&mut module, next).map(|raw| Module::from_raw(self, raw.as_ptr()))
        })
    }

    /// Returns the raw memory of this runtime.
    ///
    /// # Safety
    ///
    /// This function is unsafe because calling a wasm function can still mutate this slice while borrowed.
    pub unsafe fn memory(&self) -> &[u8] {
        let mut size = 0;
        let ptr = ffi::m3_GetMemory(self.raw.as_ptr(), &mut size, 0);
        slice::from_raw_parts(
            if size == 0 || ptr.is_null() {
                ptr::NonNull::dangling().as_ptr()
            } else {
                ptr
            },
            size as usize,
        )
    }

    /// Returns the stack of this runtime.
    ///
    /// # Safety
    ///
    /// This function is unsafe because calling a wasm function can still mutate this slice while borrowed.
    pub unsafe fn stack(&self) -> &[u64] {
        slice::from_raw_parts(
            self.raw.as_ref().stack as ffi::m3stack_t,
            self.raw.as_ref().numStackSlots as usize,
        )
    }

    /// Returns the stack of this runtime.
    ///
    /// # Safety
    ///
    /// This function is unsafe because calling a wasm function can still mutate this slice while borrowed
    /// and because this function allows aliasing to happen if called multiple times.
    // This function should definitely be replaced once a stack api exists in wasm3
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn stack_mut(&self) -> &mut [u64] {
        slice::from_raw_parts_mut(
            self.raw.as_ref().stack as ffi::m3stack_t,
            self.raw.as_ref().numStackSlots as usize,
        )
    }

    pub(crate) fn as_ptr(&self) -> ffi::IM3Runtime {
        self.raw.as_ptr()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe { ffi::m3_FreeRuntime(self.raw.as_ptr()) };
    }
}

#[test]
fn create_and_drop_rt() {
    let env = Environment::new().expect("env alloc failure");
    assert!(Runtime::new(&env, 1024 * 64).is_ok());
}
