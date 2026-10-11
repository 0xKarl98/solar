//@ revisions: strict recover
//@[recover] compile-flags: -Zrecover-incomplete-input

library Lib {
    enum Token { First }

    struct Record {
        uint256 amount;
    }

    struct Status {
        uint256 revert;
    }

    function make(uint256 amount) internal pure returns (Record memory) {
        return Record({amount: amount});
    }
}

contract C {
    Lib.Record[] private records;

    function declarations(Lib.Record[] calldata input) external pure returns (uint256) {
        Lib.Token token = Lib.Token.First;
        Lib.
            Token splitToken = token;
        Lib.Record memory value = Lib.Record({amount: 1});
        Lib.
            Record memory splitValue = value;
        Lib.
            Record[] memory values = new Lib.Record[](1);
        Lib.Record[] calldata borrowed = input;
        Lib.
            Record[2] memory pair;
        values[0] = splitValue;
        return values[0].amount + borrowed.length + uint256(splitToken) + pair.length;
    }

    function storageLocation() external view returns (uint256) {
        Lib.
            Record[] storage values = records;
        return values.length;
    }

    function f() external pure {}

    function functionAddresses() external view returns (address, address, uint256) {
        address direct = this.f.address;
        address split = this.f.
            address;
        return (direct, split, this.f.address.balance);
    }

    function memberChain() external pure returns (uint256) {
        return Lib.
            make({amount: 3}).
            amount;
    }

    function construction() external pure returns (uint256) {
        return Lib.
            Record({amount: 5}).
            amount;
    }

    function weakKeyword() external pure returns (uint256) {
        Lib.Status memory status;
        status.revert = 1;
        return status.
            revert;
    }
}
