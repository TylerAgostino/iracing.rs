# Telemetry Pipeline

## Layers

The SDK separates acquisition, transport-neutral frames, delivery/session
policy, and consumer adaptation.

```text
source bytes
  -> reader or Windows connection
  -> Provider
  -> Telemetry::read_task
       ├─ DeliveryPolicy
       └─ SessionPolicy
  -> connection/subscriber
  -> FrameAdapter
```

This separation lets recorded files and live shared memory share frame and
adapter types without pretending that their timing and loss semantics are the
same.

## Wire and schema layer

`IbtReader` parses the fixed header, disk sub-header, variable headers, session
YAML region, and fixed-size frame records from `.ibt` data. `open` retains a
private read-only memory map plus decoded headers and layout, and reads one owned
frame on demand; `from_bytes` uses the same parser over an owned in-memory
cursor. `IbtLayout` owns metadata bounds, frame start/size/count, and indexed
frame geometry. Source I/O uses checked conversions between `u64` seek offsets
and `usize` layout coordinates. Unlike the earlier `u64` file navigation, this
rejects sources larger than `usize::MAX` bytes: files of 4 GiB or more cannot be
opened on 32-bit targets. Supporting those files would require a separately
scoped wider layout API. File-backed readers require completed, immutable
recordings: no process may modify or truncate the file until the reader (or
owning provider/connection) is dropped. Use `from_bytes` with an owned copy when
that lifetime requirement cannot be met.

`frame(index)`, `session_info_snapshot()`, and `variable_headers_snapshot()` read
owned data from the source on each call. They may move its physical cursor;
there is no reader-owned logical cursor, schema, or session cache. Every indexed
read seeks to its validated region. The Windows-only `LiveSource` owns mapped
shared-memory access and event waiting; `LiveReader` validates live geometry and
acquires owned snapshots.

`VariableSchema` maps names to `VariableInfo` and records the frame size. A
`VariableInfo` carries type, byte offset, element count, time-count marker,
units, and description. Schema construction is the boundary at which ranges
should be validated.

Telemetry is little-endian. `VarData::from_bytes` and `TelemetryValue::decode`
are the authoritative decoding paths; consumers should not reproduce byte
slicing or discriminant handling.

## `FramePacket`

Providers return `FramePacket`, the common data unit:

- owned, reference-counted frame bytes (`Arc<[u8]>`);
- monotonic source tick;
- session version;
- shared `VariableSchema`.

The packet can decode a named value directly. `DynamicFrame` wraps the same
bytes and schema for exploratory name-based lookup. Hot paths should implement
`FrameAdapter` so lookup and type checking happen once.

## Providers

`Provider` is an async, owned-source abstraction with three responsibilities:

- return the next frame or permanent EOF;
- return session YAML when available;
- report the source tick rate.

`IbtProvider::from_reader` validates the exact variable-header snapshot against
the layout's frame size and owns the resulting shared schema. Construction
starts its sequential cursor at frame zero, even after indexed reader operations.
Frames without variable metadata are rejected; zero-frame recordings may have
an empty schema. Successful reads advance one index, failed reads retain the
index for retry, and the layout's frame count determines permanent EOF. Packets
use the zero-based index cast to `u32` as their synthetic tick and retain the
header's session update counter. Tick rate comes from the header with a 60 Hz
fallback for nonpositive values. Session YAML is read from a fresh snapshot,
decoded and sanitized by the provider. Reads complete as fast as the file can be
decoded; the provider has no seek/time helper API.

`LiveProvider` is Windows-only and belongs to one activated session. Construction
requires connected, stable geometry and variable metadata, validates its schema,
and fails immediately otherwise. Callers may retry construction separately.
Disconnect polling limits and reader injection are no longer builder options;
`with_poll_interval` controls connected update-event waits only.

### Live acquisition and retirement

`LiveReader` validates source geometry at activation and retains its mapping.
Each active buffer has a bounded frame range and cached, aligned synchronization
offsets; checked arithmetic establishes the ends of frames and synchronization
words before unchecked source access is permitted. Frame copies use the cached
offset and validated frame size without constructing ranges or repeating bounds
and alignment checks. Before acquisition and after copying, the reader compares
immutable geometry words with their activated values. A structural change
permanently invalidates the reader; it does not rebuild the layout. Publication
fields (current buffer, ticks, status, and session revision) remain runtime state.
Acquisition still relies on finalized geometry staying fixed between observations;
these word reads are not an atomic header snapshot. An observed disconnect
permanently retires the reader, even if the source later reconnects. A
disconnect/reconnect with identical geometry entirely between observations is
undetectable.

Acquisition results are operation-specific:

- `variable_headers_snapshot` returns `Result<Option<VariableHeadersBuffer>>`.
  Absence is advertised zero variable metadata, which provider construction rejects.
- `frame_snapshot` returns `Result<Option<LiveFrameSnapshot>>`. `None` means an
  unpublished/unchanged tick or exhaustion of three publication-copy attempts.
  It never means EOF. The reader advances its accepted tick only on success.
- `session_info_snapshot` returns `Result<LiveSessionRead>`, whose success variants
  are `Snapshot`, `Absent`, and `Contended`. Unlike frame idleness, session
  contention must remain distinct from absence for the provider's retry policy.

Frame acquisition uses the published current buffer, copies owned bytes, and
checks its publication fields before accepting the frame. Tick metadata belongs
to that accepted copy; the separately observed session revision is not atomic
with frame bytes. `LiveProvider` waits cooperatively after an idle or contended
frame attempt, then tries acquisition again. `Provider::next_frame` returns a
packet or error; this live provider does not return temporary absence as EOF.

All acquisition methods and update waiting return `LiveDisconnected` for a
retired disconnected reader. Invalidating failures return `LiveInvalidated` on
the initial and every subsequent acquisition, retaining the original error as a
shared cause. These errors are non-retryable for the current provider. Recreate
`LiveConnection` and its subscriptions for a new session: the existing connection
retains schema, source frequency, and adapter validation from initial activation,
so replacing only a reader could invalidate consumers' field offsets.

### Live session update acquisition

The session YAML occupies one current region with an after-write revision
counter, not a history and not a seqlock. The reader retries a copy up to three
times when the revision changes across it. A writer that has started but not yet
incremented the counter remains invisible to that check.

`LiveSessionPolicy` discovers revisions through accepted frame packets and calls
`Provider::session_yaml` once per observed version. The provider fetches the
current session snapshot, decodes its declared encoding, and sanitizes it with
`IRacingSessionString` before handing owned YAML to the background parser. The
version argument is a change trigger, not a historical lookup key.

The provider makes at most three reader calls per session fetch, yielding between
contended calls. This permits nine total copy attempts. Advertised absence returns
`Ok(None)` immediately; exhausted contention returns a `Buffer` error rather than
claiming absence. The policy still marks the version observed after any outcome,
so it does not refetch that version after exhaustion. Retirement errors propagate
from session acquisition; the next frame acquisition also observes retirement.

Session-only events do not produce frame packets. Replaced intermediate YAML
cannot be reconstructed, and a fetched revision can differ from the frame's
trigger revision. These limits preserve the current observation-based contract:
copy current YAML promptly and preserve FIFO order of successfully owned snapshots,
without claiming that every simulator session revision can be recovered.

Once an owned YAML snapshot has been captured, parsing does not need to be
concurrent. A single background FIFO parser can keep typed deserialization off
the frame task while naturally preserving observation order. Any observer-facing
event stream must preserve the same FIFO property; a latest-value channel may
remain useful for `current_session`, but it cannot by itself represent a
lossless sequence of session changes.

## Telemetry task

`Telemetry::read_task` owns a provider. It initializes the session policy, then
repeats:

1. acquire one delivery permit;
2. call `Provider::next_frame` with cancellation selection;
3. let the session policy observe a successful packet;
4. deliver the packet through the delivery policy.

Non-retryable provider errors stop the task immediately. Retryable errors use
exponential backoff and stop after ten consecutive errors. Terminal errors clear
live state and are returned as errors, rather than EOF, to on-demand consumers.
Dropping a high-level connection cancels its task through a
`CancellationToken`. The task finalizes its session policy exactly once on every
exit, including cancellation, provider EOF, terminal errors, and dropped frame
receivers.

The internal `TelemetryBuilder` makes delivery and session policies independent.
Its defaults are `LatestDelivery` and `LiveSessionPolicy`.

## Delivery policies

`LatestDelivery` stores `Option<Arc<FramePacket>>` in a Tokio watch channel.
Each frame replaces the previous snapshot. This is correct for live state:
consumers care about the newest value and may intentionally miss intermediate
ticks.

`OnDemandDelivery` uses an mpsc request queue and one-shot responses. One demand
authorizes exactly one provider read. `Telemetry::spawn_ibt` selects this policy
and returns its request handle to `IbtConnection`.

`IbtConnection` places a coordinated watch bridge above that request handle.
The connection starts explicitly, maintains one shared IBT cursor, and publishes
one retained frame to every active subscription. A subscription acknowledges its
current frame when it is polled for the next item. The bridge sends another
demand only after every active subscription has acknowledged the retained frame.
Dropping the final subscription parks the cursor without closing the connection;
a later subscriber receives the retained frame and can resume replay.

## Session policies

`LiveSessionPolicy` watches packet session versions. On a changed version it
fetches and owns the current YAML immediately, then submits the snapshot to a
single background FIFO parser so typed YAML deserialization does not block the
frame loop. The current semantics are:

- a version is marked observed even if fetch or parse fails;
- repeated adjacent frames with the same version do not refetch YAML;
- owned snapshots are parsed and published one at a time in observation order;
- a parse failure is logged and does not prevent a later queued snapshot from
  being parsed;
- `end` closes the task queue, drains every queued parse, and only then
  publishes `None`.

Live session publication still uses a watch channel. FIFO parsing determines
send order, but the channel retains only the latest value and can coalesce
updates that an observer does not receive promptly. Lossless observer delivery
is a separate policy concern.

`IbtSessionPolicy` fetches the file's single immutable YAML document once during
initialization, parses inline, and publishes before frames. It does not retry a
missing, failed, or malformed session, and it retains successful metadata after
EOF.

These behaviors are explicit policies because live-changing state and immutable
recording metadata have different lifecycle requirements.

## Connections

`IbtConnection` and `LiveConnection` are convenience facades that:

- build or accept a provider;
- spawn the telemetry task;
- expose typed frame subscriptions and session update streams;
- retain current frame/session snapshots;
- cancel background work on drop.

`LiveConnection` normalizes `UpdateRate` against source frequency and applies
latest-wins throttling. `IbtConnection` does not accept an update rate: recorded
delivery is paced by its coordinated subscriber acknowledgement barrier.

`LiveConnection` has a portable non-Windows stub whose constructor returns an
unsupported-platform error. The actual fields and subscription methods exist
only on Windows.

Both connection subscription methods currently panic if adapter schema
validation fails. Treat this as an existing public-API limitation, not a pattern
to copy into new fallible boundaries.

## Adapter pattern

`FrameAdapter` has a deliberate two-phase contract:

1. `validate_schema` maps requested fields to `VariableInfo` and returns
   `AdapterValidation`;
2. `adapt` decodes each `FramePacket` using that precomputed plan.

`FieldExtraction` represents required, optional, defaulted, calculated, and
skipped strategies. The derive crate generates this plan from
`IRacingTelemetryFrame` attributes.

Invariants:

- required schema mismatches fail during validation;
- per-frame adaptation should avoid schema hash-map lookup;
- decoding goes through `VarData`;
- `DynamicFrame` is for flexibility, not the default hot-path design.

## Rate limiting

For live telemetry, `UpdateRate::Native` forwards source cadence and
`UpdateRate::Max(hz)` applies the custom `ThrottleExt` stream after frame
delivery. Throttling and delivery loss are separate concerns: a live source can
already have dropped frames before a subscriber-level throttle runs.
Coordinated IBT subscriptions do not use this throttle.
