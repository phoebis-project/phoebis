//! Compares the `cch=` digits a capture recorded against the digits the signer computes for that same body.
//!
//! The fixture drift check masks the per-capture `cch=` value out of its body comparison, because a fresh capture
//! mints a new `cc_prompt_id` and so a new hash with no protocol change. That leaves the hash itself unchecked, so
//! scripts/build-fixtures.py runs this example once per freshly captured body that carries a stamp.
//!
//! With `--write` it stores the computed digits in the file instead, because the builder redacts a body before it
//! writes the fixture and the stamp has to follow the redacted bytes.

use std::process::ExitCode;

/// Width of the `cch=` field, fixed by the wire format.
const DIGITS: usize = 5;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    let write = args.next_if(|arg| arg == "--write").is_some();
    let Some(path) = args.next() else {
        eprintln!("usage: resign [--write] <body-file>");
        return ExitCode::FAILURE;
    };
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(err) => {
            eprintln!("resign: {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(loc) = phoebis_cch::find_cch(&body) else {
        eprintln!("resign: {path}: the billing block carries no cch= stamp");
        return ExitCode::FAILURE;
    };
    let signed = match phoebis_cch::sign(&body) {
        Ok(signed) => signed,
        Err(err) => {
            eprintln!("resign: {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    // sign() rewrites the digits in place, so the span holds in both buffers.
    let span = loc.digits_start..loc.digits_start + DIGITS;
    let captured = String::from_utf8_lossy(&body[span.clone()]).into_owned();
    let computed = String::from_utf8_lossy(&signed[span]).into_owned();
    if write {
        if captured == computed {
            println!("resign: {path}: unchanged");
            return ExitCode::SUCCESS;
        }
        if let Err(err) = std::fs::write(&path, &signed) {
            eprintln!("resign: {path}: {err}");
            return ExitCode::FAILURE;
        }
        println!("resign: {path}: {captured} -> {computed}");
        return ExitCode::SUCCESS;
    }
    println!("captured={captured} computed={computed}");
    if captured == computed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
