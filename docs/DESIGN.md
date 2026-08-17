varchar key data, why it does less serialization than the base DefaultHashMap<StringRef,
...> path, and what would happen if it avoided serialization by storing pointers into
source batches.

## Base hash map design

The base variable-key path uses DefaultHashMap<StringRef, ...>. For multiple varchar
or otherwise variable-width keys, the logical key tuple is serialized into one contiguous
byte range:


text
key = [serialized col0][serialized col1][serialized col2]...


For varchar, the serialized field format is self-delimiting:


text
[rowLenSize: 1 byte][length: rowLenSize bytes][payload: length bytes]


Null is represented as:


text
[0]


This is needed because plain byte concatenation is ambiguous:


text
("ab", "c") -> "abc"
("a", "bc") -> "abc"


Length metadata and null markers make the byte stream a correct representation of the
logical key tuple. The hash map can then hash and compare one StringRef.

The cost is that every candidate row must materialize this transient serialized key before
lookup/insertion.

## Taper hash table design

The taper path uses a different split:


cpp
using HashTable = TaperFlatHashTable<int64_t, true>;


The table key is a precomputed 64-bit hash, not the serialized composite key. The true
template argument means the key is already scattered/pre-hashed, so the table uses the
incoming int64_t hash directly.

The hot path is roughly:


text
1. Decode group-by input columns once per batch.
2. Compute per-row hash values from decoded columns.
3. Insert/find by 64-bit hash in TaperFlatHashTable.
4. On a matching hash/tag, compare actual columns against the stored RowContainer row.
5. For new groups only, store/copy the key columns into RowContainer-owned memory.


This means taper avoids building a full serialized composite key for every incoming row.
It pays column hashing cost for every row, equality cost only for candidate matches or
collisions, and varchar copy/serialization cost only when a new group is created.

## Multiple varchar keys in taper

For multiple varchar grouping columns, taper still needs a stable stored representation
for group keys. It stores all varchar payloads for a new group in one contiguous arena
block:


text
[varchar col0 data][varchar col1 data]...[varchar colN data]


Each varchar field still uses the same self-delimiting format:


text
[rowLenSize][length][payload]


Null varchar fields use a one-byte null marker:


text
[0]


The RowContainer keeps per-column null bits, and the first varchar column slot stores the
pointer to the merged varchar block. Non-first varchar columns in the merged set do not
store separate pointers.

During comparison, taper does not deserialize the whole key into another object. It walks
the stored row/merged varchar block and compares each stored varchar field directly with
the decoded input string_view.

## Why taper still serializes/copies varchar data

The hash aggregation table must outlive the input VectorBatch currently being processed.
If a group key includes varchar data, the table needs stable bytes for that key after the
source batch is gone.

Copying the varchar payload into the aggregation arena gives the group key table-owned
lifetime:


text
stored group row -> arena-owned varchar bytes


Without that copy, a stored group row would point into temporary input-vector memory:


text
stored group row -> input VectorBatch varchar bytes


That is unsafe unless the engine pins all source batches for the whole aggregation.

## What if taper did no varchar serialization/copying?

If taper stored only pointers into input batches, key correctness would depend on those
input batches remaining alive and unchanged for the whole aggregation. If they are freed,
reused, spilled, compacted, or released by upstream operators, the hash table would hold
dangling or stale pointers.

Likely consequences:

- Wrong grouping if a stored key's memory is later reused for different bytes.
- Missed matches when a later row should match an old group but the old pointer no longer
  points at the original key.
- False matches if reused memory happens to contain equal bytes.
- Crashes from reading freed memory during equality comparison or output.
- Nondeterministic behavior depending on allocator reuse, batch timing, spill, and vector
  encoding.

## Cost of pinning all source batches

Pinning all source batches would make the pointer-only design safe from a lifetime
perspective, but it changes the memory model badly.

Normal taper aggregation retains memory proportional to:


text
distinct groups + aggregate states + copied key bytes for distinct groups


Pinning source batches retains memory proportional to:


text
all input rows + all input varchar bytes + aggregate states


This is especially bad when many input rows collapse into few groups:


text
1,000,000,000 input rows
100 distinct varchar groups


With the current design, only about 100 varchar group keys need stable storage. With
pinning, all varchar input data for one billion rows may need to stay alive.

Other costs of pinning:

- Much higher peak memory.
- Worse spill behavior, because pinned input batches are not compact aggregate state.
- More allocator and memory-accounting pressure.
- More complicated ownership rules for flat, dictionary, constant, sliced, and large-string
  vector encodings.
- Worse cache locality during key comparison, because stored group keys point back into old
  batch memory instead of compact RowContainer-owned storage.
- Greater risk of leaks or delayed release on cancellation/error paths.

## Summary

Taper does not remove serialization entirely. It moves serialization/copying from the
per-row transient lookup path to the new-group storage path.

That is the key optimization:


text
Base StringRef path:
  serialize every candidate key, then hash/compare the serialized blob

Taper path:
  hash decoded columns for every row,
  compare actual columns only on candidate matches/collisions,
  copy/serialize varchar bytes only for newly created groups


This preserves correctness without retaining all input batches, while cutting much of the
CPU spent on transient composite-key serialization.
