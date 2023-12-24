use assert_fs::prelude::{FileTouch, FileWriteStr, PathChild};
use indoc::indoc;
use util::test_eelco;

#[test]
fn empty_file() {
    let (dir, mut eelco) = test_eelco("empty.md");
    let child = dir.child("empty.md");
    child.touch().unwrap();
    let assert = eelco.assert();

    assert
        .failure()
        .stderr("Error: could not find any REPL examples\n");
}

#[test]
fn example_fails_to_parse() {
    let (dir, mut eelco) = test_eelco("fails_to_parse.md");
    let child = dir.child("fails_to_parse.md");

    child
        .write_str(indoc! {"
            ```nix-repl
            nix-shnepl> nope
            dope
            ```
        "})
        .unwrap();

    eelco
        .assert()
        .failure()
        .stderr("Error: missing prompt LFLine(\"nix-shnepl> nope\\n\")\n");
}

#[test]
fn pass() {
    let (dir, mut eelco) = test_eelco("pass.md");
    let child = dir.child("pass.md");

    child
        .write_str(indoc! {"
            ```nix-repl
            nix-repl> 1 + 1

            2
            ```
        "})
        .unwrap();

    let child_path = child.path().to_str().unwrap();

    eelco
        .assert()
        .success()
        .stderr(format!("PASS: {child_path}:1\n"));
}
