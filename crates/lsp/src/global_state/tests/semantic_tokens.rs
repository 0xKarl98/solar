use super::support::RequestFixture;
use lsp_types::{Position, Range};
use snapbox::str;

#[test]
fn classifies_every_advertised_token_type() {
    let fixture = RequestFixture::new(
        r#"
        //- /All.sol
        library MathLib {
            function plus(uint256 left, uint256 right) internal pure returns (uint256) {
                return left + right;
            }
        }

        interface IMarker {}
        struct Record { uint256 field; }
        enum Choice { First }
        type Amount is uint256;
        error Failure(uint256 code);
        event Notice(uint256 value);
        uint256 constant GLOBAL = 1;

        function freeFunction(uint256 input) pure returns (uint256) {
            return input;
        }

        contract Example is IMarker {
            uint256 stateValue;
            event Changed(string text);

            modifier only(uint256 limit) {
                _;
            }

            function run(uint256 param) public only(1) returns (uint256) {
                // semantic comment
                uint256 local = freeFunction(param);
                string memory text = "ok";
                Choice choice = Choice.First;
                emit Changed(text);
                stateValue = MathLib.plus(local, GLOBAL);
                require(choice == Choice.First);
                return stateValue;
            }
        }
        "#,
        "/All.sol",
    );

    let tokens = fixture.semantic_tokens("/All.sol");
    let mut token_types = tokens.iter().map(|token| token.token_type).collect::<Vec<_>>();
    token_types.sort_unstable();
    token_types.dedup();

    assert_eq!(token_types, (0..18).collect::<Vec<_>>());
    assert!(tokens.iter().all(|token| token.token_modifiers_bitset == 0));

    fixture.check_semantic_token_summary(
        "/All.sol",
        str![[r#"
NAMESPACE `MathLib`
TYPE `Amount` `Failure` `string` `uint256`
CLASS `Example`
ENUM `Choice`
INTERFACE `IMarker`
STRUCT `Record`
PARAMETER `code` `input` `left` `limit` `param` `right` `text` `value`
VARIABLE `GLOBAL` `choice` `local` `text`
PROPERTY `field` `stateValue`
ENUMMEMBER `First`
EVENT `Changed` `Notice`
FUNCTION `freeFunction` `require`
METHOD `only` `plus` `run`
KEYWORD `_` `constant` `contract` `emit` `enum` `error` `event` `function` `interface` `internal` `is` `library` `memory` `modifier` `public` `pure` `return` `returns` `struct` `type`
COMMENT `// semantic comment`
STRING `"ok"`
NUMBER `1`
OPERATOR `+` `=` `==`

"#]],
    );
}

#[test]
fn classifies_imports_from_resolved_exports() {
    let fixture = RequestFixture::new(
        r#"
        //- /Defs.sol
        library Lib {}
        contract ContractType {}
        interface I {}
        struct S { uint256 field; }
        enum E { A }
        type V is uint256;
        error Err();
        event Ev();
        function free() pure {}

        //- /Main.sol
        import "./Defs.sol" as All;
        import * as Star from "./Defs.sol";
        import {
            Lib as L,
            ContractType as C,
            I,
            S,
            E,
            V,
            Err,
            Ev,
            free as f
        } from "./Defs.sol";
        "#,
        "/Main.sol",
    );

    fixture.check_semantic_token_summary(
        "/Main.sol",
        str![[r#"
NAMESPACE `All` `L` `Lib` `Star`
TYPE `Err` `V`
CLASS `C` `ContractType`
ENUM `E`
INTERFACE `I`
STRUCT `S`
PARAMETER
VARIABLE
PROPERTY
ENUMMEMBER
EVENT `Ev`
FUNCTION `f` `free`
METHOD
KEYWORD `as` `from` `import`
COMMENT
STRING `"./Defs.sol"`
NUMBER
OPERATOR `*`

"#]],
    );
}

#[test]
fn classifies_named_syntax() {
    let fixture = RequestFixture::new(
        r#"
        //- /Defs.sol
        struct Payload { uint256 field; }
        contract Target {
            function callMe(uint256 amount) external payable {}
        }

        //- /Main.sol
        import {Payload, Target} from "./Defs.sol";
        contract C {
            mapping(address owner => uint256 balance) values;
            Target target;

            function run() external {
                Payload memory payload = Payload({field: 2});
                target.callMe{gas: 1, value: 0}({amount: 1});
            }
        }
        "#,
        "/Main.sol",
    );

    fixture.check_semantic_token_summary(
        "/Main.sol",
        str![[r#"
NAMESPACE
TYPE `address` `uint256`
CLASS `C` `Target`
ENUM
INTERFACE
STRUCT `Payload`
PARAMETER `amount` `balance` `owner`
VARIABLE `payload`
PROPERTY `field` `gas` `target` `value` `values`
ENUMMEMBER
EVENT
FUNCTION
METHOD `callMe` `run`
KEYWORD `contract` `external` `from` `function` `import` `mapping` `memory`
COMMENT
STRING `"./Defs.sol"`
NUMBER `0` `1` `2`
OPERATOR `:` `=` `=>`

"#]],
    );
}

#[test]
fn classifies_override_paths() {
    let fixture = RequestFixture::new_allowing_diagnostics(
        r#"
        //- /Override.sol
        contract C {
            function run() external override(UniqueBase) {}
        }
        "#,
        "/Override.sol",
    );

    fixture.check_semantic_token_summary(
        "/Override.sol",
        str![[r#"
NAMESPACE
TYPE
CLASS `C` `UniqueBase`
ENUM
INTERFACE
STRUCT
PARAMETER
VARIABLE
PROPERTY
ENUMMEMBER
EVENT
FUNCTION
METHOD `run`
KEYWORD `contract` `external` `function` `override`
COMMENT
STRING
NUMBER
OPERATOR

"#]],
    );
}

#[test]
fn keeps_only_reliable_lexical_tokens_when_parsing_fails() {
    let fixture = RequestFixture::new_allowing_diagnostics(
        r#"
        //- /Broken.sol
        contract {
            // still highlighted
            string memory value = "text";
            uint7 add = 42;
            42 + ;
        }
        "#,
        "/Broken.sol",
    );

    fixture.check_semantic_token_summary(
        "/Broken.sol",
        str![[r#"
NAMESPACE
TYPE `string`
CLASS
ENUM
INTERFACE
STRUCT
PARAMETER
VARIABLE
PROPERTY
ENUMMEMBER
EVENT
FUNCTION
METHOD
KEYWORD `contract` `memory`
COMMENT `// still highlighted`
STRING `"text"`
NUMBER `42`
OPERATOR `+` `=`

"#]],
    );
}

#[test]
fn uses_utf16_positions_and_splits_multiline_tokens() {
    let fixture = RequestFixture::new_allowing_diagnostics(
        "//- /Unicode.sol\r\n/* \u{1f600} */ contract C {\r\n    /* first\r\n       \u{1f600} second */\r\n    string value = unicode\"\u{1f600}\";\r\n}\r\n",
        "/Unicode.sol",
    );

    fixture.check_semantic_tokens(
        "/Unicode.sol",
        str![[r#"
0:0 8 COMMENT `/* 😀 */`
0:9 8 KEYWORD `contract`
0:18 1 CLASS `C`
1:4 8 COMMENT `/* first`
2:0 19 COMMENT `       😀 second */`
3:4 6 TYPE `string`
3:11 5 PROPERTY `value`
3:17 1 OPERATOR `=`
3:19 11 STRING `unicode"😀"`

"#]],
    );
}

#[test]
fn range_returns_whole_overlapping_tokens() {
    let fixture = RequestFixture::new(
        "//- /Range.sol\ncontract C {\n    /* first\n       second */\n}\n",
        "/Range.sol",
    );

    fixture.check_semantic_tokens_in_range(
        "/Range.sol",
        Range { start: Position::new(1, 6), end: Position::new(2, 2) },
        str![[r#"
1:4 8 COMMENT `/* first`
2:0 16 COMMENT `       second */`

"#]],
    );
    fixture.check_semantic_tokens_in_range(
        "/Range.sol",
        Range { start: Position::new(2, 2), end: Position::new(2, 2) },
        "",
    );
}

#[test]
fn classifies_builtin_modules_functions_and_members() {
    let fixture = RequestFixture::new(
        r#"
        //- /Builtins.sol
        contract Builtins {
            function run() external view {
                address sender = msg.sender;
                uint256 timestamp = block.timestamp;
                bytes memory encoded = abi.encode(sender);
                uint256 balance = address(this).balance;
                require(encoded.length + balance + timestamp > 0);
            }
        }
        "#,
        "/Builtins.sol",
    );

    fixture.check_semantic_token_summary(
        "/Builtins.sol",
        str![[r#"
NAMESPACE `abi` `block` `msg`
TYPE `address` `bytes` `uint256`
CLASS `Builtins` `this`
ENUM
INTERFACE
STRUCT
PARAMETER
VARIABLE `balance` `encoded` `sender` `timestamp`
PROPERTY `balance` `length` `sender` `timestamp`
ENUMMEMBER
EVENT
FUNCTION `require`
METHOD `encode` `run`
KEYWORD `contract` `external` `function` `memory` `view`
COMMENT
STRING
NUMBER `0`
OPERATOR `+` `=` `>`

"#]],
    );
}

#[test]
fn classifies_inline_yul_symbols() {
    let fixture = RequestFixture::new(
        r#"
        //- /Yul.sol
        contract Yul {
            uint256 value;

            function run(bytes calldata data) external returns (uint256 result) {
                assembly {
                    function twice(x) -> y {
                        y := add(x, x)
                    }
                    let local := twice(calldataload(data.offset))
                    sstore(value.slot, local)
                    result := local
                }
            }
        }
        "#,
        "/Yul.sol",
    );

    fixture.check_semantic_token_summary(
        "/Yul.sol",
        str![[r#"
NAMESPACE
TYPE `bytes` `uint256`
CLASS `Yul`
ENUM
INTERFACE
STRUCT
PARAMETER `data` `result` `x` `y`
VARIABLE `local`
PROPERTY `offset` `slot` `value`
ENUMMEMBER
EVENT
FUNCTION `add` `calldataload` `sstore` `twice`
METHOD `run`
KEYWORD `assembly` `calldata` `contract` `external` `function` `let` `returns`
COMMENT
STRING
NUMBER
OPERATOR `->` `:=`

"#]],
    );
}
