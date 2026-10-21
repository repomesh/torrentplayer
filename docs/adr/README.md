# Architecture Decision Records

An Architecture Decision Record (ADR) captures one significant decision: the context, the
decision, the alternatives considered, and the consequences. ADRs are never deleted; a decision
that changes gets a new ADR that supersedes the old one.

## Process

1. Copy [`template.md`](template.md) to `NNNN-short-title.md` using the next free number.
2. Open a pull request with status **Proposed**. Leave it open for comment for at least 7 days
   (see [GOVERNANCE.md](../../.github/GOVERNANCE.md)).
3. On merge, set the status to **Accepted**.

## Index

| ADR                                                | Title                                          | Status                                        |
| -------------------------------------------------- | ---------------------------------------------- | --------------------------------------------- |
| [0001](0001-project-license.md)                    | Project license                                | Accepted (pending libmpv/FFmpeg build review) |
| [0002](0002-workspace-layout.md)                   | Cargo workspace layout                         | Accepted                                      |
| [0003](0003-frontend-framework.md)                 | Frontend framework                             | Accepted                                      |
| [0004](0004-ipc-type-generation.md)                | Generating IPC types with ts-rs                | Accepted                                      |
| [0005](0005-prioritization-with-anchor-streams.md) | Prioritization on librqbit with anchor streams | Accepted for librqbit                         |
| [0006](0006-streaming-engine.md)                   | A streaming-first BitTorrent engine            | Proposed (M1 built)                           |
| [0007](0007-dolby-and-dts-audio.md)                | Dolby and DTS audio in the built-in player     | Proposed                                      |
| [0008](0008-application-wireguard-tunnel.md)       | Application-local WireGuard tunnel             | Proposed (implemented; amended by 0009)       |
| [0009](0009-remove-vpn-adapter-mode.md)            | Remove the VPN adapter mode                    | Proposed (implemented)                        |

Decisions still to be recorded: `stream_cb` vs HTTP feed (Phase 2), libmpv rendering approach
(Phase 2).
