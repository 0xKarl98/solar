//@ revisions: strict recover
//@[recover] compile-flags: -Zrecover-incomplete-input

// Type errors in later statements prove that recovery kept them in the AST.
contract Token {
    uint256 public balance;
}

contract C {
    Token[] tokens;
    event Seen(uint256 value);

    function builtin(uint256 i) external {
        tokens[i].
        uint8 next = 300; //~[strict] ERROR: expected one of
        //~[recover]^ ERROR: expected identifier
        //~[recover]| ERROR: expected one of
        //~[recover]| ERROR: mismatched types
    }

    function custom(uint256 i) external {
        tokens[i].
        Token next = 1; //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
        //~[recover]| ERROR: mismatched types
    }

    function eventStatement(uint256 i) external {
        tokens[i].
        emit Seen(true); //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
        //~[recover]| ERROR: mismatched types
    }

    function deleteStatement(uint256 i) external {
        tokens[i].
        delete tokens[i]; //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
        uint8 later = 300; //~[recover] ERROR: mismatched types
    }

    function assemblyStatement(uint256 i) external {
        tokens[i].
        assembly { let next := 1 } //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
        uint8 later = 300; //~[recover] ERROR: mismatched types
    }

    function blockStatement(uint256 i) external {
        tokens[i].
        if (true) { //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
            uint8 inside = 300; //~[recover] ERROR: mismatched types
        }
        inside; //~[recover] ERROR: unresolved symbol
    }

    function simplePath(Token token) external {
        token.
        uint8 next = 300; //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
        //~[recover]| ERROR: mismatched types
    }

    function callback() external {}

    function getFunction() internal view returns (function() external) {
        return this.callback;
    }

    function addressMember() external view {
        getFunction().address.
        Token next = 1; //~[recover] ERROR: expected identifier
        //~[recover]^ ERROR: expected one of
        //~[recover]| ERROR: mismatched types
        uint8 later = 300; //~[recover] ERROR: mismatched types
    }
}
