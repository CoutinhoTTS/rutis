# rutis Instance and Subtree Work: Three GitHub Issues

Status: published; implementation is on the `feat/instance-subtrees` branch.

The unified design is [Instance Keys, Instance Events, and Permanent Subtree Shutdown](../../design-instance-subtrees.en.md). The requirements came from dim-agent !2001, but this document tracks only rutis framework capabilities.

| Order | Issue | Requirement | Dependency |
| --- | --- | --- | --- |
| 1 | [#22 Instance service keys and subtree visibility](https://github.com/arcships/rutis/issues/22) | R1, minimal diagnostics and shared admission facility | No other product dependency |
| 2 | [#23 Instance events and in-flight dispatch ownership](https://github.com/arcships/rutis/issues/23) | R2 | #22 |
| 3 | [#24 Permanent subtree shutdown and complete reclamation](https://github.com/arcships/rutis/issues/24) | R4 + R3 | #22, #23 |

No runtime migration or product load-test issues are planned. The 1,000-cycle create/shutdown test is a framework regression test for the third issue.

## Decisions already made in the drafts

- `TypeKey` keeps `Clone` and dynamic qualified names; getters use `instance_id`; `with_instance` can add an instance identity to an existing key.
- A fiber gets a unique process-local ID on creation. Reloads preserve it; recreations do not reuse it. An old `Ctx` can read its old ID but cannot register again.
- Instance event sending and registration obey subtree visibility and shutdown admission. Shutdown waits for related dispatches already admitted.
- Subtree `shutdown` returns only after its driver, dispatches, and internal reclamation finish. Internal completion and public completion are separate to prevent parent/child waits on each other.
- Cleanup takes ownership of resources acquired by an in-flight `apply` during shutdown.
- Errors are collected through ownership. Failed subtrees that shut down independently and detach are not retained forever by the parent.
- Do not claim that omitting an instance ID from a service will fail to compile; callers own business-scope type policies.

Existing [#13 diagnostics](https://github.com/arcships/rutis/issues/13), [#9 root shutdown](https://github.com/arcships/rutis/issues/9), [#11 historical error consumption](https://github.com/arcships/rutis/issues/11), and [#12 disposal wait deadlines](https://github.com/arcships/rutis/issues/12) are related only; this draft does not change their remote status.
