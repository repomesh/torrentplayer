# Governance

TorrentStream is a community project. This document describes how decisions are made.

## Roles

- **Users** use TorrentStream and give feedback through issues and discussions.
- **Contributors** submit issues, documentation, code, reviews, or designs. Anyone can be a
  contributor.
- **Maintainers** (listed in [MAINTAINERS.md](MAINTAINERS.md)) have write access. They review
  and merge pull requests, triage issues, cut releases, and handle security and Code of
  Conduct reports.

## Decision making

- Day-to-day changes are decided in pull request review. A PR needs passing CI and approval from
  one maintainer, or two for changes to `unsafe` code, the network and kill-switch logic, or the
  release pipeline.
- Significant technical decisions (architecture, new major dependencies, licensing, data formats)
  are proposed as an [Architecture Decision Record](docs/adr/) in a pull request and left open
  for comment for at least 7 days.
- Maintainers aim for consensus. If consensus cannot be reached, maintainers decide by simple
  majority vote.

## Becoming a maintainer

Contributors who have made sustained, high-quality contributions and reviews over several months
can be nominated by an existing maintainer. Nominations are approved by a majority of the
current maintainers. Maintainers who are inactive for 12 months may be moved to emeritus status.

## Changes to this document

Changes to governance require approval from a majority of maintainers.
