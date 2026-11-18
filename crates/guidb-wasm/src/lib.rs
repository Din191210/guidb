use guidb_shared::sql_gen::{format_sql, preview_mutations};
use guidb_shared::{EngineKind, StagedMutation};
use serde::Deserialize;
use std::alloc::{alloc, dealloc, Layout};
use std::slice;

#[derive(Deserialize)]
struct PreviewInput {
    engine: EngineKind,
    mutations: Vec<StagedMutation>,
}

/// Allocate a linear-memory buffer for host-to-WASM UTF-8 string exchange.
#[no_mangle]
pub extern "C" fn wasm_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let layout = match Layout::from_size_align(len, 1) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { alloc(layout) }
}

/// Free a previously allocated linear-memory buffer.
///
/// # Safety
/// `ptr` must have been returned by `wasm_alloc(len)` and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn wasm_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(len, 1) {
        unsafe { dealloc(ptr, layout) };
    }
}

/// Return a packed u64 `(ptr << 32) | len` pointing to an allocated UTF-8 string.
fn pack_output(text: String) -> u64 {
    let bytes = text.into_bytes();
    let len = bytes.len();
    let out_ptr = wasm_alloc(len);
    if out_ptr.is_null() {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, len);
    }
    ((out_ptr as u64) << 32) | (len as u64)
}

/// Format SQL or JSON query text inside pure Rust WebAssembly.
///
/// # Safety
/// `ptr` must point to at least `len` readable bytes in WASM linear memory.
#[no_mangle]
pub unsafe extern "C" fn wasm_format_sql(ptr: *const u8, len: usize) -> u64 {
    if ptr.is_null() || len == 0 {
        return pack_output(String::new());
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    let input = std::str::from_utf8(bytes).unwrap_or("");
    pack_output(format_sql(input))
}

/// Generate SQL / Document / KV preview statements from staged mutations JSON inside Rust WASM.
///
/// # Safety
/// `ptr` must point to at least `len` readable bytes in WASM linear memory.
#[no_mangle]
pub unsafe extern "C" fn wasm_preview_mutations(ptr: *const u8, len: usize) -> u64 {
    if ptr.is_null() || len == 0 {
        return pack_output("[]".to_string());
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    let input = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return pack_output("[]".to_string()),
    };
    let parsed: PreviewInput = match serde_json::from_str(input) {
        Ok(p) => p,
        Err(e) => return pack_output(format!("[\"Invalid mutation payload: {e}\"]")),
    };
    let stmts = preview_mutations(&parsed.mutations, parsed.engine);
    pack_output(serde_json::to_string(&stmts).unwrap_or_else(|_| "[]".to_string()))
}
