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
pub unsafe extern "C" fn inka_runtime_run_module(
    _rt: *mut c_void,
    specifier: *const c_char,
    source: *const c_char,
    source_len: usize,
    argc: c_int,
    argv: *const *const c_char,
    exit_code: *mut c_int,
    err_msg: *mut *mut c_char,
) -> c_int {
    if exit_code.is_null() {
        return -1;
    }
    *exit_code = 0;
    if !err_msg.is_null() {
        *err_msg = std::ptr::null_mut();
    }

    let spec = if specifier.is_null() {
        String::new()
    } else {
        CStr::from_ptr(specifier).to_string_lossy().into_owned()
    };

    let src = if source.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(source as *const u8, source_len)
    };

    let echo = std::env::var_os("INKA_STUB_ECHO").is_some();
    if echo {
        eprintln!("[inka-stub] run_module(specifier='{spec}', source={} bytes, argc={argc})", src.len());
        if !argv.is_null() && argc > 0 {
            for i in 0..argc as isize {
                let p = *argv.offset(i);
                if !p.is_null() {
                    let s = CStr::from_ptr(p).to_string_lossy();
                    eprintln!("[inka-stub] argv[{i}] = {s}");
                }
            }
        }
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
pub unsafe extern "C" fn inka_runtime_destroy(rt: *mut c_void) {
    if !rt.is_null() {
        drop(Box::from_raw(rt as *mut ()));
    }
}
