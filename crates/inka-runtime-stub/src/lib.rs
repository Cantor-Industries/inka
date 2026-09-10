use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

fn version_cstr() -> &'static CStr {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| {
        let v = option_env!("INKA_STUB_VERSION").unwrap_or("0.0.0");
        CString::new(format!("inka-stub-runtime {v}")).expect("nul in version")
    })
}

#[no_mangle]
pub extern "C" fn inka_runtime_version() -> *const c_char {
    version_cstr().as_ptr()
}

#[no_mangle]
pub extern "C" fn inka_runtime_create() -> *mut c_void {
    Box::into_raw(Box::new(())) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn inka_runtime_destroy(rt: *mut c_void) {
    if !rt.is_null() {
        drop(Box::from_raw(rt as *mut ()));
    }
}

unsafe fn read_cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn echo_args(argc: c_int, argv: *const *const c_char) {
    if !argv.is_null() && argc > 0 {
        for i in 0..argc as isize {
            let p = *argv.offset(i);
            if !p.is_null() {
                let s = CStr::from_ptr(p).to_string_lossy();
                eprintln!("[inka-stub] argv[{i}] = {s}");
            }
        }
    }
}

fn echo_enabled() -> bool {
    std::env::var_os("INKA_STUB_ECHO").is_some()
}

/// Common entry guard: reset outputs, return false when `exit_code` is null.
unsafe fn init(exit_code: *mut c_int, err_msg: *mut *mut c_char) -> bool {
    if exit_code.is_null() {
        return false;
    }
    *exit_code = 0;
    if !err_msg.is_null() {
        *err_msg = std::ptr::null_mut();
    }
    true
}

#[no_mangle]
pub unsafe extern "C" fn inka_runtime_run_module_perm(
    _rt: *mut c_void,
    specifier: *const c_char,
    source: *const c_char,
    source_len: usize,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
    perms: *const c_char,
) -> c_int {
    if !init(exit_code, err_msg) {
        return -1;
    }
    let spec = read_cstr(specifier);
    let perms = read_cstr(perms);
    let src = if source.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(source as *const u8, source_len)
    };

    if echo_enabled() {
        eprintln!(
            "[inka-stub] run_module_perm(specifier='{spec}', source={} bytes, argc={argc}, perms='{perms}')",
            src.len()
        );
        echo_args(argc, argv);
        eprintln!("[inka-stub] ---- module source ----");
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(src);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn inka_runtime_run_module_dir(
    _rt: *mut c_void,
    dir: *const c_char,
    entry: *const c_char,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
    perms: *const c_char,
) -> c_int {
    if !init(exit_code, err_msg) {
        return -1;
    }
    if echo_enabled() {
        eprintln!(
            "[inka-stub] run_module_dir(dir='{}', entry='{}', argc={argc}, perms='{}')",
            read_cstr(dir),
            read_cstr(entry),
            read_cstr(perms)
        );
        echo_args(argc, argv);
    }
    0
}
