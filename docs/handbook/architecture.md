# Ownership without losing the shared process

Big separates three questions: who maintains a source, what Cargo links, and which
process owns running work. Separate Gits do not require a `.so` or process per app.

## Source and dependency direction

`big-framework` owns the reusable foundation, UI/app SDK and test support. Products
(`big-desktop`, `bigterminal`, `bigfiles`, `bigeditor`, `bigiris`, `bigshot`, `bigsearch`)
use it. `big-suite` owns host composition and cross-product contracts. The shared
file-operation service stays in the framework because both terminal and files use
it; the portal/file-picker adapter stays with BigFiles.

A product never depends on another product or the host. A lower layer does not
reach upward for a helper. Before adding an abstraction, name its real callers
and owner. One caller usually needs a local concrete function, not a new crate.
Cross-product assertions belong to the integration contract crate, not to a product
that reads its sibling's source through `include_str!`.

## Implemented runtime shape

The optional `builtin-session` feature links the shell and Workbench compositions
into `bighost`. Terminal, files, editor and images reuse one GTK application and
the linked `SessionRuntime`/`SessionResources` owner. They also retain standalone
entry points. The default dynamic wrappers `libbig_shell.so`/`libbig_workbench.so`
remain available; separate `.so` copies can still own separate Rust state.

There is no separate library required for every Git. Updating a product still
requires rebuilding/testing the integration that incorporates it. A source-level
boundary does not promise arbitrary binary mixing, fault isolation or independent
hot upgrades. Native libraries remain loaded while callbacks/GTypes can refer to them.

PTYs, durable file operations, search/indexing, decoders and the compositor keep
appropriate external boundaries. BigShot is not yet a hosted interface, and an
independent recorder service remains unfinished. Do not absorb these workers merely
to lower the process counter or describe the entire OS session as one PID.

## Contracts to preserve

Hosted code receives the application; it does not run another main loop or call
process exit to close one window. GTK stays on its main thread; asynchronous work
returns typed results tied to an owner/generation. Stop obsolete tasks, disconnect
signals/timers and release images after their final consumer. A cancelled worker
keeps its work permit until it actually terminates.

Pixel charges follow unique storage, not each Arc clone. CPU bytes and GPU estimates
are separate measurements, especially in unified memory. Admission cannot account
for all native decoder/driver allocations or prove a PSS ceiling. jemalloc does not
free still-referenced widgets. Measure retained objects, queues and resources as
well as process memory when investigating long-lived growth.

The external module ABI uses the existing versioned C descriptors, sizes, opaque
handles and producer-owned destruction. Do not export Rust String/Vec/trait object
representations. Normal Rust APIs are suitable inside a composition rebuilt together.
A panic guard is not a sandbox for SIGSEGV, abort or native memory corruption.

## Required is not the same as proven

Desktop UX/visual/accessibility contracts are required constraints even where a
feature remains unfinished. Source and tests establish what is implemented; current
run logs establish what was exercised. Roadmap is not an API, and a historical
screenshot or test report is not approval of a new candidate.

The [maintenance guide](maintenance.md) explains how to evolve these boundaries,
pin integration revisions and introduce independent repositories without copying
SDK code or promising unsupported binary compatibility.

## Shared-process identity and widget ownership

A module id, every product binary/alias and every window app-id must identify an
unambiguous product. Compiled modules reserve their identities against disk entries.
When two disk manifests collide, neither is exposed; sorting order must not choose
the application that receives a user's files. Explicit id lookup uses the same
policy as alias lookup. Distinct products within Workbench retain distinct app-ids.

GTK signal handlers are owned by their emitting objects. A child callback must not
own its parent or a model that owns that child. Use weak captures on these back
edges, while keeping one explicit strong owner for state needed by live controls.
A removed row's late signal must not mutate the new row at its former index. Test
both normal edits and destruction; replacing every capture with a weak reference
can also break working controls by dropping their only state owner.
