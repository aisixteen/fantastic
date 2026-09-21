//! Python-format helpers every fantastic_canvas Rust bundle shares.
//!
//! Python is the canonical runtime, and its refusals, reflect texts and
//! persisted bytes are built from CPython's own formatters: `{x!r}`, `{x}`,
//! truthiness, `json.dumps`, `int()`, `OSError`'s text. A bundle that needs one
//! of those takes it from here, so there is ONE Rust spelling of each, pinned to
//! CPython by the vectors in `tests.rs` (measured with CPython 3.11).

#![deny(missing_docs)]

mod int;
mod json;
mod oserror;
mod pyset;
mod repr;
mod text;
mod unicode;
mod unicode_tables;

pub use int::{py_float, py_int, py_int_or, py_type_name, PyError};
pub use json::py_json_dumps;
pub use oserror::{
    io_error_str, os_error_class, os_error_str, os_error_str2, strerror, utf8_decode_error_str,
};
pub use pyset::py_set_order;
pub use repr::{py_float_repr, py_repr, py_repr_value, py_str, py_truthy};
pub use text::{py_isdecimal, py_isdigit, py_split_whitespace, py_splitlines, py_strip};
pub use unicode::{
    decimal_value, is_alnum, is_digit, is_printable, is_space, transform_decimal_and_space_to_ascii,
};

#[cfg(test)]
mod tests;
