# SHA256 Circuit with lookup table

This circuit use a forking of [table16](https://zcash.github.io/halo2/design/gadgets/sha256/table16.html) in `halo2-gadget`, with some patches:

+ Make all code generic for the `Field` trait so that it also work with the `bn254` curve
+ Fix the digest exporting part, output correct digest (the final state ⊕ init state) with correct constraint (rows for 512-bit block increased from **2102** -> **2114**)

The witness in table16 is then exported to an extra region so that the RLC of input and digest can be calculated and form the lookup table for the SHA256 precompile in zkevm-circuit. To achieve this, we have introduced several cols and assigned them to two regions: `input` and `digest`. The following table illustrates:

input region (example for input 'abc'):
|          | s_final_block | s_u16     | byte_counter| bytes_rlc | trans_byte | copied_data | s_output|  s_padding      |helper |
|----------|------------------|-----------|-----------|-----------|------------|-------------|---------|-----------------|------------|
|(inherit) |     *1*          |           |  *42*     |*inherit_rlc*|          |             |         |       *1*       |            |
|s_begin   |     1            |           |     0     |     0     |            |             |         |       0         |            |
|s_enable  |     1            |    1      |     1     |  0x61     |   b'0x61'  |  *0x6162*   |         |       0         |            |
|s_enable  |     1            |    0      |     2     |  0x61062  |   b'0x62'  |             |         |       0         |            |
|s_enable  |     1            |    1      |     3     | 0x61062063|   b'0x63'  |  *0x6380*   |         |       0         |            |
|s_enable  |     1            |    0      |     3     | 0x61062063|   b'0x80'  |             |         |       1         |            |
|....      |
|s_enable  |     1            |    1      |     3     | 0x61062063|   b'0x00   |  *0x0018*   |         |       1         |    0       |
|s_final   |     1            |    0      |     3     | 0x61062063|   b'0x18   |             |         |       1         |    24      |


digest region (example for the hash of 'abc'):
|          | s_final_block | s_u16     | byte_counter| bytes_rlc | trans_byte | copied_data | s_output|s_padding  |helper |
|----------|------------------|-----------|-----------|-----------|------------|-------------|---------|-----------|-----------|
|          |     *1*          |           |           | **0**     |            |             |         |    **0**  |
|s_enable  |      1           |    1      |           |  0xba     |   b'0xba'  | *0xba78*    |  0x6a09 |    0      |
|s_enable  |      1           |    0      |           | 0xba078   |   b'0x78   | *0x6a09*    |         |
|....      |
|s_enable  |      1           |    1      |           |           |   b'0x15   | *0x15ad*    |  0xcd19 |    0      |
|s_enable  |      1           |    0      |           | hash_rlc  |   b'0xad   | *0xcd19*    |         |    **0**  |
|s_final    |                  |           |*input_counter*|*hash_rlc*|         |          |    1    |           | *input_rlc* |

Note: 
+ *Italic* indicate the cell is equality constrained whie **bold** indicate the cell is constarinted with constant
+ We suppose the `random` value for rlc is `0x1000`

### Definition of the cols

+ `copied_data` col is used to copy the cells with `u16` values from `table16`.
+ `trans_byte` expands each `u16` value copied from `table16` into two bytes across two adjacent rows, with the help of the selector `s_u16`
+ `s_padding` col marks whether the byte in current row is padding or input byte.
+ `bytes_rlc` accumulates bytes in `trans_byte` col to its RLC expression only if the byte in current row is not padding. Otherwise, it continues the value from the previous row if the current row is marked as padding.
+ `byte_counter` counts the total input bytes if byte in current row is not padding, Otherwise it continues the value from previous row if the current row is marked as padding.
+ `s_final_block` is a boolean advice col that identifies in each row of an input region, marking whether the current block is the last block
+ `helper` col has multiple usage. In input region it calculate the accumulation of the last 8 bytes and represent the bit counts in the last row (if current block is the final one); in output region it copied the rlc of input bytes in the final row.

The circuit contains fixed layouts of input and output regions, i.e. an input region is followed by a output region, handling one sha256 block (512 bits or 64 bytes), and the whole circuit would handle the fixed count of blocks. So it is possible to put fixed selector cols in the circuit:
+ `s_begin` is used to indicate the first row in both input and output region.
+ `s_enable` is used to indicate each row in regions the common gates must be activated
+ `s_final` is used to indicate the last row in both input and output region.
+ `s_assigned_u16` is used to indicate there is a copied u16 word in the cell of `copied_data` col.
+ `s_padding_size` and `s_common_bytes` is a pair of fixed selector to mark the the last 8 bytes in every input region. `s_padding_size` select the last 8 bytes while `s_common_bytes` select the other. They help to accumulate the last 8 bytes in input region and obtains the bit counts recorded in the tail of the padding, which is specified by SHA2.

### Definition in regions:

  The 256-bit state to start a new compression is stored in 16 cells extracted from the digest region (see below), each cell for a 16-bit part and is called a "dense state" in table16. Such a dense state is assigned into an initialization region of table16 and export a "working state" for the following compression step. The working state also contain another dense state inside it. The caller to this initialization entry of table16, that is, the sha256 circuit, has a response to constraint the input dense state with the output one.
  
  Also, for the first compression step, circuit also constraint the dense state inside of the working state with the constants of the "Initiazation Vector" of sha256.

  Each input region captures a 512-bit block and copies the 16 x 32-bit integers (in the form of a pair of assigned cells for their lo and hi 16-bit parts) inside of the `message schedule` region of table16. The region consists of 66 rows: 64 rows for 64 bytes representing the 512-bit block and 2 extra rows at the beginning. For the `s_final_block`, `byte_counter`, `s_padding` and `bytes_rlc` cols, the cells in last row (enabled by `s_last` selector) are connected by equality constraints to the first row of next input region for the subsequent 512-bit block. Additionally the `s_final_block` cells is also connected with the corresponding digest reion. 
  
  The second row at the top of the region determines how the `byte_counter`, `s_padding` and `bytes_rlc` cols begin: if the inherited `s_final_block` cell (at the first row at the top of the region) is 1, these cols will start with an initial value (i.e., 0); else they will start with the "inherited" value of the previous 512-bit block. 

  Note that it is free to specify `s_final_block` in each block as either 0 or 1. If `s_final_block` is set to 1, the last row must satisfy the "final" constraint, that is the cell in `byte_counter` col has to equal the calculated bit size in `s_padding_size` cell.

  There is exactly one digest region corresponding to each input region. This region captures the 256-bit digest of the 512-bit block and copies it from the `digest` region of table16. The region consists of 34 rows: 32 rows for bytes of digests, 1 extra row at the beginning, and 1 row at the bottom. The `s_final_block` is inherited from the input region, and the first row for `byte_counter`, `s_padding` and `bytes_rlc` cols are specified with 0 by constraints to a constant. The last row for digest bytes is also constraint the `s_padding` cell as 0, which also ensure there is no padding row existed in digest region.

  Like input region, digest region calculated the RLC of digest bytes. The final row in digest copied `s_final_block` and `byte_counter` value inherited from input region into the corresponding cols; `bytes_rlc` of the cell in previous cell (i.e. the RLC of digest); and the RLC of input into `helper` col. This row represents a row in SHA256 table used for looking up from evm circuit.

## Performance

  Currently the SHA256 circuit can calculate SHA256 for 1k bytes within 4.891s (`k=17`), ~26% overhead to its `table16` core (3.854s), and verification is 6.601ms, 6% overhead to `table16` (6.207ms).

  We have a [detailed performance for table16 and Brecht's sha256](https://www.notion.so/scrollzkp/Precompile-SHA256-7a0f519d5bbe4f52a9fa08ebff9a8118) (accessing privilege required).

  With `k=21`, SHA256-circuit can calculate the hashes for as much as 16KB bytes, which should be enough for the txs in mainnet.

## Known issue in table16

+ Initialize state is not constrained in table16
