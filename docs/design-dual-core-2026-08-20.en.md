# Dual-Core Architecture and Experience Hardened into Rust (rutis × dsh)

> Decision (2026-08-20): rutis is the Rust spine of the polyglot harness; final direction A (permanent dual core).
> **Principle: the TS side keeps changing; durable lessons proven over time are hardened into Rust.**
> TS is the lab; Rust receives graduates.
> Direction revised (2026-08-21): **run the full dsh stack on the TS side; Rust provides the foundation only** (model integration / event observation / services hardened one at a time). The agent loop has not converged, so **keep it in TS** and do not rush to harden it in Rust.

## Decision

1. **Permanent dual core:** Rust spine (five core pillars / aimux / single-binary distribution) × TS feature surface (full dsh stack: agent loop, services, plugins, prompt content, npm-bound clients). Design the bridge as permanent and evolve it as Rust hardens capabilities: with each hardened piece, shrink the TS side and remove one seam from the bridge. Each step should be self-contained and reversible.
2. **The bridge is a small set of seams:** v1 has two—an **LLM seam** (TS model calls go through aimux) and an **event seam** (Rust can observe TS events). Harden services such as sandbox/fs one at a time, adding seams as needed. Each bridged service doubles the protocol, so resist doing so. Detailed rules are in bridge design [v3](design-dsh-bridge-2026-08-21.md).
3. **`rutis-agent` is an independent Rust baseline:** driver + minimal tools + TUI, **zero coupling to dsh and no dependency on the bridge crate** (a sibling over the core). It serves as a fallback without Node, a control group for the bridge, and a seed that may grow into an engine after loop semantics converge. rutis will not build its own plugin ecosystem.
4. **Do not:** embed a JS engine (npm resolution would mean rebuilding Node inside Rust); build dsh's UI/product surface (client / ACP / API remotes stay TS forever); or duplicate its loop before semantics converge (dual implementations shift ongoing catch-up cost to us).

## When to harden experience into Rust

An experience graduates into Rust only when its semantics have converged, it is a mechanism rather than wording, it is sensitive to performance or distribution, it is not tied to the npm ecosystem, or it is on a trust boundary. Consequence: the **assembly mechanism** for prompts may be hardened; prompt **content** never is. A trust boundary such as sandbox is the strongest candidate.

## Next steps

1. M0 foundation experiment + two-seam bridge (LLM and events; run one full turn of the real dsh stack through aimux).
2. Harden sandbox/fs (the first hardened seam, replacing the TS service behind the bridge).
3. After dsh loop semantics converge, grow `rutis-agent` from a baseline into an engine, enabling the orchestration layer to swap the TS product onto that engine.

---

*The inventory and wave plan for dsh's 54 packages is a snapshot from 2026-08-20 and will drift with the ecosystem; it is not part of this decision. Re-inventory using the criteria when needed. Revision on 2026-08-21: the agent loop was removed from “already hardened.” The minimal Rust loop is a paradigm experiment and baseline, not a completed engine. The first service actually hardened is sandbox/fs.*
