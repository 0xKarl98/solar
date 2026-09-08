use super::super::{
    ImportPathAt, import_path_at, import_path_at_for_completion, parse_import_path,
    plain_string_at, recover_unterminated_import_path,
};

#[test]
fn completion_recovers_an_unterminated_import_before_an_unrelated_string() {
    let source = "import \"./Dep\ncontract Main { string value = \"ordinary\"; }";
    let cursor = source.find('\n').unwrap();

    assert!(import_path_at(source, cursor).is_none());
    let import = import_path_at_for_completion(source, cursor).unwrap();

    assert_eq!(import.raw_path, "./Dep");
    assert_eq!(import.content_range, 8..13);
    assert_eq!(import.delimiter, b'"');
}

#[test]
fn completion_recovers_a_single_quoted_named_import() {
    let source = "import { Dependency } from './Dep";
    let cursor = source.len();
    let import = import_path_at_for_completion(source, cursor).unwrap();

    assert_eq!(import.raw_path, "./Dep");
    assert_eq!(&source[import.content_range], "./Dep");
    assert_eq!(import.delimiter, b'\'');
}

#[test]
fn completion_does_not_recover_an_unterminated_ordinary_string() {
    let source = "contract Main { string value = \"./Dep";

    assert!(import_path_at_for_completion(source, source.len()).is_none());
}

#[test]
fn completion_does_not_recover_past_an_unescaped_line_break() {
    let source = "import \"./Dep\ncontract Main { string value = \"ordinary\"; }";
    let cursor = source.find("contract").unwrap() + "contract".len();

    assert!(import_path_at_for_completion(source, cursor).is_none());
}

#[test]
fn valid_import_paths_match_parser_at_every_boundary() {
    for source in [
        r#"import "./Dep.sol"; contract C { string s = "ordinary"; }"#,
        "import {Dep as Alias} from './Dep.sol'; // import './Fake.sol';",
        r#"/* import "./Fake.sol"; */ import "./Dep.sol" as Dep;"#,
        r#"import * as Dep from "./😀.sol";"#,
        r#"contract C { string s = unicode"./Dep.sol"; bytes s2 = hex"abcd"; }"#,
    ] {
        for cursor in (0..=source.len()).filter(|&cursor| source.is_char_boundary(cursor)) {
            let expected = parse_import_path(source, cursor);
            assert_eq!(import_path_at(source, cursor), expected, "cursor {cursor} in {source}");
            assert_eq!(
                import_path_at_for_completion(source, cursor),
                expected,
                "cursor {cursor} in {source}",
            );
        }
    }
}

#[test]
fn definition_matches_parser_import_ranges_at_every_boundary() {
    for source in [r#"import "./Dep" "suffix";"#, r#"import "./Dep"#] {
        for cursor in (0..=source.len()).filter(|&cursor| source.is_char_boundary(cursor)) {
            assert_eq!(
                import_path_at(source, cursor),
                parse_import_path(source, cursor),
                "cursor {cursor} in {source}"
            );
        }
    }
}

#[test]
fn completion_fast_path_matches_unfiltered_path_at_every_boundary() {
    for source in [
        "",
        "contract C { uint value; }\n",
        "import \"./Dep.sol\";\ncontract C {}",
        "import './Dep.sol';\r\ncontract C {}",
        "import {Dep as Alias} from './Dep",
        "import \"./Dep\ncontract C { string s = \"ordinary\"; }",
        "import \"./Dep\r\ncontract C {}",
        "import \"./Dep\rcontract C {}",
        "import \"./Dep\n",
        "import \"./Dep\r\n",
        "import \"./Dep\r",
        "import \"./Dep\\",
        r#"import "./\"Dep.sol";"#,
        r#"import './\'Dep.sol';"#,
        r#"import "./Dep" "suffix";"#,
        "/* 😀 */ import \"./中😀.sol\";\r\ncontract C {}",
        "// import \"./Fake.sol\";\ncontract C {}",
        "/* import \"./Fake.sol\";\n still a comment */\ncontract C {}",
        "/* import \"./Fake.sol\";\n still an unterminated comment",
        "contract C { string s = \"./ordinary\\\ncontinued\"; }",
        "contract C { string s = unicode\"😀\"; bytes b = hex\"abcd\"; }",
    ] {
        check_completion_matches_unfiltered_path(source);
    }
}

#[test]
fn completion_fast_path_preserves_line_continuations_and_recovery() {
    for delimiter in ['\'', '"'] {
        for line_break in ["\n", "\r\n", "\r"] {
            for backslashes in 0..=3 {
                let prefix = format!(
                    "import {{Dep}} from {delimiter}./{}{line_break}",
                    "\\".repeat(backslashes),
                );
                for suffix in ["Dep".to_owned(), format!("Dep.sol{delimiter};\ncontract C {{}}")] {
                    check_completion_matches_unfiltered_path(&format!("{prefix}{suffix}"));
                }
            }
        }
    }
}

fn check_completion_matches_unfiltered_path(source: &str) {
    for cursor in source.char_indices().map(|(cursor, _)| cursor).chain([source.len()]) {
        assert_eq!(
            import_path_at_for_completion(source, cursor),
            unfiltered_import_path_at_for_completion(source, cursor),
            "cursor {cursor} in {source:?}",
        );
    }
}

/// The completion path before the fast rejection, retained as a behavioral oracle.
fn unfiltered_import_path_at_for_completion(source: &str, cursor: usize) -> Option<ImportPathAt> {
    if cursor > source.len() || !source.is_char_boundary(cursor) {
        return None;
    }

    let string = plain_string_at(source, cursor)?;
    if string.first_unescaped_line_break.is_some_and(|line_break| cursor > line_break) {
        return None;
    }
    if string.terminated && string.first_unescaped_line_break.is_none() {
        return parse_import_path(source, cursor);
    }
    if string.terminated && parse_import_path(source, cursor).is_some() {
        return None;
    }
    recover_unterminated_import_path(source, cursor, string)
}
