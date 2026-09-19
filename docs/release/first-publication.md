# Before the first publication

What only the repository's owner can do, because it needs accounts and
credentials. The agent does none of it and holds none of the credentials.
Until it is done, no workflow has ever run: what `.github/workflows/` says
has been linted, and its scripts have run on a developer's machine, and that
is all.

## Once

1. **Create `joelee/passalong-server` on GitHub.** The arm64 build uses the
   runner `ubuntu-22.04-arm`, which GitHub gives to **public** repositories
   free of charge. In a private repository that job waits for a runner that
   never comes, unless your plan has one; make the repository public first,
   or expect that.
2. **Read the history before making it public.** Everything since the first
   commit becomes public with it, including the time before `6b4633e`, when
   the `LICENSE` file said "proprietary and confidential". You hold the
   copyright and have relicensed it; the old notice stays in history, and
   that is ordinary. There is no secret in the history: API keys in tests
   are made up, and `tests/logs.rs` and the session tests check that none
   is written anywhere.
3. **Push `main`**: `git remote add origin git@github.com:joelee/passalong-server.git`,
   `git push -u origin main`. CI runs. It is the first time `just ci` runs
   anywhere but on one machine; expect it to find something. The most
   likely: the runner's Docker is older than 28, which `scripts/test-service.sh`
   needs and says.
4. **Create `joeworks/passalong-server` on Docker Hub**, public, or let the
   first push create it, which makes it public only if your account's
   default says so.
5. **A Docker Hub access token** (Account settings, Personal access tokens):
   read and write, and if your plan allows, limited to that one repository.
   Not your password.
6. **Two repository secrets** on GitHub (Settings, Secrets and variables,
   Actions): `DOCKERHUB_USERNAME` = `joeworks`, `DOCKERHUB_TOKEN` = the
   token. They are read by `release.yml` and by nothing else.

## Before every first of something: a dry run

**Actions, Release, Run workflow**, on `main`. It checks nothing about a
tag, runs `just ci`, builds both architectures, their images, and their
archives, verifies the checksums, and **publishes nothing**: no image is
pushed, no release is created. The archives are attached to the run for you
to look at. This is the first arm64 build there has ever been.

Do it before the first tag, and after any change to `release.yml`.

## Then, a release

As `AGENTS.md`, "Release workflow", says. In short: the agent finalises the
records in one commit (`CHANGELOG.md`, `docs/release/vX.Y.Z.md`, the
README's status), `scripts/check-release-tag.sh vX.Y.Z` passes, the pull
request is merged, and you tag the merge commit and push the tag.

**What is published stays published.** A pushed image and a release that
someone fetched cannot be taken back, and a version tag is never moved. A
bad release is answered by the next patch release. Deleting `latest` or a
tag on Docker Hub helps nobody who has pulled it.

## What waits for this

Of PLAN-00006's acceptance criteria, these are met as far as a machine
without GitHub can show, and proven only by the runs above: that CI runs on
every branch and pull request (AC-09); that the release workflow builds
both architectures and publishes (AC-10, AC-11). Everything else is
verified.
