//! ron-check-probe 1.0.0 — theodolite checker probe for RON syntax.
//!
//! Built on the `ron` crate (=0.12.3, pinned in Cargo.toml/lock):
//! the same parser every Rust consumer of .ron data files uses, so
//! its verdicts are ground truth by construction. The probe
//! implements the bridge's glslang checker argv contract directly
//! (bin/statix-check is the contract's reference shape):
//!
//!   ron-check-probe -V --target-env vulkan1.3 --error-column \
//!       --stdin -S <stage> -I<dir> -I<root> --depfile <p> -o <p> <path>
//!
//! The file's exact bytes arrive on stdin. A clean parse emits only
//! the framing line; a parse failure emits the parser's spanned
//! error (ron::error::SpannedError: code + span start, 1-based
//! line/col) as one severity-1 line in the glslang info-log
//! grammar, plus the recounted aggregate, and exits 2 (0 = no
//! error diagnostics). `--version` prints the ron pin line the
//! bin/ron-check wrapper composes into the stanza's
//! expect_version.

use std::io::Read;

/// The ron crate pin, kept in lockstep with Cargo.toml's `=`
/// requirement and the committed Cargo.lock (the lock is the
/// proof; this line is what the stanza pins against).
const RON_VERSION: &str = "0.12.3";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("ron {RON_VERSION}");
        return;
    }

    // Parse the glslang-profile argv: --depfile <p> and -o <p> take
    // values; the target is the last bare token (the -I<dir> and
    // flag arguments all start with '-'; --target-env's value is a
    // bare token but the target path follows it and wins).
    let mut depfile: Option<String> = None;
    let mut output_path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--depfile" && i + 1 < args.len() {
            i += 1;
            depfile = Some(args[i].clone());
        } else if a == "-o" && i + 1 < args.len() {
            i += 1;
            output_path = Some(args[i].clone());
        }
        i += 1;
    }

    let mut data = Vec::new();
    if std::io::stdin().read_to_end(&mut data).is_err() {
        // A shape the bridge refuses, rather than a silent zero.
        println!("ron-check-probe: failed to read stdin");
        std::process::exit(1);
    }

    // Depfile naming no local dependencies (a RON parse resolves no
    // includes), and the empty output file, per the contract.
    if let Some(dep) = &depfile {
        let out = output_path.as_deref().unwrap_or("output");
        let _ = std::fs::write(dep, format!("{out}: stdin\n"));
    }
    if let Some(out) = &output_path {
        let _ = std::fs::write(out, b"");
    }

    match ron::Options::default().from_bytes::<ron::Value>(&data) {
        Ok(_) => {
            eprintln!("stdin");
        }
        Err(e) => {
            eprintln!("stdin");
            eprintln!(
                "ERROR: stdin:{}:{}: ron {:?}: {}",
                e.span.start.line, e.span.start.col, e.code, e.code
            );
            eprintln!("ERROR: 1 compilation error.  No code generated.");
            println!("SPIR-V is not generated for failed compile or link");
            std::process::exit(2);
        }
    }
}
