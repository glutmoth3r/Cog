# Foreign memory

The first native interface copies scalars across the C boundary. Native
Cog computation can still allocate arrays, records, closures, and BigInts.
Those values remain managed by Cog. A native frame holds temporary roots
for the duration of execution and the VM restores live values before
continuing at a fallback point. The C interface exposes no pointer to
their representation.

The generated C header contains one opaque pointer type,
`cog_native_context *`. It denotes the context returned from the same
library instance's `cog_native_open()`. The owning thread may reuse it
until one successful call to `cog_native_close()`. Copying its C pointer
copies an alias and does not create a second owner. After close all copies
are invalid. The C host must serialize close with its callers and pass a
valid, writable, aligned result pointer for the duration of an export
call. A null result pointer is rejected. The runtime never stores the
result pointer. The shared object stays mapped through process exit so
that runtime thread state can finish safely.

Future foreign pointers and C layout values need their own type
representation and ABI classification. The implementation must keep C
null separate from Cog `nil` and treat C `void` as an ABI marker.
Owned foreign resources need one shared, invalidatable descriptor with an
explicit close operation, destructor identity, and lifetime of its
containing library. Borrowed views need the storage to remain live for the
whole borrow. A source lifetime assertion cannot extend the lifetime of C
storage. Callbacks and async retention of borrowed views require separate
contracts. This interface currently exposes only scalar values and
runtime context lifecycle.
