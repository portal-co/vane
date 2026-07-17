# JIT host-method name mappings

Vane JIT output accesses a fixed `$` runtime object. `vane-arch` can resolve
those accesses through a compile-time name scheme so a host whose `$` members
were property-mangled can execute generated code without preserving Vane's
canonical property names.

## ABI inventory

`VaneHostMethod`, owned by `vane-arch`, describes the configurable `$` members:

- register state and cache: `r`, `_r`;
- JIT cache and wrapper helper: `p`, `f`;
- memory/page services: `get_page`, `_sys`;
- execution and asynchronous memory services: `ecall`, `reg_read`,
  `reg_write`, `mem_read`, `mem_write`.

Only this host ABI is configurable. Generated local temporaries, the `$` and
`J` bindings themselves, JavaScript globals, and platform methods such as
`DataView#getUint32` are not part of the mapping.

## Default output

Existing `render_stack_to_js` and `CoreJS` callers retain canonical behavior:
the zero-sized `CanonicalHostMethodNames` resolver is used internally and
source keeps direct accesses such as `$.r`, `$.ecall()`, and `$.get_page(...)`.

## Custom output

Use `render_stack_to_js_with_names` for a stack-op stream, or attach a resolver
to a wrapper with `CoreJS::with_names`:

```rust
use portal_jit_host_names::MappedHostMethodNames;
use vane_arch::{CoreJS, VaneHostMethod};

let names = MappedHostMethodNames::new([
    ("r".into(), "a".into()),
    ("_r".into(), "b".into()),
    ("p".into(), "c".into()),
    ("f".into(), "d".into()),
    ("get_page".into(), "e".into()),
    ("_sys".into(), "f".into()),
    ("ecall".into(), "g".into()),
    ("reg_read".into(), "h".into()),
    ("reg_write".into(), "i".into()),
    ("mem_read".into(), "j".into()),
    ("mem_write".into(), "k".into()),
]);

let named_wrapper = CoreJS { content: &body, flate: &flate, flags }.with_names(names);
# let _ = named_wrapper;
```

The resolver is checked against the complete inventory before source
production. Missing entries are an error; there is no per-operation fallback.
A non-identifier mapped key is safely rendered as a computed access, e.g.
`$["not-a-name"]`.

The selected mapping must also be applied to the JavaScript host's `$` object.
Resolution occurs while text is emitted, so generated code contains direct or
computed property expressions only—no map lookup or naming abstraction on its
execution path.