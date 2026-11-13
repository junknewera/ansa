#![forbid(unsafe_code)]

#[test]
fn scaffold_does_not_claim_to_execute_runtime_commands() {
    for args in [
        vec![],
        vec!["a prompt"],
        vec!["--login"],
        vec!["--offline-demo"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ansa-rs"))
            .args(args)
            .output()
            .expect("run scaffold executable");
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .expect("UTF-8 diagnostic")
                .contains("Rust CLI is not implemented")
        );
    }
}
