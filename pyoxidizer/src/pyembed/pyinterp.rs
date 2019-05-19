// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use libc::c_char;
use python3_sys as pyffi;
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::ptr::null;

use cpython::{
    GILGuard, NoArgs, ObjectProtocol, PyDict, PyErr, PyList, PyModule, PyObject, PyResult, Python,
    PythonObject, ToPyObject,
};

use super::data::*;
use super::pyalloc::{make_raw_memory_allocator, RawAllocator};
use super::pymodules_module::PyInit__pymodules;
use super::pystr::{osstring_to_bytes, osstring_to_str, OwnedPyStr};

const PYMODULES_NAME: &'static [u8] = b"_pymodules\0";

/// Holds the configuration of an embedded Python interpreter.
pub struct PythonConfig {
    /// Path to the current executable.
    pub exe: PathBuf,
    /// Name of the current program to tell to Python.
    pub program_name: String,
    /// Name of encoding for stdio handles.
    pub standard_io_encoding: Option<String>,
    /// Name of encoding error mode for stdio handles.
    pub standard_io_errors: Option<String>,
    /// Python optimization level.
    pub opt_level: i32,
    /// Whether to load our custom frozen importlib bootstrap modules.
    pub use_custom_importlib: bool,
    /// Filesystem paths to add to sys.path.
    ///
    /// ``.`` will resolve to the path of the application at run-time.
    pub sys_paths: Vec<PathBuf>,
    /// Whether to load the site.py module at initialization time.
    pub import_site: bool,
    /// Whether to load a user-specific site module at initialization time.
    pub import_user_site: bool,
    /// Whether to ignore various PYTHON* environment variables.
    pub ignore_python_env: bool,
    /// Whether to suppress writing of ``.pyc`` files when importing ``.py``
    /// files from the filesystem. This is typically irrelevant since modules
    /// are imported from memory.
    pub dont_write_bytecode: bool,
    /// Whether stdout and stderr streams should be unbuffered.
    pub unbuffered_stdio: bool,
    /// Whether to set sys.argvb with bytes versions of process arguments.
    ///
    /// On Windows, bytes will be UTF-16. On POSIX, bytes will be raw char*
    /// values passed to `int main()`.
    pub argvb: bool,
    /// Whether to use Rust's global memory allocator for the Python raw
    /// memory domain.
    pub rust_allocator_raw: bool,
    /// Environment variable holding the directory to write a loaded modules file.
    ///
    /// If this value is set and the environment it refers to is set,
    /// on interpreter shutdown, we will write a ``modules-<random>`` file to
    /// the directory specified containing a ``\n`` delimited list of modules
    /// loaded in ``sys.modules``.
    pub write_modules_directory_env: Option<String>,
}

impl PythonConfig {
    /// Obtain the PythonConfig with the settings compiled into the binary.
    pub fn default() -> PythonConfig {
        let standard_io_encoding = match STANDARD_IO_ENCODING {
            Some(value) => Some(String::from(value)),
            None => None,
        };

        let standard_io_errors = match STANDARD_IO_ERRORS {
            Some(value) => Some(String::from(value)),
            None => None,
        };

        let write_modules_directory_env = match WRITE_MODULES_DIRECTORY_ENV {
            Some(path) => Some(String::from(path)),
            None => None,
        };

        PythonConfig {
            exe: env::current_exe().unwrap(),
            program_name: PROGRAM_NAME.to_string(),
            standard_io_encoding,
            standard_io_errors,
            opt_level: OPT_LEVEL,
            use_custom_importlib: true,
            sys_paths: vec![],
            import_site: !NO_SITE,
            import_user_site: !NO_USER_SITE_DIRECTORY,
            ignore_python_env: IGNORE_ENVIRONMENT,
            dont_write_bytecode: DONT_WRITE_BYTECODE,
            unbuffered_stdio: UNBUFFERED_STDIO,
            argvb: false,
            rust_allocator_raw: RUST_ALLOCATOR_RAW,
            write_modules_directory_env,
        }
    }
}

fn make_custom_frozen_modules() -> [pyffi::_frozen; 3] {
    [
        pyffi::_frozen {
            name: FROZEN_IMPORTLIB_NAME.as_ptr() as *const i8,
            code: FROZEN_IMPORTLIB_DATA.as_ptr(),
            size: FROZEN_IMPORTLIB_DATA.len() as i32,
        },
        pyffi::_frozen {
            name: FROZEN_IMPORTLIB_EXTERNAL_NAME.as_ptr() as *const i8,
            code: FROZEN_IMPORTLIB_EXTERNAL_DATA.as_ptr(),
            size: FROZEN_IMPORTLIB_EXTERNAL_DATA.len() as i32,
        },
        pyffi::_frozen {
            name: null(),
            code: null(),
            size: 0,
        },
    ]
}

#[cfg(windows)]
extern "C" {
    pub fn __acrt_iob_func(x: libc::uint32_t) -> *mut libc::FILE;
}

#[cfg(windows)]
fn stdin_to_file() -> *mut libc::FILE {
    // The stdin symbol is made available by importing <stdio.h>. On Windows,
    // stdin is defined in corecrt_wstdio.h as a `#define` that calls this
    // internal CRT function. There's no exported symbol to use. So we
    // emulate the behavior of the C code.
    //
    // Relying on an internal CRT symbol is probably wrong. But Microsoft
    // typically keeps backwards compatibility for undocumented functions
    // like this because people use them in the wild.
    //
    // An attempt was made to use fdopen(0) like we do on POSIX. However,
    // this causes a crash. The Microsoft C Runtime is already bending over
    // backwards to coerce its native HANDLEs into POSIX file descriptors.
    // Even if there are other ways to coerce a FILE* from a HANDLE
    // (_open_osfhandle() + _fdopen() might work), using the same function
    // that <stdio.h> uses to obtain a FILE* seems like the least risky thing
    // to do.
    unsafe { __acrt_iob_func(0) }
}

#[cfg(unix)]
fn stdin_to_file() -> *mut libc::FILE {
    unsafe { libc::fdopen(libc::STDIN_FILENO, &('r' as libc::c_char)) }
}

/// Represents an embedded Python interpreter.
///
/// Since the Python API has global state and methods of this mutate global
/// state, there should only be a single instance of this type at any time.
pub struct MainPythonInterpreter<'a> {
    pub config: PythonConfig,
    frozen_modules: [pyffi::_frozen; 3],
    init_run: bool,
    raw_allocator: Option<RawAllocator>,
    gil: Option<GILGuard>,
    py: Option<Python<'a>>,
}

impl<'a> MainPythonInterpreter<'a> {
    /// Construct an instance from a config.
    ///
    /// There are no significant side-effects from calling this.
    pub fn new(config: PythonConfig) -> MainPythonInterpreter<'a> {
        let raw_allocator = if config.rust_allocator_raw {
            Some(make_raw_memory_allocator())
        } else {
            None
        };

        MainPythonInterpreter {
            config,
            frozen_modules: make_custom_frozen_modules(),
            init_run: false,
            raw_allocator,
            gil: None,
            py: None,
        }
    }

    /// Ensure the Python GIL is released.
    pub fn release_gil(&mut self) {
        match self.py {
            Some(_) => {
                self.py = None;
                self.gil = None;
            }
            None => {}
        }
    }

    /// Ensure the Python GIL is acquired, returning a handle on the interpreter.
    pub fn acquire_gil(&mut self) -> Python<'a> {
        match self.py {
            Some(py) => py,
            None => {
                let gil = GILGuard::acquire();
                let py = unsafe { Python::assume_gil_acquired() };

                self.gil = Some(gil);
                self.py = Some(py);

                py
            }
        }
    }

    /// Initialize the interpreter.
    ///
    /// This mutates global state in the Python interpreter according to the
    /// bound config and initializes the Python interpreter.
    ///
    /// After this is called, the embedded Python interpreter is ready to
    /// execute custom code.
    ///
    /// If called more than once, the function is a no-op from the perspective
    /// of interpreter initialization.
    ///
    /// Returns a Python instance which has the GIL acquired.
    pub fn init(&mut self) -> Python {
        // TODO return Result<> and don't panic.
        if self.init_run {
            return self.acquire_gil();
        }

        let config = &self.config;

        if let Some(raw_allocator) = &self.raw_allocator {
            unsafe {
                let ptr = &raw_allocator.allocator as *const _;
                pyffi::PyMem_SetAllocator(
                    pyffi::PyMemAllocatorDomain::PYMEM_DOMAIN_RAW,
                    ptr as *mut _,
                );

                // TODO call this if memory debugging enabled.
                //pyffi::PyMem_SetupDebugHooks();
            }
        }

        if config.use_custom_importlib {
            // Replace the frozen modules in the interpreter with our custom set
            // that knows how to import from memory.
            unsafe {
                pyffi::PyImport_FrozenModules = self.frozen_modules.as_ptr();
            }

            // Register our _pymodules extension which exposes modules data.
            unsafe {
                // name char* needs to live as long as the interpreter is active.
                pyffi::PyImport_AppendInittab(
                    PYMODULES_NAME.as_ptr() as *const i8,
                    Some(PyInit__pymodules),
                );
            }
        }

        let home = OwnedPyStr::from(config.exe.to_str().unwrap());

        unsafe {
            // Pointer needs to live for lifetime of interpreter.
            pyffi::Py_SetPythonHome(home.into());
        }

        let program_name = OwnedPyStr::from(config.program_name.as_str());

        unsafe {
            // Pointer needs to live for lifetime of interpreter.
            pyffi::Py_SetProgramName(program_name.into());
        }

        if let (Some(ref encoding), Some(ref errors)) =
            (&config.standard_io_encoding, &config.standard_io_errors)
        {
            let cencoding = CString::new(encoding.clone()).unwrap();
            let cerrors = CString::new(errors.clone()).unwrap();

            let res = unsafe {
                pyffi::Py_SetStandardStreamEncoding(
                    cencoding.as_ptr() as *const i8,
                    cerrors.as_ptr() as *const i8,
                )
            };

            if res != 0 {
                panic!("unable to set standard stream encoding");
            }
        }

        /*
        // TODO expand "." to the exe's path.
        let paths: Vec<&str> = config.sys_paths.iter().map(|p| p.to_str().unwrap()).collect();
        // TODO use ; on Windows.
        // TODO OwnedPyStr::from("") appears to fail?
        let paths = paths.join(":");
        let path = OwnedPyStr::from(paths.as_str());
        unsafe {
            pyffi::Py_SetPath(path.into());
        }
        */

        unsafe {
            pyffi::Py_DontWriteBytecodeFlag = match config.dont_write_bytecode {
                true => 1,
                false => 0,
            };
        }

        unsafe {
            pyffi::Py_IgnoreEnvironmentFlag = match config.ignore_python_env {
                true => 1,
                false => 0,
            };
        }

        unsafe {
            pyffi::Py_NoSiteFlag = match config.import_site {
                true => 0,
                false => 1,
            };
        }

        unsafe {
            pyffi::Py_NoUserSiteDirectory = match config.import_user_site {
                true => 0,
                false => 1,
            };
        }

        unsafe {
            pyffi::Py_OptimizeFlag = config.opt_level;
        }

        unsafe {
            pyffi::Py_UnbufferedStdioFlag = match config.unbuffered_stdio {
                true => 1,
                false => 0,
            };
        }

        /* Pre-initialization functions we could support:
         *
         * PyObject_SetArenaAllocator()
         * PySys_AddWarnOption()
         * PySys_AddXOption()
         * PySys_ResetWarnOptions()
         */

        unsafe {
            pyffi::Py_Initialize();
        }

        let py = unsafe { Python::assume_gil_acquired() };
        self.py = Some(py);

        self.init_run = true;

        // env::args() panics if arguments aren't valid Unicode. But invalid
        // Unicode arguments are possible and some applications may want to
        // support them.
        //
        // env::args_os() provides access to the raw OsString instances, which
        // will be derived from wchar_t on Windows and char* on POSIX. We can
        // convert these to Python str instances using a platform-specific
        // mechanism.
        let args_objs: Vec<PyObject> = env::args_os()
            .map(|os_arg| osstring_to_str(py, os_arg))
            .collect();

        // This will steal the pointer to the elements and mem::forget them.
        let args = PyList::new(py, &args_objs);
        let argv = b"argv\0";

        let res = args.with_borrowed_ptr(py, |args_ptr| unsafe {
            pyffi::PySys_SetObject(argv.as_ptr() as *const i8, args_ptr)
        });

        match res {
            0 => (),
            _ => panic!("unable to set sys.argv"),
        }

        if config.argvb {
            let args_objs: Vec<PyObject> = env::args_os()
                .map(|os_arg| osstring_to_bytes(py, os_arg))
                .collect();

            let args = PyList::new(py, &args_objs);
            let argvb = b"argvb\0";

            let res = args.with_borrowed_ptr(py, |args_ptr| unsafe {
                pyffi::PySys_SetObject(argvb.as_ptr() as *const i8, args_ptr)
            });

            match res {
                0 => (),
                _ => panic!("unable to set sys.argvb"),
            }
        }

        // As a convention, sys.frozen is set to indicate we are running from
        // a self-contained application.
        let frozen = b"_pymodules\0";

        let res = py.True().with_borrowed_ptr(py, |py_true| unsafe {
            pyffi::PySys_SetObject(frozen.as_ptr() as *const i8, py_true)
        });

        match res {
            0 => (),
            _ => panic!("unable to set sys.frozen"),
        }

        py
    }

    /// Runs the interpreter with the default code execution settings.
    ///
    /// The crate was built with settings that configure what should be
    /// executed by default. Those settings will be loaded and executed.
    pub fn run(&mut self) -> PyResult<PyObject> {
        self.init();

        match RUN_MODE {
            0 => self.run_repl(),
            1 => {
                let name = RUN_MODULE_NAME.expect("RUN_MODULE_NAME should be defined");
                self.run_module_as_main(name)
            }
            2 => {
                let code = RUN_CODE.expect("RUN_CODE should be defined");
                self.run_code(code)
            }
            val => panic!("unhandled run mode: {}", val),
        }
    }

    /// Runs the interpreter and handles any exception that was raised.
    pub fn run_and_handle_error(&mut self) {
        // There are underdefined lifetime bugs at play here. There is no
        // explicit lifetime for the PyObject's returned. If we don't have
        // the local variable in scope, we can get into a situation where
        // drop() on self is called before the PyObject's drop(). This is
        // problematic because PyObject's drop() attempts to acquire the GIL.
        // If the interpreter is shut down, there is no GIL to acquire, and
        // we may segfault.
        // TODO look into setting lifetimes properly so the compiler can
        // prevent some issues.
        let res = self.run();

        match res {
            Ok(_) => {}
            Err(err) => self.print_err(err),
        }
    }

    /// Runs a Python module as the __main__ module.
    ///
    /// Returns the execution result of the module code.
    ///
    /// The interpreter is automatically initialized if needed.
    pub fn run_module_as_main(&mut self, name: &str) -> PyResult<PyObject> {
        let py = self.init();

        // This is modeled after runpy.py:_run_module_as_main().
        let main: PyModule = unsafe {
            PyObject::from_owned_ptr(
                py,
                pyffi::PyImport_AddModule("__main__\0".as_ptr() as *const c_char),
            )
            .cast_into(py)?
        };

        let main_dict = main.dict(py);

        let importlib_util = py.import("importlib.util")?;
        let spec = importlib_util.call(py, "find_spec", (name,), None)?;
        let loader = spec.getattr(py, "loader")?;
        let code = loader.call_method(py, "get_code", (name,), None)?;

        let origin = spec.getattr(py, "origin")?;
        let cached = spec.getattr(py, "cached")?;

        // TODO handle __package__.
        main_dict.set_item(py, "__name__", "__main__")?;
        main_dict.set_item(py, "__file__", origin)?;
        main_dict.set_item(py, "__cached__", cached)?;
        main_dict.set_item(py, "__doc__", py.None())?;
        main_dict.set_item(py, "__loader__", loader)?;
        main_dict.set_item(py, "__spec__", spec)?;

        unsafe {
            let globals = main_dict.as_object().as_ptr();
            let res = pyffi::PyEval_EvalCode(code.as_ptr(), globals, globals);

            if res.is_null() {
                let err = PyErr::fetch(py);
                err.print(py);
                Err(PyErr::fetch(py))
            } else {
                Ok(PyObject::from_owned_ptr(py, res))
            }
        }
    }

    /// Start and run a Python REPL.
    ///
    /// This emulates what CPython's main.c does.
    ///
    /// The interpreter is automatically initialized if needed.
    pub fn run_repl(&mut self) -> PyResult<PyObject> {
        let py = self.init();

        unsafe {
            pyffi::Py_InspectFlag = 0;
        }

        match py.import("readline") {
            Ok(_) => (),
            Err(_) => (),
        };

        let sys = py.import("sys")?;

        match sys.get(py, "__interactivehook__") {
            Ok(hook) => {
                hook.call(py, NoArgs, None)?;
            }
            Err(_) => (),
        };

        let stdin_filename = "<stdin>";
        let filename = CString::new(stdin_filename).expect("could not create CString");
        let mut cf = pyffi::PyCompilerFlags { cf_flags: 0 };

        // TODO use return value.
        unsafe {
            let stdin = stdin_to_file();
            pyffi::PyRun_AnyFileExFlags(stdin, filename.as_ptr() as *const c_char, 0, &mut cf)
        };

        Ok(py.None())
    }

    /// Runs Python code provided by a string.
    ///
    /// This is similar to what ``python -c <code>`` would do.
    ///
    /// The interpreter is automatically initialized if needed.
    pub fn run_code(&mut self, code: &str) -> PyResult<PyObject> {
        let py = self.init();

        let code = CString::new(code).unwrap();

        unsafe {
            let main = pyffi::PyImport_AddModule("__main__\0".as_ptr() as *const _);

            if main.is_null() {
                return Err(PyErr::fetch(py));
            }

            let main_dict = pyffi::PyModule_GetDict(main);

            let res = pyffi::PyRun_StringFlags(
                code.as_ptr() as *const _,
                pyffi::Py_file_input,
                main_dict,
                main_dict,
                0 as *mut _,
            );

            if res.is_null() {
                Err(PyErr::fetch(py))
            } else {
                Ok(PyObject::from_owned_ptr(py, res))
            }
        }
    }

    /// Print a Python error.
    ///
    /// Under the hood this calls ``PyErr_PrintEx()``, which may call
    /// ``Py_Exit()`` and may write to stderr.
    pub fn print_err(&mut self, err: PyErr) {
        let py = self.acquire_gil();
        err.print(py);
    }
}

/// Write loaded Python modules to a directory.
///
/// Given a Python interpreter and a path to a directory, this will create a
/// file in that directory named ``modules-<UUID>`` and write a ``\n`` delimited
/// list of loaded names from ``sys.modules`` into that file.
fn write_modules_to_directory(py: &Python, path: &PathBuf) {
    // TODO this needs better error handling all over.

    fs::create_dir_all(path).expect("could not create directory for modules");

    let rand = uuid::Uuid::new_v4();

    let path = path.join(format!("modules-{}", rand.to_string()));

    let sys = py.import("sys").expect("could not obtain sys module");
    let modules = sys
        .get(*py, "modules")
        .expect("could not obtain sys.modules");

    let modules = modules
        .cast_as::<PyDict>(*py)
        .expect("sys.modules is not a dict");

    let mut f = fs::File::create(path).expect("could not open file for writing");

    for (key, _value) in modules.items(*py) {
        let name = key
            .extract::<String>(*py)
            .expect("module name is not a str");

        f.write_fmt(format_args!("{}\n", name))
            .expect("could not write");
    }
}

impl<'a> Drop for MainPythonInterpreter<'a> {
    fn drop(&mut self) {
        if let Some(key) = &self.config.write_modules_directory_env {
            match env::var(key) {
                Ok(path) => {
                    let path = PathBuf::from(path);
                    let py = self.acquire_gil();
                    write_modules_to_directory(&py, &path);
                }
                Err(_) => {}
            }
        }

        let _ = unsafe { pyffi::Py_FinalizeEx() };
    }
}
