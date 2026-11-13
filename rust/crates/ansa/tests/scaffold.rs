#![forbid(unsafe_code)]

#[test]
fn executable_exposes_local_help_and_a_real_offline_roundtrip() {
    for args in [vec!["--help"], vec!["--offline-demo"]] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ansa-rs"))
            .args(&args)
            .output()
            .expect("run scaffold executable");
        assert_eq!(output.status.code(), Some(0), "{:?}", output);
        let text = String::from_utf8(output.stdout).expect("UTF-8");
        assert!(text.contains(if args[0] == "--help" {
            "Usage: ansa"
        } else {
            "OMH_TOOL_OK:OMH_FIXED_VALUE_42"
        }));
        assert!(output.stderr.is_empty());
    }
}
