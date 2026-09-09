#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &str| {
    let _ = ifx_lang::project::parse_manifest(data);
    let result = ifx_lang::analyze(data);
    assert!(result.diagnostics.len() <= 100);
    for diagnostic in result.diagnostics {
        assert!(diagnostic.span.start <= diagnostic.span.end);
        assert!(diagnostic.span.end <= data.len());
    }
});
