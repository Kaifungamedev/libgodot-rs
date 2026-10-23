// Copyright 2026 malakai
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::ffi::{CStr, CString, NulError, c_char, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::sys;

/// Godot can only be set up once per process, even after the first instance
/// is destroyed.
static CREATED: AtomicBool = AtomicBool::new(false);

/// The GDExtension functions the wrapper calls, fetched when Godot runs our
/// initialization function.
static INTERFACE: OnceLock<Interface> = OnceLock::new();

/// A caller-supplied initialization function to forward to, from
/// [`GodotInstance::create_with_init`].
static USER_INIT: OnceLock<sys::GDExtensionInitializationFunction> = OnceLock::new();

/// Big enough for a Variant in both single- and double-precision builds
/// (24 and 40 bytes).
type VariantStorage = MaybeUninit<[u64; 5]>;
/// A StringName is a single pointer.
type StringNameStorage = MaybeUninit<*mut c_void>;

#[derive(Debug)]
pub enum Error {
    /// Godot was already created in this process; it can't be set up twice.
    AlreadyCreated,
    /// An argument contained a NUL byte.
    InvalidArgument(NulError),
    /// `libgodot_create_godot_instance` returned null; Godot prints why.
    CreateFailed,
    /// Godot refused to call a method on the instance.
    CallFailed(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::AlreadyCreated => {
                f.write_str("a Godot instance was already created in this process")
            }
            Error::InvalidArgument(e) => write!(f, "invalid argument: {e}"),
            Error::CreateFailed => f.write_str("Godot failed to create an instance"),
            Error::CallFailed(method) => write!(f, "calling GodotInstance.{method}() failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::InvalidArgument(e) => Some(e),
            _ => None,
        }
    }
}

impl From<NulError> for Error {
    fn from(e: NulError) -> Self {
        Error::InvalidArgument(e)
    }
}

/// An embedded Godot engine.
///
/// Only one can ever exist per process, and it must stay on the thread that
/// created it. Dropping it shuts the engine down.
///
/// ```no_run
/// let mut godot = libgodot::GodotInstance::create(["--headless", "--path", "my_project"])?;
/// godot.run()?;
/// # Ok::<(), libgodot::Error>(())
/// ```
pub struct GodotInstance {
    ptr: sys::GDExtensionObjectPtr,
    /// Godot is single-threaded at this level: !Send and !Sync.
    _not_send: PhantomData<*mut ()>,
}

impl GodotInstance {
    /// Sets up Godot with the given command-line arguments (without the
    /// program name, which is filled in from this process).
    ///
    /// The engine is set up but not started; call [`start`](Self::start) or
    /// [`run`](Self::run).
    pub fn create<I, S>(args: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // SAFETY: no user initialization function is involved.
        unsafe { Self::create_inner(args, None) }
    }

    /// Like [`create`](Self::create), additionally running `init` as the
    /// host's GDExtension initialization function, so the host can register
    /// classes (e.g. godot-rust's entry point).
    ///
    /// # Safety
    ///
    /// `init` must be a correct GDExtension initialization function; Godot
    /// calls it and the callbacks it fills in.
    pub unsafe fn create_with_init<I, S>(
        args: I,
        init: unsafe extern "C" fn(
            sys::GDExtensionInterfaceGetProcAddress,
            sys::GDExtensionClassLibraryPtr,
            *mut sys::GDExtensionInitialization,
        ) -> sys::GDExtensionBool,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        unsafe { Self::create_inner(args, Some(init)) }
    }

    unsafe fn create_inner<I, S>(
        args: I,
        init: sys::GDExtensionInitializationFunction,
    ) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // Convert first, so a bad argument doesn't use up the one creation.
        let program = std::env::args()
            .next()
            .unwrap_or_else(|| "godot".to_string());
        let args: Vec<CString> = std::iter::once(Ok(CString::new(program)?))
            .chain(args.into_iter().map(|a| CString::new(a.as_ref())))
            .collect::<Result<_, _>>()?;

        if CREATED.swap(true, Ordering::SeqCst) {
            return Err(Error::AlreadyCreated);
        }
        // Can't already be set: CREATED guards this.
        let _ = USER_INIT.set(init);

        // Godot may keep pointers into argv for its lifetime, so leak it.
        let mut argv: Vec<*mut c_char> = args.into_iter().map(CString::into_raw).collect();
        argv.push(ptr::null_mut());
        let argc = (argv.len() - 1) as i32;
        let argv = argv.leak();

        let ptr = unsafe {
            sys::libgodot_create_godot_instance(argc, argv.as_mut_ptr(), Some(initialize))
        };
        if ptr.is_null() {
            return Err(Error::CreateFailed);
        }
        Ok(GodotInstance {
            ptr,
            _not_send: PhantomData,
        })
    }

    /// Finishes setup and starts the main loop. Returns false if the engine
    /// couldn't start, e.g. because the project has no main scene.
    pub fn start(&mut self) -> Result<bool, Error> {
        self.call(c"start", "start")
    }

    pub fn is_started(&self) -> Result<bool, Error> {
        self.call(c"is_started", "is_started")
    }

    /// Runs one frame. Returns true once the engine wants to quit.
    pub fn iteration(&mut self) -> Result<bool, Error> {
        self.call(c"iteration", "iteration")
    }

    /// Starts the engine and runs frames until it quits.
    pub fn run(&mut self) -> Result<(), Error> {
        if !self.is_started()? && !self.start()? {
            return Err(Error::CallFailed("start"));
        }
        while !self.iteration()? {}
        Ok(())
    }

    pub fn focus_in(&mut self) -> Result<(), Error> {
        self.call(c"focus_in", "focus_in").map(drop)
    }

    pub fn focus_out(&mut self) -> Result<(), Error> {
        self.call(c"focus_out", "focus_out").map(drop)
    }

    pub fn pause(&mut self) -> Result<(), Error> {
        self.call(c"pause", "pause").map(drop)
    }

    pub fn resume(&mut self) -> Result<(), Error> {
        self.call(c"resume", "resume").map(drop)
    }

    /// The raw `GodotInstance` object, for calls the wrapper doesn't cover.
    pub fn as_ptr(&self) -> sys::GDExtensionObjectPtr {
        self.ptr
    }

    /// Calls an argument-less method by name and returns its result as a bool
    /// (false for void methods).
    fn call(&self, method: &'static CStr, name: &'static str) -> Result<bool, Error> {
        let i = INTERFACE.get().ok_or(Error::CallFailed(name))?;
        unsafe {
            let mut this: VariantStorage = MaybeUninit::uninit();
            (i.variant_from_object)(
                this.as_mut_ptr().cast(),
                (&raw const self.ptr).cast_mut().cast(),
            );

            let mut method_name: StringNameStorage = MaybeUninit::uninit();
            // Not flagged static: static StringNames must never be destroyed,
            // and this one is, below.
            (i.string_name_new_with_latin1_chars)(
                method_name.as_mut_ptr().cast(),
                method.as_ptr(),
                0,
            );

            let mut ret: VariantStorage = MaybeUninit::uninit();
            let mut error = sys::GDExtensionCallError {
                error: 0,
                argument: 0,
                expected: 0,
            };
            (i.variant_call)(
                this.as_mut_ptr().cast(),
                method_name.as_ptr().cast(),
                ptr::null(),
                0,
                ret.as_mut_ptr().cast(),
                &mut error,
            );

            // variant_call always initializes the return Variant (to nil on
            // error), so it needs destroying either way.
            let ok = error.error == sys::GDExtensionCallErrorType_GDEXTENSION_CALL_OK;
            let value = ok && (i.variant_booleanize)(ret.as_ptr().cast()) != 0;
            (i.variant_destroy)(ret.as_mut_ptr().cast());
            (i.string_name_destroy)(method_name.as_mut_ptr().cast());
            (i.variant_destroy)(this.as_mut_ptr().cast());

            if ok {
                Ok(value)
            } else {
                Err(Error::CallFailed(name))
            }
        }
    }
}

impl Drop for GodotInstance {
    fn drop(&mut self) {
        // Stops the main loop and cleans the engine up.
        unsafe { sys::libgodot_destroy_godot_instance(self.ptr) };
    }
}

struct Interface {
    variant_from_object:
        unsafe extern "C" fn(sys::GDExtensionUninitializedVariantPtr, sys::GDExtensionTypePtr),
    variant_call: unsafe extern "C" fn(
        sys::GDExtensionVariantPtr,
        sys::GDExtensionConstStringNamePtr,
        *const sys::GDExtensionConstVariantPtr,
        sys::GDExtensionInt,
        sys::GDExtensionUninitializedVariantPtr,
        *mut sys::GDExtensionCallError,
    ),
    variant_booleanize:
        unsafe extern "C" fn(sys::GDExtensionConstVariantPtr) -> sys::GDExtensionBool,
    variant_destroy: unsafe extern "C" fn(sys::GDExtensionVariantPtr),
    string_name_new_with_latin1_chars: unsafe extern "C" fn(
        sys::GDExtensionUninitializedStringNamePtr,
        *const c_char,
        sys::GDExtensionBool,
    ),
    string_name_destroy: unsafe extern "C" fn(sys::GDExtensionTypePtr),
}

impl Interface {
    /// None if any function is missing.
    unsafe fn load(
        get_proc_address: unsafe extern "C" fn(
            *const c_char,
        ) -> sys::GDExtensionInterfaceFunctionPtr,
    ) -> Option<Self> {
        unsafe {
            // Each lookup returns a generic function pointer; the name fixes
            // its real signature, as declared in gdextension_interface.json.
            macro_rules! get {
                ($name:literal as $ty:ty) => {{
                    let f = get_proc_address($name.as_ptr())?;
                    mem::transmute::<unsafe extern "C" fn(), <$ty as OptionInner>::Inner>(f)
                }};
            }

            let get_from_type: <sys::GDExtensionInterfaceGetVariantFromTypeConstructor as OptionInner>::Inner =
                get!(c"get_variant_from_type_constructor" as sys::GDExtensionInterfaceGetVariantFromTypeConstructor);
            let get_destructor: <sys::GDExtensionInterfaceVariantGetPtrDestructor as OptionInner>::Inner =
                get!(c"variant_get_ptr_destructor" as sys::GDExtensionInterfaceVariantGetPtrDestructor);

            Some(Interface {
                variant_from_object: get_from_type(
                    sys::GDExtensionVariantType_GDEXTENSION_VARIANT_TYPE_OBJECT,
                )?,
                variant_call: get!(c"variant_call" as sys::GDExtensionInterfaceVariantCall),
                variant_booleanize: get!(
                    c"variant_booleanize" as sys::GDExtensionInterfaceVariantBooleanize
                ),
                variant_destroy: get!(
                    c"variant_destroy" as sys::GDExtensionInterfaceVariantDestroy
                ),
                string_name_new_with_latin1_chars: get!(
                    c"string_name_new_with_latin1_chars"
                        as sys::GDExtensionInterfaceStringNameNewWithLatin1Chars
                ),
                string_name_destroy: get_destructor(
                    sys::GDExtensionVariantType_GDEXTENSION_VARIANT_TYPE_STRING_NAME,
                )?,
            })
        }
    }
}

/// bindgen wraps every function pointer type in Option; this names the
/// function pointer inside.
trait OptionInner {
    type Inner;
}

impl<T> OptionInner for Option<T> {
    type Inner = T;
}

/// The host's GDExtension entry point, handed to libgodot: grabs the
/// interface, then defers to the caller's own initialization function if
/// there is one.
unsafe extern "C" fn initialize(
    get_proc_address: sys::GDExtensionInterfaceGetProcAddress,
    library: sys::GDExtensionClassLibraryPtr,
    r_initialization: *mut sys::GDExtensionInitialization,
) -> sys::GDExtensionBool {
    let Some(get) = get_proc_address else {
        return 0;
    };
    let Some(interface) = (unsafe { Interface::load(get) }) else {
        return 0;
    };
    let _ = INTERFACE.set(interface);

    if let Some(Some(user_init)) = USER_INIT.get() {
        return unsafe { user_init(get_proc_address, library, r_initialization) };
    }

    // Godot requires both callbacks even when there is nothing to do.
    unsafe extern "C" fn nothing(_: *mut c_void, _: sys::GDExtensionInitializationLevel) {}
    unsafe {
        *r_initialization = sys::GDExtensionInitialization {
            minimum_initialization_level:
                sys::GDExtensionInitializationLevel_GDEXTENSION_INITIALIZATION_CORE,
            userdata: ptr::null_mut(),
            initialize: Some(nothing),
            deinitialize: Some(nothing),
        };
    }
    1
}
