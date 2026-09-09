#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &str| {
    let _ = ifx_lang::project::parse_manifest(data);
    let next = ifx_lang::authoring::analyze("main", &std::collections::BTreeMap::from([("main".into(),data.into())]), &Default::default(), &Default::default());
    assert!(next.diagnostics.len() <= 101);
    for d in next.diagnostics { assert!(d.span.start <= d.span.end && d.span.end <= data.len()); }
    let result = ifx_lang::analyze(data);
    assert!(result.diagnostics.len() <= 100);
    for diagnostic in result.diagnostics {
        assert!(diagnostic.span.start <= diagnostic.span.end);
        assert!(diagnostic.span.end <= data.len());
    }
});
