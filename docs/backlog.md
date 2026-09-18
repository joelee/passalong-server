# Backlog

Future work not covered by an active plan. Completed items are removed.

## @joelee road map for next releases

### v0.1.0
- **Decide IDEA-00001**: answer its blocking questions, run the protocol
  spike of its §14, then accept, revise, or park it.
- **PLAN-00001**, once the idea is accepted: the server with parity for
  passalong client v0.2.1, its Docker image, and its systemd unit.
- **Licence terms or a private Docker Hub repository** before the first
  public image (IDEA-00001-R02-MED-03).

### Unscheduled
- An open-source licence.

## Agent suggested next steps

### Features

- **`THIRD-PARTY-NOTICES` in the image**, generated at build time, as soon
  as the first third-party dependency lands.
- **`workspace import` and `export`** for stores from the client's `ssh` and
  `local` backends; the on-disk files are byte-identical by design.
- **`itemsChanged` subscription**, so pull mode need not poll.
- **Server-side retention** per workspace by age and count, which works for
  encrypted workspaces too because ids carry creation time.
- **`passalong-server backup`**: a consistent copy while the server runs.
- **A send-only role**, for devices that contribute but must not read.
- **Remote administration** with an admin-scoped key.
- **A static musl build and a distroless image.**
- **Metrics endpoint.**
- **ACME.**

### Process

- **`just test-integration`, `test-client`, `test-deploy`, and `schema`**
  recipes, named in the `justfile`, arrive with the plans that need them.
- **macOS job in CI** is deliberately absent: the server supports Linux
  only. Revisit if developers need to build on macOS.
