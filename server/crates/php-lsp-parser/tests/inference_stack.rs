use php_lsp_parser::{
    parser::FileParser, resolve::infer_variable_type_info_at_position,
    symbols::extract_file_symbols,
};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn deep_cst_inference_stays_within_a_small_stack() {
    const CHILD: &str = "PHP_LSP_INFERENCE_STACK_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let mut source =
                    String::from("<?php class Known{} function walk(Known $value,$flag){");
                source.push_str(&"if($flag){".repeat(512));
                source.push_str("echo 0;");
                source.push_str(&"}".repeat(512));
                source.push_str("/*USE*/$value;");
                source.push('}');
                let mut parser = FileParser::new();
                parser.parse_full(&source);
                let tree = parser.tree().unwrap();
                assert!(!tree.root_node().has_error());
                let symbols = extract_file_symbols(tree, &source, "file:///deep-inference.php");
                let references = php_lsp_parser::references::collect_symbol_references_in_file(
                    tree, &source, &symbols,
                );
                assert!(references
                    .iter()
                    .any(|reference| reference.is_declaration && reference.target_fqn == "Known"));
                let usage = source.find("/*USE*/").unwrap() + 7;
                let result = infer_variable_type_info_at_position(
                    tree,
                    &source,
                    &symbols,
                    0,
                    usage as u32,
                    "$value",
                );
                assert!(result.is_none_or(|ty| ty.to_string().trim_start_matches('\\') == "Known"));
            })
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("deep_cst_inference_stays_within_a_small_stack")
        .arg("--test-threads=1")
        .env(CHILD, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("deep inference exceeded watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "deep inference crashed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
